//! The notifier as a service: events in, messages out; relayed verbs for the person.
//!
//! Every channel has a queue and a task of its own. A message goes to each of them at once; one
//! channel after another is told in order (a crash is never announced before the start it follows),
//! and a channel that is slow or down only delays itself.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use oxidepm_adapter_proto::messages::{EventParams, InitializeParams, InvokeParams, InvokeResult};
use oxidepm_adapter_sdk::{Output, Service};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::channels::{budget, channels, client, Channel};
use crate::config::{self, Config, Stamp};
use crate::event::{alert_for, Alert};
use crate::limiter::{Limiter, Verdict};

/// Messages waiting for one channel; beyond this the new one is dropped (and said so on stderr).
pub const QUEUE: usize = 256;

pub struct Notifier {
    pub path: PathBuf,
    loaded: std::sync::Mutex<Loaded>,
    limiter: std::sync::Mutex<Limiter>,
    /// The tasks of pipelines that a new configuration replaced, still sending what they had.
    retired: std::sync::Mutex<Vec<JoinHandle<()>>>,
    /// One verb that changes the file at a time.
    pub edit: tokio::sync::Mutex<()>,
}

struct Loaded {
    read: bool,
    stamp: Option<Stamp>,
    config: Result<Arc<Config>, String>,
    pipeline: Option<Arc<Pipeline>>,
    /// The error already said on stderr, so a broken file is not repeated for every event.
    said: Option<String>,
}

/// One result of sending to one channel.
pub struct Sent {
    pub channel: &'static str,
    pub result: Result<(), String>,
}

/// The channels of one configuration, each with its queue and its task.
pub struct Pipeline {
    lanes: Vec<(&'static str, mpsc::Sender<Alert>)>,
    handles: std::sync::Mutex<Vec<JoinHandle<()>>>,
}

impl Pipeline {
    fn start(channels: Vec<Channel>, http: reqwest::Client, timeout: Duration) -> Pipeline {
        let mut lanes = Vec::new();
        let mut handles = Vec::new();
        for channel in channels {
            let (tx, mut rx) = mpsc::channel::<Alert>(QUEUE);
            lanes.push((channel.name(), tx));
            let http = http.clone();
            handles.push(tokio::spawn(async move {
                while let Some(alert) = rx.recv().await {
                    let result =
                        match tokio::time::timeout(budget(timeout), channel.send(&http, &alert))
                            .await
                        {
                            Ok(result) => result,
                            Err(_) => Err("timed out".to_string()),
                        };
                    if let Err(why) = result {
                        eprintln!(
                            "notifier: channel={} host={} event={} app={} failed: {why}",
                            channel.name(),
                            channel.host(),
                            alert.event,
                            alert.name
                        );
                    }
                }
            }));
        }
        Pipeline {
            lanes,
            handles: std::sync::Mutex::new(handles),
        }
    }

    fn offer(&self, alert: &Alert) {
        for (name, tx) in &self.lanes {
            if tx.try_send(alert.clone()).is_err() {
                eprintln!("notifier: channel={name} is {QUEUE} messages behind; one {} message for {} was dropped", alert.event, alert.name);
            }
        }
    }

    /// The tasks, to be waited for. Their queues close when the last owner of the pipeline lets go.
    fn take_handles(&self) -> Vec<JoinHandle<()>> {
        std::mem::take(&mut *self.handles.lock().unwrap_or_else(|p| p.into_inner()))
    }
}

impl Notifier {
    pub fn new(init: &InitializeParams) -> Notifier {
        Notifier {
            path: PathBuf::from(&init.paths.state).join("config.toml"),
            loaded: std::sync::Mutex::new(Loaded {
                read: false,
                stamp: None,
                config: Ok(Arc::new(Config::default())),
                pipeline: None,
                said: None,
            }),
            limiter: std::sync::Mutex::new(Limiter::default()),
            retired: std::sync::Mutex::new(Vec::new()),
            edit: tokio::sync::Mutex::new(()),
        }
    }

    /// The configuration as the file says now (read again when the file changed), with the
    /// pipeline made for it. A broken file is an error, not the defaults.
    fn current(&self) -> (Result<Arc<Config>, String>, Option<Arc<Pipeline>>) {
        let now = config::stamp(&self.path);
        let mut loaded = self.loaded.lock().unwrap_or_else(|p| p.into_inner());
        if !loaded.read || loaded.stamp != now {
            loaded.read = true;
            loaded.stamp = now;
            loaded.config = config::load(&self.path).map(Arc::new);
            // the old pipeline finishes what it has, then ends
            if let Some(old) = loaded.pipeline.take() {
                let mut retired = self.retired.lock().unwrap_or_else(|p| p.into_inner());
                retired.retain(|h| !h.is_finished());
                retired.extend(old.take_handles());
            }
            if let Ok(c) = &loaded.config {
                let list = channels(c);
                if !list.is_empty() {
                    loaded.pipeline = Some(Arc::new(Pipeline::start(
                        list,
                        client(c.timeout()),
                        c.timeout(),
                    )));
                }
            }
        }
        (loaded.config.clone(), loaded.pipeline.clone())
    }

    fn say_config_error_once(&self, error: &str) {
        let mut loaded = self.loaded.lock().unwrap_or_else(|p| p.into_inner());
        if loaded.said.as_deref() != Some(error) {
            loaded.said = Some(error.to_string());
            eprintln!("notifier: the configuration is not used, nothing is sent until it is fixed: {error}");
        }
    }

    fn passes(config: &Config, alert: &Alert) -> bool {
        if config
            .min_severity()
            .is_some_and(|min| alert.severity < min)
        {
            return false;
        }
        config.events.is_empty() || config.events.iter().any(|e| e == alert.event)
    }

    /// Send `alert` to every channel at once, each within its own time limit (the test verb).
    pub async fn send_all(
        http: &reqwest::Client,
        channels: &[Channel],
        alert: &Alert,
        timeout: Duration,
    ) -> Vec<Sent> {
        let sends = channels.iter().map(|channel| async move {
            let result =
                match tokio::time::timeout(budget(timeout), channel.send(http, alert)).await {
                    Ok(result) => result,
                    Err(_) => Err("timed out".to_string()),
                };
            Sent {
                channel: channel.name(),
                result,
            }
        });
        futures::future::join_all(sends).await
    }
}

#[async_trait]
impl Service for Notifier {
    async fn event(self: Arc<Self>, event: EventParams) {
        let Some(alert) = alert_for(&event) else {
            return;
        };
        let (config, pipeline) = self.current();
        let config = match config {
            Ok(config) => config,
            Err(error) => {
                self.say_config_error_once(&error);
                return;
            }
        };
        let Some(pipeline) = pipeline else { return };
        if !Self::passes(&config, &alert) {
            return;
        }
        let verdict = self
            .limiter
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .check(
                &alert.name,
                alert.event,
                alert.is_critical(),
                Instant::now(),
            );
        let alert = match verdict {
            Verdict::Send => alert,
            Verdict::Notice(text) => Alert::own("notice", &text),
            Verdict::Drop => return,
        };
        pipeline.offer(&alert);
    }

    async fn invoke(self: Arc<Self>, call: InvokeParams, out: Output) -> InvokeResult {
        crate::verbs::run(&self, call, out).await
    }

    async fn shutdown(self: Arc<Self>) {
        // the daemon waits two seconds for the answer: let the messages that are on their way
        // finish. Closing the queues ends the tasks when they have sent what they hold.
        let mut handles =
            std::mem::take(&mut *self.retired.lock().unwrap_or_else(|p| p.into_inner()));
        {
            let mut loaded = self.loaded.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(pipeline) = loaded.pipeline.take() {
                handles.extend(pipeline.take_handles());
                // `pipeline` (and with it the senders) is dropped here unless an event holds it
            }
        }
        let _ = tokio::time::timeout(
            Duration::from_millis(1500),
            futures::future::join_all(handles),
        )
        .await;
    }
}
