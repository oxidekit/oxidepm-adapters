//! The exporter as a service: it listens for scrapes and takes no events and no verbs.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use oxidepm_adapter_proto::messages::{EventParams, InitializeParams, InvokeParams, InvokeResult};
use oxidepm_adapter_sdk::{Host, Output, Service};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use crate::config;
use crate::server::{serve, State};
use crate::source::{Cached, DaemonSource, StatusSource, CACHE_FOR, REQUEST_TIMEOUT};

/// How long to wait before trying to bind again (the port may be held by a process that is leaving).
const REBIND: Duration = Duration::from_secs(5);

pub struct Exporter {
    task: std::sync::Mutex<Option<JoinHandle<()>>>,
}

impl Exporter {
    pub fn new(init: &InitializeParams, host: Host) -> Exporter {
        let path = PathBuf::from(&init.paths.state).join("config.toml");
        let task = match config::load(&path) {
            Ok(config) => {
                let source: Box<dyn StatusSource> = Box::new(Cached::new(
                    DaemonSource {
                        host,
                        timeout: REQUEST_TIMEOUT,
                    },
                    CACHE_FOR,
                ));
                Some(tokio::spawn(listen(config, source)))
            }
            Err(error) => {
                // nothing is bound until the file is fixed (and the adapter restarted); `adapter info` shows this line
                eprintln!("exporter-prometheus: the configuration is not used and nothing is listening: {error}");
                None
            }
        };
        Exporter {
            task: std::sync::Mutex::new(task),
        }
    }
}

/// Bind the configured address, trying again until it works, and serve it.
async fn listen(config: config::Config, source: Box<dyn StatusSource>) {
    let mut said = String::new();
    let listener = loop {
        match TcpListener::bind(config.listen).await {
            Ok(listener) => break listener,
            Err(e) => {
                let why = bind_error(config.listen, &e);
                if why != said {
                    eprintln!("exporter-prometheus: {why}");
                    said = why;
                }
                tokio::time::sleep(REBIND).await;
            }
        }
    };
    eprintln!(
        "exporter-prometheus: listening on {}{}",
        config.listen,
        if config.key.is_some() {
            " (key required)"
        } else {
            ""
        }
    );
    let state = Arc::new(State {
        source,
        key: config.key,
        bound: listener.local_addr().unwrap_or(config.listen),
    });
    serve(listener, state).await;
}

fn bind_error(addr: SocketAddr, e: &std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::PermissionDenied => format!(
            "cannot listen on {addr}: not permitted. The sandbox only lets this adapter bind the ports its manifest lists \
             (127.0.0.1:9616 as shipped); another port needs an adapter built with it listed"
        ),
        std::io::ErrorKind::AddrInUse => format!("cannot listen on {addr}: the address is in use; trying again every {} seconds", REBIND.as_secs()),
        _ => format!("cannot listen on {addr}: {e}; trying again every {} seconds", REBIND.as_secs()),
    }
}

#[async_trait]
impl Service for Exporter {
    async fn event(self: Arc<Self>, _event: EventParams) {}

    async fn invoke(self: Arc<Self>, call: InvokeParams, out: Output) -> InvokeResult {
        out.stderr(format!(
            "exporter-prometheus has no command '{}'",
            call.verb
        ));
        InvokeResult {
            exit_code: 2,
            result: None,
        }
    }

    async fn shutdown(self: Arc<Self>) {
        let task = self.task.lock().unwrap_or_else(|p| p.into_inner()).take();
        if let Some(task) = task {
            task.abort();
        }
    }
}
