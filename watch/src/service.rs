use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use oxidepm_adapter_proto::messages::{EventParams, InvokeParams, InvokeResult};
use oxidepm_adapter_sdk::{Host, Output, Service};
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};
use tokio::task::{AbortHandle, JoinHandle, JoinSet};
use tokio::time::timeout;

use crate::config::WatchOptions;
use crate::watcher;

const READ_TIMEOUT: Duration = Duration::from_secs(5);
const RESTART_TIMEOUT: Duration = Duration::from_secs(10);

pub struct Watch {
    events: mpsc::UnboundedSender<EventParams>,
    worker: Mutex<Option<Worker>>,
}

struct Worker {
    stop: oneshot::Sender<()>,
    task: JoinHandle<()>,
}

impl Watch {
    pub fn new(host: Host) -> Self {
        let (events, receive) = mpsc::unbounded_channel();
        let (stop, shutdown) = oneshot::channel();
        let task = tokio::spawn(track(host, receive, shutdown));
        Self {
            events,
            worker: Mutex::new(Some(Worker { stop, task })),
        }
    }
}

#[async_trait]
impl Service for Watch {
    async fn event(self: Arc<Self>, event: EventParams) {
        if event.family == "lifecycle" {
            let _ = self.events.send(event);
        }
    }

    async fn invoke(self: Arc<Self>, _: InvokeParams, _: Output) -> InvokeResult {
        InvokeResult {
            exit_code: 2,
            result: None,
        }
    }

    async fn shutdown(self: Arc<Self>) {
        let worker = self.worker.lock().unwrap().take();
        if let Some(worker) = worker {
            let _ = worker.stop.send(());
            let _ = worker.task.await;
        }
    }
}

async fn track(
    host: Host,
    mut events: mpsc::UnboundedReceiver<EventParams>,
    mut shutdown: oneshot::Receiver<()>,
) {
    let mut jobs = JoinSet::new();
    let mut watched: HashMap<u64, AbortHandle> = HashMap::new();
    let mut seen = HashSet::new();
    let mut discovering = true;
    let discovery = request(&host, json!({ "type": "status" }), READ_TIMEOUT);
    tokio::pin!(discovery);

    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            event = events.recv() => {
                let Some(event) = event else { break };
                let Some(app) = event.app else { continue };
                let id = u64::from(app.id);
                match event.kind.as_str() {
                    "started" | "restarted" | "stopped" | "errored" | "deleted" | "crashed" => {},
                    _ => continue,
                }
                // A status reply may have been made before a lifecycle change we already saw.
                if discovering {
                    seen.insert(id);
                }
                if let Some(job) = watched.remove(&id) {
                    job.abort();
                }
                if !matches!(event.kind.as_str(), "started" | "restarted") {
                    continue;
                }
                let Some(options) = event.options else { continue };
                let options = match serde_json::from_value(Value::Object(options)) {
                    Ok(options) => options,
                    Err(err) => {
                        eprintln!("watch: invalid options for '{}': {err}", app.name);
                        continue;
                    }
                };
                let host = host.clone();
                let job = jobs.spawn(async move {
                    match lookup_cwd(&host, &app.name, id).await {
                        Ok(Some(cwd)) => watch_app(host, app.name, cwd, options).await,
                        Ok(None) => {}
                        Err(err) => eprintln!("watch: cannot look up '{}': {err}", app.name),
                    }
                });
                watched.insert(id, job);
            }
            result = &mut discovery, if discovering => {
                discovering = false;
                match result {
                    Ok(status) => {
                        for app in status["apps"].as_array().into_iter().flatten() {
                            let Some(id) = app["state"]["app_id"].as_u64() else { continue };
                            let Some(name) = app["spec"]["name"].as_str() else { continue };
                            if seen.contains(&id)
                                || !matches!(app["state"]["status"].as_str(), Some("starting" | "running" | "unhealthy"))
                            {
                                continue;
                            }
                            if let Some(cwd) = opted_in_cwd(app) {
                                let options = match WatchOptions::from_redacted(&app["spec"]["adapters"]["watch"]) {
                                    Ok(options) => options,
                                    Err(err) => {
                                        eprintln!("watch: invalid options for '{name}': {err}");
                                        continue;
                                    }
                                };
                                let job = jobs.spawn(watch_app(host.clone(), name.to_owned(), cwd, options));
                                watched.insert(id, job);
                            }
                        }
                    }
                    Err(err) => eprintln!("watch: cannot discover apps: {err}"),
                }
                seen.clear();
            }
            result = jobs.join_next(), if !jobs.is_empty() => {
                if let Some(Err(err)) = result {
                    if err.is_panic() {
                        eprintln!("watch: a watcher failed: {err}");
                    }
                }
                watched.retain(|_, job| !job.is_finished());
            }
        }
    }
    jobs.shutdown().await;
}

fn opted_in_cwd(app: &Value) -> Option<PathBuf> {
    let spec = &app["spec"];
    spec["adapters"].get("watch")?;
    spec["cwd"].as_str().map(PathBuf::from)
}

fn app_request(kind: &str, name: &str) -> Value {
    json!({ "type": kind, "selector": { "by": "name", "value": name } })
}

async fn request(host: &Host, body: Value, deadline: Duration) -> Result<Value, String> {
    timeout(deadline, host.request(body))
        .await
        .map_err(|_| "daemon request timed out".to_owned())?
        .map_err(|err| err.to_string())
}

async fn lookup_cwd(host: &Host, name: &str, id: u64) -> Result<Option<PathBuf>, String> {
    let reply = request(host, app_request("show", name), READ_TIMEOUT).await?;
    match reply["type"].as_str() {
        Some("show") => {
            let app = &reply["app"];
            Ok((app["state"]["app_id"].as_u64() == Some(id))
                .then(|| opted_in_cwd(app))
                .flatten())
        }
        Some("error") if reply["code"] == "E_APP_NOT_FOUND" => Ok(None),
        _ => Err(reply.to_string()),
    }
}

async fn watch_app(host: Host, name: String, cwd: PathBuf, options: WatchOptions) {
    let result = watcher::run(cwd, options, || restart(&host, &name)).await;
    if let Err(err) = result {
        eprintln!("watch: cannot watch '{name}': {err}");
    }
}

async fn restart(host: &Host, name: &str) {
    match request(host, app_request("restart", name), RESTART_TIMEOUT).await {
        Ok(reply) if reply["type"] == "restarted" && reply["count"] == 1 => {}
        Ok(reply) => eprintln!("watch: restart of '{name}' refused: {reply}"),
        Err(err) => eprintln!("watch: restart of '{name}' failed: {err}"),
    }
}
