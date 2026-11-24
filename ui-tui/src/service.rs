//! The adapter as the SDK sees it: one session verb, `monit`.

use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use oxidepm_adapter_proto::messages::{EventParams, InvokeParams, InvokeResult};
use oxidepm_adapter_sdk::{Host, Output, Service};
use tokio::sync::mpsc;

use crate::daemon::{Daemon, HostDaemon};
use crate::{run, term};

pub struct Monitor {
    host: Host,
    /// Whether this process was started for a session: only then are standard input and output
    /// the terminal. Otherwise they carry the protocol and nothing else may be written to them.
    session: bool,
}

impl Monitor {
    pub fn new(host: Host, session: bool) -> Monitor {
        Monitor { host, session }
    }
}

/// What the person sees on the terminal when the dashboard cannot start. The terminal is
/// standard output; standard error is the adapter's log.
fn say(text: &str) {
    println!("{text}");
}

#[async_trait]
impl Service for Monitor {
    async fn event(self: Arc<Self>, _event: EventParams) {}

    async fn invoke(self: Arc<Self>, call: InvokeParams, out: Output) -> InvokeResult {
        if !self.session {
            out.stderr("monit opens a dashboard on your terminal: run it with oxidepm monit");
            return InvokeResult {
                exit_code: 2,
                result: None,
            };
        }
        if call.verb != "monit" {
            say(&format!("this adapter has no verb '{}'", call.verb));
            return InvokeResult {
                exit_code: 2,
                result: None,
            };
        }
        if !call.argv.is_empty() {
            say("oxidepm monit takes no arguments");
            return InvokeResult {
                exit_code: 2,
                result: None,
            };
        }
        if !call.tty || !std::io::stdout().is_terminal() {
            say("oxidepm monit needs a terminal for its input and its output");
            return InvokeResult {
                exit_code: 2,
                result: None,
            };
        }
        let daemon = Arc::new(HostDaemon {
            host: self.host.clone(),
        });
        // its own task: a panic ends the dashboard (the terminal is put back) and not the answer
        let dashboard = tokio::spawn(dashboard(daemon));
        let exit_code = match dashboard.await {
            Ok(Ok(())) => 0,
            Ok(Err(e)) => {
                eprintln!("ui-tui: the dashboard stopped: {e}");
                1
            }
            Err(e) => {
                eprintln!("ui-tui: the dashboard failed: {e}");
                1
            }
        };
        InvokeResult {
            exit_code,
            result: None,
        }
    }

    async fn shutdown(self: Arc<Self>) {}
}

async fn dashboard(daemon: Arc<dyn Daemon>) -> std::io::Result<()> {
    let terminal = term::enter()?;
    let (tx, mut events) = mpsc::unbounded_channel();
    let stop = Arc::new(AtomicBool::new(false));
    let reader = term::read_events(tx, Arc::clone(&stop));
    let result = run::run_guarded(terminal, term::leave, daemon.as_ref(), &mut events).await;
    stop.store(true, Ordering::Relaxed);
    let _ = reader.join();
    result
}
