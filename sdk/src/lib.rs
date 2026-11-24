//! The main loop of an adapter.
//!
//! The daemon starts the adapter with a cleared environment, speaks first (`initialize`), then
//! sends `ping`, `event`, `command.invoke` and `shutdown`. This crate answers the protocol and
//! hands the interesting messages to a [`Service`]; it keeps nothing but the plumbing. Reading
//! never waits for the service: an event is handed over and the next line is read at once, a
//! relayed verb runs on its own task, so a slow verb or a slow event cannot make the adapter miss
//! a `ping`. A verb that owns the terminal runs the same loop over a session socket
//! ([`run_session`]).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use async_trait::async_trait;
use oxidepm_adapter_proto::limits::{Limits, SUPPORTED_PROTOCOLS};
use oxidepm_adapter_proto::messages::{
    method, CancelParams, EventParams, HostRequestParams, HostRequestResult, InitializeParams,
    InitializeResult, InvokeOutputParams, InvokeParams, InvokeResult, OutputStream, PlanParams,
    PlanResult, PrepareParams, PrepareResult,
};
use oxidepm_adapter_proto::{AsyncFrameReader, ErrorObject, Frame, Params, Read};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot, Notify};
use tokio::task::AbortHandle;

/// What an adapter implements.
#[async_trait]
pub trait Service: Send + Sync + 'static {
    /// An event of a family the manifest listed. Must return quickly (hand the work to a task).
    async fn event(self: Arc<Self>, event: EventParams);

    /// A relayed verb. Output for the person goes through `out`.
    async fn invoke(self: Arc<Self>, call: InvokeParams, out: Output) -> InvokeResult;

    /// The daemon is stopping the adapter: finish what is in flight, within a second or two.
    async fn shutdown(self: Arc<Self>);

    /// `runtime.prepare`: the steps the daemon is to run before the app starts. Only an adapter
    /// that provides a runtime answers it (see [`RuntimeProvider`]).
    async fn runtime_prepare(
        self: Arc<Self>,
        _params: PrepareParams,
    ) -> Result<PrepareResult, ErrorObject> {
        Err(ErrorObject::new(-32601, "this adapter provides no runtime"))
    }

    /// `runtime.plan`: what the daemon is to execute, once the steps have run.
    async fn runtime_plan(self: Arc<Self>, _params: PlanParams) -> Result<PlanResult, ErrorObject> {
        Err(ErrorObject::new(-32601, "this adapter provides no runtime"))
    }
}

/// Why a runtime adapter cannot make a launch plan for an app. The text is shown to the person
/// who started the app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal(pub String);

impl Refusal {
    pub fn new(text: impl Into<String>) -> Self {
        Refusal(text.into())
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Refusal {}

/// What a runtime adapter implements. It returns data and runs nothing: the daemon runs the steps
/// and executes the plan itself.
pub trait RuntimeProvider: Send + Sync + 'static {
    /// The steps to run before the app starts (a build), possibly none.
    fn prepare(&self, params: &PrepareParams) -> Result<PrepareResult, Refusal>;

    /// What to execute, given the results of the steps.
    fn plan(&self, params: &PlanParams) -> Result<PlanResult, Refusal>;
}

/// The [`Service`] of an adapter that only provides runtimes: it takes no events and has no verbs.
struct RuntimeService<R>(R);

#[async_trait]
impl<R: RuntimeProvider> Service for RuntimeService<R> {
    async fn event(self: Arc<Self>, _event: EventParams) {}

    async fn invoke(self: Arc<Self>, _call: InvokeParams, out: Output) -> InvokeResult {
        out.stderr("this adapter provides runtimes and no commands\n");
        InvokeResult {
            exit_code: 2,
            result: None,
        }
    }

    async fn shutdown(self: Arc<Self>) {}

    async fn runtime_prepare(
        self: Arc<Self>,
        params: PrepareParams,
    ) -> Result<PrepareResult, ErrorObject> {
        self.0
            .prepare(&params)
            .map_err(|e| ErrorObject::new(-32602, e.0))
    }

    async fn runtime_plan(self: Arc<Self>, params: PlanParams) -> Result<PlanResult, ErrorObject> {
        self.0
            .plan(&params)
            .map_err(|e| ErrorObject::new(-32602, e.0))
    }
}

/// Run an adapter that provides runtimes (`name` and `version` are those of the manifest).
pub async fn run_runtime<R: RuntimeProvider>(name: &str, version: &str, provider: R) -> Exit {
    run::<RuntimeService<R>, _>(name, version, move |_| RuntimeService(provider)).await
}

const OUTPUT_QUEUE: usize = 64;
const OUTPUT_TIMEOUT: Duration = Duration::from_secs(10);

struct Outbox {
    tx: mpsc::Sender<Vec<u8>>,
    invalid_frame: Notify,
}

impl Outbox {
    fn channel() -> (Arc<Self>, mpsc::Receiver<Vec<u8>>) {
        let (tx, rx) = mpsc::channel(OUTPUT_QUEUE);
        (
            Arc::new(Self {
                tx,
                invalid_frame: Notify::new(),
            }),
            rx,
        )
    }

    /// Wait for room for protocol replies and host requests. If a frame cannot be encoded,
    /// end the session so a missing reply cannot look like success.
    async fn send(&self, frame: Frame) -> Result<(), HostError> {
        let Ok(bytes) = frame.encode_limited(Limits::PROTOCOL_1.max_line) else {
            self.invalid_frame.notify_one();
            return Err(HostError::Gone);
        };
        self.tx.send(bytes).await.map_err(|_| HostError::Gone)
    }
}

/// Where the progress of a relayed verb is written. These calls return immediately. If progress
/// cannot be queued, the command's reply reports incomplete output.
#[derive(Clone)]
pub struct Output {
    id: u64,
    frames: Arc<Outbox>,
    dropped: Arc<AtomicBool>,
}

impl Output {
    pub fn stdout(&self, text: impl Into<String>) {
        self.send(OutputStream::Stdout, text.into());
    }

    pub fn stderr(&self, text: impl Into<String>) {
        self.send(OutputStream::Stderr, text.into());
    }

    fn send(&self, stream: OutputStream, text: String) {
        let bytes = Frame::notification(
            method::INVOKE_OUTPUT,
            &InvokeOutputParams {
                id: self.id,
                stream,
                text,
            },
        )
        .and_then(|frame| frame.encode_limited(Limits::PROTOCOL_1.max_line));
        if !bytes.is_ok_and(|bytes| self.frames.tx.try_send(bytes).is_ok()) {
            self.dropped.store(true, Ordering::Relaxed);
        }
    }
}

/// Why a `host.request` gave no answer.
#[derive(Debug, Clone, PartialEq)]
pub enum HostError {
    /// The daemon answered with an error (a refused grant, the rate limit, a busy or stopping
    /// daemon, a request it cannot read).
    Refused(ErrorObject),
    /// The daemon went away, or the adapter is stopping: nobody will answer.
    Gone,
}

impl std::fmt::Display for HostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HostError::Refused(e) => match e.name() {
                Some(name) => write!(f, "{name}: {}", e.message),
                None => write!(f, "{}", e.message),
            },
            HostError::Gone => write!(f, "the daemon is gone"),
        }
    }
}

impl std::error::Error for HostError {}

type Answer = Result<Params, ErrorObject>;

/// The requests this adapter has sent to the daemon and has no answer to yet.
#[derive(Default)]
struct Pending {
    next: AtomicU64,
    waiting: Mutex<Option<HashMap<u64, oneshot::Sender<Answer>>>>,
}

impl Pending {
    fn open() -> Pending {
        Pending {
            next: AtomicU64::new(1),
            waiting: Mutex::new(Some(HashMap::new())),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<HashMap<u64, oneshot::Sender<Answer>>>> {
        self.waiting.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// A new id and the place its answer will arrive; `None` once the adapter is stopping.
    fn register(&self) -> Option<(u64, oneshot::Receiver<Answer>)> {
        let mut waiting = self.lock();
        let map = waiting.as_mut()?;
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        map.insert(id, tx);
        Some((id, rx))
    }

    fn forget(&self, id: u64) {
        if let Some(map) = self.lock().as_mut() {
            map.remove(&id);
        }
    }

    /// An answer from the daemon. One for an id nobody waits for (a request given up on) is dropped.
    fn complete(&self, id: u64, answer: Answer) {
        let sender = self.lock().as_mut().and_then(|map| map.remove(&id));
        if let Some(sender) = sender {
            let _ = sender.send(answer);
        }
    }

    /// The input has ended: every waiter hears `Gone`, and no new request is accepted.
    fn close(&self) {
        drop(self.lock().take());
    }
}

/// Removes a request's entry when the caller stops waiting (a timeout, a cancelled task).
struct Registered<'a> {
    pending: &'a Pending,
    id: u64,
}

impl Drop for Registered<'_> {
    fn drop(&mut self) {
        self.pending.forget(self.id);
    }
}

/// The way back to the daemon: `host.request`. Cheap to clone. It holds the output queue only
/// weakly, so a clone kept by a background task does not keep the adapter from finishing.
#[derive(Clone)]
pub struct Host {
    frames: Weak<Outbox>,
    pending: Arc<Pending>,
}

impl Host {
    /// Send `request` (the object the CLI would send, such as `{"type":"status"}`) and wait for the
    /// daemon's answer, which is the response object it would have sent to the CLI. A response of
    /// type `error` is an answer, not a `HostError`. There is no time limit here; the caller sets
    /// one (dropping the future forgets the request).
    pub async fn request(
        &self,
        request: serde_json::Value,
    ) -> Result<serde_json::Value, HostError> {
        let (id, answer) = self.pending.register().ok_or(HostError::Gone)?;
        let _registered = Registered {
            pending: &self.pending,
            id,
        };
        let frame = Frame::request(
            id,
            method::HOST_REQUEST,
            &HostRequestParams {
                request,
                dry_run: false,
                end_user: None,
            },
        )
        .map_err(|_| HostError::Gone)?;
        let frames = self.frames.upgrade().ok_or(HostError::Gone)?;
        frames.send(frame).await?;
        match answer.await {
            Ok(Ok(result)) => {
                let result: HostRequestResult =
                    oxidepm_adapter_proto::from_params(&result).map_err(|_| HostError::Gone)?;
                Ok(result.response)
            }
            Ok(Err(error)) => Err(HostError::Refused(error)),
            Err(_) => Err(HostError::Gone),
        }
    }
}

/// Why the adapter stopped.
#[derive(Debug)]
pub enum Exit {
    /// The daemon asked it to shut down, or closed its end.
    Clean,
    /// It was not started by the daemon, or the handshake failed.
    Refused(String),
}

/// Run the adapter named in `name` and `version` (they must be those of the manifest: the daemon
/// compares them with what it installed). `make` builds the service once `initialize` arrived.
pub async fn run<S, F>(name: &str, version: &str, make: F) -> Exit
where
    S: Service,
    F: FnOnce(&InitializeParams) -> S,
{
    run_with_host(name, version, |init, _host| make(init)).await
}

/// Like [`run`], and `make` also gets the [`Host`], for an adapter that asks the daemon things.
pub async fn run_with_host<S, F>(name: &str, version: &str, make: F) -> Exit
where
    S: Service,
    F: FnOnce(&InitializeParams, Host) -> S,
{
    if std::env::var_os("OXIDEPM_ADAPTER_PROTOCOLS").is_none() {
        return Exit::Refused(
            "OXIDEPM_ADAPTER_PROTOCOLS is not set: this program is started by oxidepmd".to_string(),
        );
    }
    drive(name, version, make, tokio::io::stdin(), tokio::io::stdout()).await
}

/// Run an adapter whose verb owns the terminal (a session verb). Its standard input and output
/// are the terminal, not the protocol channel: the daemon names a socket and a single-use token
/// in `OXIDEPM_SESSION_SOCKET` and `OXIDEPM_SESSION_TOKEN`, this connects, sends the one line
/// that greets the daemon, and speaks the same protocol as [`run_with_host`] over that
/// connection. The verb's `invoke` is the program that uses the terminal; its `exit_code` is the
/// exit code of `oxidepm <verb>`.
pub async fn run_session<S, F>(name: &str, version: &str, make: F) -> Exit
where
    S: Service,
    F: FnOnce(&InitializeParams, Host) -> S,
{
    if std::env::var_os("OXIDEPM_ADAPTER_PROTOCOLS").is_none() {
        return Exit::Refused(
            "OXIDEPM_ADAPTER_PROTOCOLS is not set: this program is started by oxidepmd".to_string(),
        );
    }
    let (Some(path), Some(token)) = (
        std::env::var_os("OXIDEPM_SESSION_SOCKET"),
        std::env::var("OXIDEPM_SESSION_TOKEN").ok(),
    ) else {
        return Exit::Refused("OXIDEPM_SESSION_SOCKET and OXIDEPM_SESSION_TOKEN are not set: this verb runs in a session opened by oxidepm".to_string());
    };
    run_session_on(std::path::Path::new(&path), &token, name, version, make).await
}

/// [`run_session`] with the socket and the token given.
pub async fn run_session_on<S, F>(
    socket: &std::path::Path,
    token: &str,
    name: &str,
    version: &str,
    make: F,
) -> Exit
where
    S: Service,
    F: FnOnce(&InitializeParams, Host) -> S,
{
    let mut stream = match tokio::net::UnixStream::connect(socket).await {
        Ok(stream) => stream,
        Err(e) => {
            return Exit::Refused(format!(
                "cannot reach the session socket {}: {e}",
                socket.display()
            ))
        }
    };
    let hello = format!(
        "{}\n",
        serde_json::json!({ "session": "adapter", "token": token })
    );
    if let Err(e) = stream.write_all(hello.as_bytes()).await {
        return Exit::Refused(format!("cannot greet the session socket: {e}"));
    }
    let (read, write) = stream.into_split();
    drive(name, version, make, read, write).await
}

async fn drive<S, F, R, W>(name: &str, version: &str, make: F, input: R, output: W) -> Exit
where
    S: Service,
    F: FnOnce(&InitializeParams, Host) -> S,
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (frames, mut queue) = Outbox::channel();
    let mut writer = tokio::spawn(async move {
        let mut output = output;
        while let Some(bytes) = queue.recv().await {
            let write = async {
                output.write_all(&bytes).await?;
                output.flush().await
            };
            if !matches!(
                tokio::time::timeout(OUTPUT_TIMEOUT, write).await,
                Ok(Ok(()))
            ) {
                break;
            }
        }
    });

    let pending = Arc::new(Pending::open());
    let host = Host {
        frames: Arc::downgrade(&frames),
        pending: Arc::clone(&pending),
    };
    let mut reader = AsyncFrameReader::new(BufReader::new(input), Limits::PROTOCOL_1.max_line * 2);
    let mut service: Option<Arc<S>> = None;
    let mut make = Some(make);
    let mut calls = tokio::task::JoinSet::new();
    let mut active: HashMap<u64, AbortHandle> = HashMap::new();
    let (exit, output_failed) = loop {
        let read = tokio::select! {
            biased;
            _ = frames.invalid_frame.notified() => {
                break (Exit::Refused("adapter protocol frame could not be encoded".to_string()), true);
            }
            _ = frames.tx.closed() => {
                break (Exit::Refused("adapter protocol output closed".to_string()), true);
            }
            complete = calls.join_next_with_id(), if !calls.is_empty() => {
                if let Some(result) = complete {
                    let task = match result {
                        Ok((task, ())) => task,
                        Err(error) => error.id(),
                    };
                    active.retain(|_, handle| handle.id() != task);
                }
                continue;
            }
            read = reader.read_frame() => read,
        };
        let frame = match read {
            Ok(Read::Frame(frame)) => frame,
            Ok(Read::Eof) | Ok(Read::Truncated { .. }) | Err(_) => break (Exit::Clean, false),
            Ok(Read::Violation(_)) | Ok(Read::Overflow { .. }) => continue,
        };
        match frame {
            Frame::Request {
                id,
                method: m,
                params,
            } if m == method::INITIALIZE => {
                let init: InitializeParams = match oxidepm_adapter_proto::from_params(&params) {
                    Ok(init) => init,
                    Err(e) => {
                        let _ = frames
                            .send(Frame::error(id, ErrorObject::new(-32602, e.to_string())))
                            .await;
                        break (Exit::Refused(format!("bad initialize: {e}")), false);
                    }
                };
                let Some(protocol) = SUPPORTED_PROTOCOLS
                    .iter()
                    .copied()
                    .filter(|p| init.protocols.contains(p))
                    .max()
                else {
                    let _ = frames
                        .send(Frame::error(
                            id,
                            ErrorObject::with_name(
                                -32022,
                                "E_ADAPTER_PROTOCOL",
                                "no protocol in common",
                            ),
                        ))
                        .await;
                    break (Exit::Refused("no protocol in common".to_string()), false);
                };
                if let Some(make) = make.take() {
                    service = Some(Arc::new(make(&init, host.clone())));
                }
                let result = InitializeResult {
                    protocol,
                    name: name.to_string(),
                    version: version.to_string(),
                };
                if let Ok(frame) = Frame::response(id, &result) {
                    let _ = frames.send(frame).await;
                }
            }
            Frame::Request { id, method: m, .. } if m == method::PING => {
                let _ = frames
                    .send(Frame::Response {
                        id,
                        result: Default::default(),
                    })
                    .await;
            }
            Frame::Request { id, method: m, .. } if m == method::SHUTDOWN => {
                if let Some(service) = service.take() {
                    service.shutdown().await;
                }
                let _ = frames
                    .send(Frame::Response {
                        id,
                        result: Default::default(),
                    })
                    .await;
                break (Exit::Clean, false);
            }
            Frame::Request {
                id,
                method: m,
                params,
            } if matches!(
                m.as_str(),
                method::COMMAND_INVOKE | method::RUNTIME_PREPARE | method::RUNTIME_PLAN
            ) =>
            {
                let Some(service) = service.clone() else {
                    let _ = frames
                        .send(Frame::error(
                            id,
                            ErrorObject::new(-32603, "not initialized"),
                        ))
                        .await;
                    continue;
                };
                if active.contains_key(&id)
                    || active.len() >= Limits::PROTOCOL_1.max_in_flight as usize
                {
                    let _ = frames
                        .send(Frame::error(
                            id,
                            ErrorObject::new(
                                -32600,
                                "duplicate request or too many active requests",
                            ),
                        ))
                        .await;
                    continue;
                }
                let frames = frames.clone();
                let task = calls.spawn(async move {
                    let reply = match m.as_str() {
                        method::COMMAND_INVOKE => {
                            let call = oxidepm_adapter_proto::from_params::<InvokeParams>(&params);
                            match call {
                                Ok(call) => {
                                    let out = Output { id, frames: frames.clone(), dropped: Arc::new(AtomicBool::new(false)) };
                                    let result = service.invoke(call, out.clone()).await;
                                    if out.dropped.load(Ordering::Relaxed) {
                                        Err(ErrorObject::new(-32603, "adapter command output was incomplete; some output was dropped"))
                                    } else {
                                        Frame::response(id, &result).map_err(|e| ErrorObject::new(-32603, e.to_string()))
                                    }
                                }
                                Err(e) => Err(ErrorObject::new(-32602, e.to_string())),
                            }
                        }
                        method::RUNTIME_PREPARE => {
                            match oxidepm_adapter_proto::from_params::<PrepareParams>(&params) {
                                Ok(p) => service.runtime_prepare(p).await.and_then(|r| Frame::response(id, &r).map_err(|e| ErrorObject::new(-32603, e.to_string()))),
                                Err(e) => Err(ErrorObject::new(-32602, e.to_string())),
                            }
                        }
                        _ => {
                            match oxidepm_adapter_proto::from_params::<PlanParams>(&params) {
                                Ok(p) => service.runtime_plan(p).await.and_then(|r| Frame::response(id, &r).map_err(|e| ErrorObject::new(-32603, e.to_string()))),
                                Err(e) => Err(ErrorObject::new(-32602, e.to_string())),
                            }
                        }
                    };
                    let _ = frames.send(reply.unwrap_or_else(|error| Frame::error(id, error))).await;
                });
                active.insert(id, task);
            }
            Frame::Request { id, method: m, .. } => {
                let _ = frames
                    .send(Frame::error(
                        id,
                        ErrorObject::new(-32601, format!("unknown method {m}")),
                    ))
                    .await;
            }
            Frame::Notification { method: m, params } if m == method::EVENT => {
                if let (Some(service), Ok(event)) = (
                    service.clone(),
                    oxidepm_adapter_proto::from_params::<EventParams>(&params),
                ) {
                    service.event(event).await;
                }
            }
            Frame::Response { id, result } => pending.complete(id, Ok(result)),
            Frame::Error { id, error } => pending.complete(id, Err(error)),
            Frame::Notification { method: m, params } if m == method::CANCEL => {
                if let Ok(cancel) = oxidepm_adapter_proto::from_params::<CancelParams>(&params) {
                    if let Some(call) = active.get(&cancel.id) {
                        call.abort();
                    }
                }
            }
            Frame::Notification { .. } => {}
        }
    };
    pending.close();
    calls.shutdown().await;
    if output_failed {
        writer.abort();
    }
    if let Some(service) = service.take() {
        service.shutdown().await;
    }
    drop(frames);
    if tokio::time::timeout(OUTPUT_TIMEOUT, &mut writer)
        .await
        .is_err()
    {
        writer.abort();
        let _ = writer.await;
    }
    exit
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::AsyncBufReadExt;

    fn host() -> (Host, Arc<Pending>, Arc<Outbox>, mpsc::Receiver<Vec<u8>>) {
        let (tx, rx) = Outbox::channel();
        let pending = Arc::new(Pending::open());
        (
            Host {
                frames: Arc::downgrade(&tx),
                pending: Arc::clone(&pending),
            },
            pending,
            tx,
            rx,
        )
    }

    async fn next_frame(rx: &mut mpsc::Receiver<Vec<u8>>) -> Frame {
        oxidepm_adapter_proto::parse_frame(&rx.recv().await.unwrap()).unwrap()
    }

    #[tokio::test]
    async fn a_response_completes_the_request_it_answers() {
        let (host, pending, _tx, mut rx) = host();
        let call = tokio::spawn(async move { host.request(json!({"type": "status"})).await });
        let Frame::Request {
            id,
            method: m,
            params,
        } = next_frame(&mut rx).await
        else {
            panic!("a request")
        };
        assert_eq!(m, "host.request");
        assert_eq!(params["request"], json!({"type": "status"}));
        let result = oxidepm_adapter_proto::to_params(&HostRequestResult {
            response: json!({"type": "status", "apps": []}),
        })
        .unwrap();
        pending.complete(id, Ok(result));
        assert_eq!(
            call.await.unwrap().unwrap(),
            json!({"type": "status", "apps": []})
        );
    }

    #[tokio::test]
    async fn an_error_completes_it_as_refused_and_ids_do_not_repeat() {
        let (host, pending, _tx, mut rx) = host();
        let first = {
            let host = host.clone();
            tokio::spawn(async move { host.request(json!({})).await })
        };
        let Frame::Request { id: a, .. } = next_frame(&mut rx).await else {
            panic!("a request")
        };
        let second = tokio::spawn(async move { host.request(json!({})).await });
        let Frame::Request { id: b, .. } = next_frame(&mut rx).await else {
            panic!("a request")
        };
        assert_ne!(a, b);
        pending.complete(
            a,
            Err(ErrorObject::with_name(
                -32000,
                "E_RATE_LIMITED",
                "slow down",
            )),
        );
        pending.complete(b, Err(ErrorObject::new(-32603, "broken")));
        assert_eq!(
            first.await.unwrap().unwrap_err().to_string(),
            "E_RATE_LIMITED: slow down"
        );
        assert_eq!(second.await.unwrap().unwrap_err().to_string(), "broken");
    }

    #[tokio::test]
    async fn closing_wakes_the_waiters_and_refuses_new_requests() {
        let (host, pending, _tx, mut rx) = host();
        let waiting = {
            let host = host.clone();
            tokio::spawn(async move { host.request(json!({})).await })
        };
        let _ = rx.recv().await;
        pending.close();
        assert_eq!(waiting.await.unwrap(), Err(HostError::Gone));
        assert_eq!(host.request(json!({})).await, Err(HostError::Gone));
    }

    #[tokio::test]
    async fn a_request_that_is_given_up_on_leaves_nothing_behind() {
        let (host, pending, _tx, _rx) = host();
        let gave_up = tokio::time::timeout(
            std::time::Duration::from_millis(20),
            host.request(json!({})),
        )
        .await;
        assert!(gave_up.is_err());
        assert_eq!(pending.lock().as_ref().map(|m| m.len()), Some(0));
        // an answer that comes late is dropped quietly
        pending.complete(1, Ok(Params::new()));
    }

    #[tokio::test]
    async fn the_host_does_not_keep_the_output_queue_open() {
        let (host, _pending, tx, mut rx) = host();
        drop(tx);
        assert!(rx.recv().await.is_none(), "only the weak handle is left");
        assert_eq!(host.request(json!({})).await, Err(HostError::Gone));
    }

    struct Echo;

    #[async_trait]
    impl Service for Echo {
        async fn event(self: Arc<Self>, _event: EventParams) {}
        async fn invoke(self: Arc<Self>, call: InvokeParams, _out: Output) -> InvokeResult {
            InvokeResult {
                exit_code: if call.tty { 7 } else { 3 },
                result: Some(json!({ "verb": call.verb })),
            }
        }
        async fn shutdown(self: Arc<Self>) {}
    }

    async fn line(
        reader: &mut tokio::io::BufReader<tokio::net::unix::OwnedReadHalf>,
    ) -> serde_json::Value {
        let mut text = String::new();
        reader.read_line(&mut text).await.unwrap();
        serde_json::from_str(&text).unwrap_or_else(|_| panic!("a JSON line, got {text:?}"))
    }

    #[tokio::test]
    async fn a_session_greets_first_then_speaks_the_protocol_over_the_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let adapter = {
            let path = path.clone();
            tokio::spawn(async move {
                run_session_on(&path, "tok-1", "monitor", "0.1.0", |_, _| Echo).await
            })
        };
        let (stream, _) = listener.accept().await.unwrap();
        let (read, mut write) = stream.into_split();
        let mut read = tokio::io::BufReader::new(read);
        // nothing is sent before the greeting, and the greeting is one line with the token
        assert_eq!(
            line(&mut read).await,
            json!({ "session": "adapter", "token": "tok-1" })
        );
        let send = |v: serde_json::Value| format!("{v}\n");
        write
            .write_all(
                send(
                    json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
                "protocols": [1],
                "host": { "name": "oxidepmd", "version": "0.1.0" },
                "adapter": { "name": "monitor", "version": "0.1.0" },
                "kind": "tool",
                "grant": { "level": "read", "scope": "all" },
                "paths": { "state": "/x" },
                "limits": { "max_line": 262144, "max_in_flight": 32, "event_queue": 1024 }
            } }),
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let answer = line(&mut read).await;
        assert_eq!(answer["id"], 1);
        assert_eq!(answer["result"]["name"], "monitor", "{answer}");
        write
            .write_all(
                send(json!({ "jsonrpc": "2.0", "id": 2, "method": "command.invoke", "params": { "verb": "monit", "argv": [], "cwd": "/", "actor": { "class": "human", "label": "cli" }, "tty": true } }))
                    .as_bytes(),
            )
            .await
            .unwrap();
        let answer = line(&mut read).await;
        assert_eq!(answer["id"], 2);
        assert_eq!(answer["result"]["exit_code"], 7, "{answer}");
        write
            .write_all(
                send(json!({ "jsonrpc": "2.0", "id": 3, "method": "shutdown", "params": {} }))
                    .as_bytes(),
            )
            .await
            .unwrap();
        assert_eq!(line(&mut read).await["id"], 3);
        assert!(matches!(adapter.await.unwrap(), Exit::Clean));
    }

    struct Fixed;

    impl RuntimeProvider for Fixed {
        fn prepare(&self, params: &PrepareParams) -> Result<PrepareResult, Refusal> {
            if params.spec.command == "refuse" {
                return Err(Refusal::new("no way"));
            }
            Ok(PrepareResult {
                steps: Vec::new(),
                notes: vec![format!("phase {:?}", params.phase)],
            })
        }

        fn plan(&self, params: &PlanParams) -> Result<PlanResult, Refusal> {
            Ok(PlanResult {
                program: "tool".to_string(),
                args: vec![params.spec.command.clone()],
                cwd: params.spec.cwd.clone(),
                extra_env: Default::default(),
                binary_path: None,
            })
        }
    }

    /// An adapter over an in-memory pipe, initialized, and the means to talk to it.
    async fn connected<S: Service>(
        service: S,
    ) -> (
        tokio::io::BufReader<tokio::io::DuplexStream>,
        tokio::task::JoinHandle<Exit>,
    ) {
        let (ours, theirs) = tokio::io::duplex(1 << 20);
        let (theirs_read, theirs_write) = tokio::io::split(theirs);
        let task = tokio::spawn(async move {
            drive(
                "rt",
                "0.1.0",
                move |_, _| service,
                theirs_read,
                theirs_write,
            )
            .await
        });
        let mut ours = tokio::io::BufReader::new(ours);
        let init = json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocols": [1],
            "host": { "name": "oxidepmd", "version": "0.1.0" },
            "adapter": { "name": "rt", "version": "0.1.0" },
            "kind": "tool",
            "grant": { "level": "read", "scope": "all" },
            "paths": { "state": "/x" },
            "limits": { "max_line": 262144, "max_in_flight": 32, "event_queue": 1024 }
        } });
        ours.write_all(format!("{init}\n").as_bytes())
            .await
            .unwrap();
        let mut text = String::new();
        ours.read_line(&mut text).await.unwrap();
        assert!(text.contains("\"result\""), "{text}");
        (ours, task)
    }

    async fn ask(
        ours: &mut tokio::io::BufReader<tokio::io::DuplexStream>,
        id: u64,
        method: &str,
        params: serde_json::Value,
    ) -> serde_json::Value {
        ours.write_all(
            format!(
                "{}\n",
                json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
            )
            .as_bytes(),
        )
        .await
        .unwrap();
        let mut text = String::new();
        ours.read_line(&mut text).await.unwrap();
        serde_json::from_str(&text).unwrap_or_else(|_| panic!("a JSON line, got {text:?}"))
    }

    fn view(command: &str) -> serde_json::Value {
        json!({ "name": "app", "runtime": "x", "command": command, "args": [], "cwd": "/srv/app", "instances": 1 })
    }

    struct Cancellable {
        started: Mutex<Option<oneshot::Sender<()>>>,
        finished: Mutex<Option<oneshot::Sender<()>>>,
    }

    #[async_trait]
    impl Service for Cancellable {
        async fn event(self: Arc<Self>, _event: EventParams) {}

        async fn invoke(self: Arc<Self>, _call: InvokeParams, _out: Output) -> InvokeResult {
            let _finished = self.finished.lock().unwrap().take().unwrap();
            self.started
                .lock()
                .unwrap()
                .take()
                .unwrap()
                .send(())
                .unwrap();
            std::future::pending().await
        }

        async fn shutdown(self: Arc<Self>) {}
    }

    async fn start_call(ours: &mut tokio::io::BufReader<tokio::io::DuplexStream>) {
        let call = json!({
            "jsonrpc": "2.0", "id": 2, "method": "command.invoke",
            "params": { "verb": "test", "cwd": "/", "actor": { "class": "human", "label": "cli" } }
        });
        ours.write_all(format!("{call}\n").as_bytes())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn cancellation_drops_the_running_call() {
        let (started, running) = oneshot::channel();
        let (finished, stopped) = oneshot::channel();
        let service = Cancellable {
            started: Mutex::new(Some(started)),
            finished: Mutex::new(Some(finished)),
        };
        let (mut ours, task) = connected(service).await;
        start_call(&mut ours).await;
        running.await.unwrap();
        ours.write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"cancel\",\"params\":{\"id\":2}}\n")
            .await
            .unwrap();
        let dropped = tokio::time::timeout(std::time::Duration::from_secs(1), stopped).await;
        assert!(
            matches!(dropped, Ok(Err(_))),
            "the cancelled call was still running"
        );
        let reply = ask(&mut ours, 3, "ping", json!({})).await;
        assert_eq!(reply["id"], 3, "a cancelled call sent a reply: {reply}");
        drop(ours);
        assert!(matches!(task.await.unwrap(), Exit::Clean));
    }

    struct Chatty;

    #[async_trait]
    impl Service for Chatty {
        async fn event(self: Arc<Self>, _event: EventParams) {}

        async fn invoke(self: Arc<Self>, _call: InvokeParams, out: Output) -> InvokeResult {
            let text = "x".repeat(32 * 1024);
            for _ in 0..256 {
                out.stdout(text.clone());
            }
            InvokeResult {
                exit_code: 0,
                result: None,
            }
        }

        async fn shutdown(self: Arc<Self>) {}
    }

    #[tokio::test]
    async fn a_full_output_queue_reports_dropped_output_to_the_command() {
        let (mut ours, task) = connected(Chatty).await;
        start_call(&mut ours).await;
        let (answer, chunks) = tokio::time::timeout(Duration::from_secs(2), async {
            let mut chunks = 0;
            loop {
                let mut text = String::new();
                assert_ne!(
                    ours.read_line(&mut text).await.unwrap(),
                    0,
                    "the adapter closed without a reply"
                );
                let frame: serde_json::Value = serde_json::from_str(&text).unwrap();
                if frame["id"] == 2 {
                    break (frame, chunks);
                }
                assert_eq!(frame["method"], method::INVOKE_OUTPUT);
                assert_eq!(frame["params"]["id"], 2);
                chunks += 1;
            }
        })
        .await
        .expect("the command gets a reply");
        assert_eq!(chunks, OUTPUT_QUEUE);
        assert_eq!(
            answer["error"]["message"],
            "adapter command output was incomplete; some output was dropped",
            "{answer}"
        );
        assert_eq!(ask(&mut ours, 3, "ping", json!({})).await["id"], 3);
        drop(ours);
        assert!(matches!(task.await.unwrap(), Exit::Clean));
    }

    #[tokio::test(start_paused = true)]
    async fn a_blocked_writer_cannot_accumulate_unbounded_output() {
        let (mut ours, task) = connected(Chatty).await;
        start_call(&mut ours).await;
        let started = tokio::time::Instant::now();
        let exit = tokio::time::timeout(OUTPUT_TIMEOUT + Duration::from_secs(1), task).await;
        assert!(tokio::time::Instant::now() - started >= OUTPUT_TIMEOUT);
        assert!(
            matches!(exit, Ok(Ok(Exit::Refused(ref reason))) if reason.contains("output closed")),
            "{exit:?}"
        );
    }

    #[tokio::test]
    async fn a_runtime_adapter_answers_prepare_and_plan() {
        let (mut ours, task) = connected(RuntimeService(Fixed)).await;
        let prepared = ask(
            &mut ours,
            2,
            "runtime.prepare",
            json!({ "spec": view("go"), "path_hint": "/usr/bin", "phase": "restart" }),
        )
        .await;
        assert_eq!(
            prepared["result"]["notes"],
            json!(["phase Restart"]),
            "{prepared}"
        );
        let planned = ask(
            &mut ours,
            3,
            "runtime.plan",
            json!({ "spec": view("go"), "steps_done": [] }),
        )
        .await;
        assert_eq!(planned["result"]["program"], "tool", "{planned}");
        assert_eq!(planned["result"]["args"], json!(["go"]));
        assert_eq!(planned["result"]["cwd"], "/srv/app");
        let _ = ask(&mut ours, 4, "shutdown", json!({})).await;
        assert!(matches!(task.await.unwrap(), Exit::Clean));
    }

    #[tokio::test]
    async fn a_refusal_comes_back_as_an_error_with_its_text() {
        let (mut ours, task) = connected(RuntimeService(Fixed)).await;
        let refused = ask(
            &mut ours,
            2,
            "runtime.prepare",
            json!({ "spec": view("refuse"), "path_hint": "", "phase": "start" }),
        )
        .await;
        assert_eq!(refused["error"]["message"], "no way", "{refused}");
        // a request that does not have the shape is an error, not a crash
        let bad = ask(&mut ours, 3, "runtime.plan", json!({ "nothing": true })).await;
        assert!(bad["error"]["message"].is_string(), "{bad}");
        let _ = ask(&mut ours, 4, "shutdown", json!({})).await;
        assert!(matches!(task.await.unwrap(), Exit::Clean));
    }

    #[tokio::test]
    async fn an_adapter_without_a_runtime_refuses_the_runtime_calls() {
        let (mut ours, task) = connected(Echo).await;
        let answer = ask(
            &mut ours,
            2,
            "runtime.prepare",
            json!({ "spec": view("x"), "path_hint": "", "phase": "start" }),
        )
        .await;
        assert_eq!(answer["error"]["code"], -32601, "{answer}");
        let _ = ask(&mut ours, 3, "shutdown", json!({})).await;
        assert!(matches!(task.await.unwrap(), Exit::Clean));
    }

    #[tokio::test]
    async fn a_session_without_a_reachable_socket_is_refused_and_says_why() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("none.sock");
        match run_session_on(&missing, "t", "monitor", "0.1.0", |_, _| Echo).await {
            Exit::Refused(why) => assert!(why.contains("cannot reach the session socket"), "{why}"),
            Exit::Clean => panic!("nothing to talk to"),
        }
    }
}
