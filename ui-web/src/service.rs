//! The adapter as a service: it listens for HTTP, forwards the daemon's lifecycle events to the
//! WebSocket clients, and offers one verb, `web`, that says where it listens and prints the key.
//!
//! The key is made the first time the adapter starts (no `config.toml` yet), written with mode
//! 0600, and printed once, by `oxidepm web`; after that the verb only says where the key is kept.
//! `oxidepm web new-key` replaces it (the old one stops working at once) and prints the new one.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use oxidepm_adapter_proto::messages::{
    EventParams, Grant, InitializeParams, InvokeParams, InvokeResult,
};
use oxidepm_adapter_proto::{HostLevel, HostScope};
use oxidepm_adapter_sdk::{Host, Output, Service};
use oxidepm_core::fsutil::write_atomic;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use crate::api::{self, Shared};
use crate::config::{self, Config};
use crate::daemon::HostDaemon;
use crate::guard::Rules;
use crate::hub::Hub;
use crate::server;

/// How long to wait before trying to bind again (the port may be held by a process that is leaving).
const REBIND: Duration = Duration::from_secs(5);
/// The file that exists while the generated key has not been shown to anyone.
const UNSHOWN: &str = "key-unshown";

struct Running {
    shared: Arc<Shared>,
    task: JoinHandle<()>,
    listen: SocketAddr,
}

#[derive(Default)]
struct Inner {
    running: Option<Running>,
    /// Why nothing is listening.
    problem: Option<String>,
}

pub struct Web {
    state_dir: PathBuf,
    grant: Grant,
    host: Host,
    hub: Hub,
    version: String,
    inner: Mutex<Inner>,
    rotation: Mutex<()>,
}

impl Web {
    pub fn new(init: &InitializeParams, host: Host) -> Web {
        let web = Web {
            state_dir: PathBuf::from(&init.paths.state),
            grant: init.grant,
            host,
            hub: Hub::new(),
            version: init.adapter.version.clone(),
            inner: Mutex::new(Inner::default()),
            rotation: Mutex::new(()),
        };
        web.boot();
        web
    }

    fn config_path(&self) -> PathBuf {
        self.state_dir.join("config.toml")
    }

    fn unshown_path(&self) -> PathBuf {
        self.state_dir.join(UNSHOWN)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Read the configuration, make the key if there is none yet, and start listening.
    fn boot(&self) {
        let path = self.config_path();
        let outcome = match config::load(&path) {
            Ok(Some(config)) if config.key.is_some() => Ok(config),
            // a file with settings and no key: the settings stay, a key is added
            Ok(Some(config)) => self.give_key(config),
            Ok(None) => self.give_key(Config::default()),
            Err(why) => Err(why),
        };
        match outcome {
            Ok(config) => self.listen(config),
            Err(why) => {
                // nothing is bound until it is fixed; `adapter info` shows this line
                eprintln!("ui-web: nothing is listening: {why}");
                self.lock().problem = Some(why);
            }
        }
    }

    /// Make a key for `config`, write the file (the old one is kept as `config.toml.bak`) and note
    /// that nobody has seen the key yet.
    fn give_key(&self, mut config: Config) -> Result<Config, String> {
        config.key = Some(config::generate_key()?);
        let path = self.config_path();
        self.keep_backup()?;
        write_atomic(&path, config::render(&config).as_bytes(), true)
            .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
        write_atomic(&self.unshown_path(), b"", false)
            .map_err(|e| format!("cannot write {}: {e}", self.unshown_path().display()))?;
        eprintln!(
            "ui-web: made an access key in {}; `oxidepm web` prints it once",
            path.display()
        );
        Ok(config)
    }

    /// The file as it is now, as `config.toml.bak` (nothing when there is no file).
    fn keep_backup(&self) -> Result<(), String> {
        let path = self.config_path();
        let Ok(old) = std::fs::read(&path) else {
            return Ok(());
        };
        let mut backup = path.into_os_string();
        backup.push(".bak");
        write_atomic(&PathBuf::from(backup), &old, false)
            .map_err(|e| format!("cannot keep the old file: {e}"))
    }

    fn listen(&self, config: Config) {
        let key = config.key.clone().unwrap_or_default();
        let rules = Rules::new(
            config.listen,
            config.origin.clone(),
            config.allowed_hosts.clone(),
        );
        let shared = Shared::new(
            Arc::new(HostDaemon {
                host: self.host.clone(),
            }),
            key,
            rules,
            self.grant,
            self.hub.clone(),
        );
        let task = tokio::spawn(accept(config.listen, api::router(shared.clone())));
        let mut inner = self.lock();
        if let Some(old) = inner.running.take() {
            old.task.abort();
        }
        inner.problem = None;
        inner.running = Some(Running {
            shared,
            task,
            listen: config.listen,
        });
    }

    /// The first line of `status`, and the rest.
    fn status_text(&self) -> String {
        let inner = self.lock();
        let mut text = String::new();
        match (&inner.running, &inner.problem) {
            (Some(r), _) => text.push_str(&format!(
                "ui-web {} listens on http://{}\n",
                self.version, r.listen
            )),
            (None, Some(why)) => text.push_str(&format!(
                "ui-web {} is not listening: {why}\n",
                self.version
            )),
            (None, None) => text.push_str(&format!("ui-web {} is starting\n", self.version)),
        }
        text.push_str(&format!(
            "grant: level {}, scope {}\n",
            level_name(self.grant.level),
            scope_name(self.grant.scope)
        ));
        text.push_str(&format!("{}\n", what_it_can_do(self.grant)));
        text.push_str(&format!(
            "event streams connected: {}\n",
            self.hub.clients()
        ));
        text.push_str(&format!(
            "configuration: {}\n",
            self.config_path().display()
        ));
        text
    }

    fn new_key(&self) -> Result<String, String> {
        let _rotation = self.rotation.lock().unwrap_or_else(|p| p.into_inner());
        let path = self.config_path();
        let config = match config::load(&path) {
            Err(why) => {
                return Err(format!(
                    "{why}; fix the file first (a new key would not be written over it)"
                ))
            }
            Ok(None) => Config::default(),
            Ok(Some(config)) => config,
        };
        let config = self.give_key(config)?;
        let key = config.key.clone().unwrap_or_default();
        let running = self.lock().running.as_ref().map(|r| r.shared.clone());
        match running {
            // the listener and its settings stay; only the key changes
            Some(shared) => shared.set_key(key.clone()),
            None => self.listen(config),
        }
        // the caller is shown the key right now: it is not "unshown" any more
        let _ = std::fs::remove_file(self.unshown_path());
        Ok(key)
    }
}

/// Bind, trying again until it works, and serve.
async fn accept(addr: SocketAddr, app: axum::Router) {
    let mut said = String::new();
    let listener = loop {
        match TcpListener::bind(addr).await {
            Ok(listener) => break listener,
            Err(e) => {
                let why = bind_error(addr, &e);
                if why != said {
                    eprintln!("ui-web: {why}");
                    said = why;
                }
                tokio::time::sleep(REBIND).await;
            }
        }
    };
    eprintln!("ui-web: listening on {addr}");
    server::serve(listener, app).await;
}

fn bind_error(addr: SocketAddr, e: &std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::PermissionDenied => format!(
            "cannot listen on {addr}: not permitted. The sandbox only lets this adapter bind the ports its manifest lists \
             (127.0.0.1:9615 as shipped); another port needs an adapter built with it listed"
        ),
        std::io::ErrorKind::AddrInUse => format!("cannot listen on {addr}: the address is in use; trying again every {} seconds", REBIND.as_secs()),
        _ => format!("cannot listen on {addr}: {e}; trying again every {} seconds", REBIND.as_secs()),
    }
}

pub fn level_name(level: HostLevel) -> &'static str {
    match level {
        HostLevel::Read => "read",
        HostLevel::Lifecycle => "lifecycle",
        HostLevel::Admin => "admin",
    }
}

pub fn scope_name(scope: HostScope) -> &'static str {
    match scope {
        HostScope::OptedIn => "opted-in",
        HostScope::All => "all",
    }
}

fn what_it_can_do(grant: Grant) -> String {
    let which = match grant.scope {
        HostScope::OptedIn => "the apps that list ui-web under adapters",
        HostScope::All => "every app that is not protected",
    };
    match grant.level {
        HostLevel::Read => "it can show the apps and stream their events; it cannot change anything".to_string(),
        HostLevel::Lifecycle => format!("it can show everything, and restart, stop and reload {which}; it cannot create or delete apps"),
        HostLevel::Admin => format!("it can show everything, change {which}, create apps and delete them"),
    }
}

#[async_trait]
impl Service for Web {
    async fn event(self: Arc<Self>, event: EventParams) {
        if event.family == "lifecycle" {
            self.hub.publish(&event);
        }
    }

    async fn invoke(self: Arc<Self>, call: InvokeParams, out: Output) -> InvokeResult {
        let argv: Vec<&str> = call.argv.iter().map(String::as_str).collect();
        match argv.as_slice() {
            [] | ["status"] => {
                out.stdout(self.status_text());
                match std::fs::remove_file(self.unshown_path()) {
                    // the key was made by this adapter and nobody has seen it yet: this is the one time
                    Ok(()) => match config::load(&self.config_path()) {
                        Ok(Some(Config { key: Some(key), .. })) => {
                            out.stdout(format!("key: {key}\n"));
                            out.stdout("This is the only time the key is shown. It is kept in config.toml (mode 0600); replace it with: oxidepm web new-key\n".to_string());
                            out.stdout("Send it as the X-API-Key header (a browser WebSocket sends it as the subprotocol oxidepm-key.<key>). There is no TLS: put a proxy in front for anything but a trusted network.\n".to_string());
                        }
                        _ => out.stdout("The key is in config.toml.\n".to_string()),
                    },
                    Err(_) => out.stdout("key: kept in config.toml (mode 0600); replace it with: oxidepm web new-key\n".to_string()),
                }
                InvokeResult {
                    exit_code: 0,
                    result: None,
                }
            }
            ["new-key"] => match self.new_key() {
                Ok(key) => {
                    out.stdout(format!("key: {key}\n"));
                    out.stdout("This is the only time the key is shown. The old key no longer works; the previous file is kept as config.toml.bak.\n".to_string());
                    InvokeResult {
                        exit_code: 0,
                        result: None,
                    }
                }
                Err(why) => {
                    out.stderr(format!("ui-web: {why}\n"));
                    InvokeResult {
                        exit_code: 1,
                        result: None,
                    }
                }
            },
            _ => {
                out.stderr("usage: oxidepm web [status | new-key]\n".to_string());
                InvokeResult {
                    exit_code: 2,
                    result: None,
                }
            }
        }
    }

    async fn shutdown(self: Arc<Self>) {
        if let Some(running) = self.lock().running.take() {
            running.task.abort();
        }
    }
}
