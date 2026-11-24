//! A daemon that answers from a script, and apps to put in its list.

use std::sync::Mutex;

use async_trait::async_trait;
use oxidepm_core::{AppInfo, AppSpec, AppStatus, RunState, Runtime};
use serde_json::{json, Value};

use crate::daemon::{Daemon, Fail};

type Handler = Box<dyn Fn(&Value) -> Result<Value, Fail> + Send + Sync>;

pub struct Fake {
    pub calls: Mutex<Vec<Value>>,
    handler: Handler,
}

impl Fake {
    pub fn new(handler: impl Fn(&Value) -> Result<Value, Fail> + Send + Sync + 'static) -> Fake {
        Fake {
            calls: Mutex::new(Vec::new()),
            handler: Box::new(handler),
        }
    }

    /// Answers `status` with `apps`, `logs` with `lines`, and a stop or restart with the
    /// daemon's answer for success.
    pub fn serving(apps: Vec<AppInfo>, lines: Vec<&'static str>) -> Fake {
        Fake::new(move |request| {
            Ok(match request["type"].as_str().unwrap_or("") {
                "status" => json!({ "type": "status", "apps": apps }),
                "logs" => json!({ "type": "log_lines", "lines": lines }),
                "stop" => json!({ "type": "stopped", "count": 1, "items": [] }),
                "restart" => json!({ "type": "restarted", "count": 1, "items": [] }),
                other => {
                    json!({ "type": "error", "code": "E_INTERNAL", "message": format!("unexpected {other}") })
                }
            })
        })
    }

    pub fn types(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .map(|c| c["type"].as_str().unwrap_or("").to_string())
            .collect()
    }

    pub fn count(&self, kind: &str) -> usize {
        self.types().iter().filter(|t| *t == kind).count()
    }

    pub fn last(&self, kind: &str) -> Option<Value> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|c| c["type"] == kind)
            .cloned()
    }
}

#[async_trait]
impl Daemon for Fake {
    async fn call(&self, request: Value) -> Result<Value, Fail> {
        self.calls.lock().unwrap().push(request.clone());
        (self.handler)(&request)
    }
}

pub fn app(id: u32, name: &str) -> AppInfo {
    let mut spec = AppSpec::new(
        name.to_string(),
        Runtime::CMD,
        "sleep".to_string(),
        std::path::PathBuf::from("/tmp"),
    );
    spec.id = id;
    let mut state = RunState::new(id);
    state.status = AppStatus::Running;
    state.pid = Some(1000 + id);
    AppInfo::new(spec, state)
}

pub fn key(code: crossterm::event::KeyCode) -> crossterm::event::KeyEvent {
    crossterm::event::KeyEvent::new(code, crossterm::event::KeyModifiers::NONE)
}

pub fn ch(c: char) -> crossterm::event::KeyEvent {
    key(crossterm::event::KeyCode::Char(c))
}
