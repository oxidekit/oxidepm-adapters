use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;
use std::time::Duration;

pub const DEFAULT_IGNORE_PATTERNS: &[&str] = &[
    "target",
    "node_modules",
    ".git",
    ".oxidepm",
    "*.swp",
    "*.swo",
    "*~",
    ".DS_Store",
    "__pycache__",
    "*.pyc",
    "*.pyo",
    "*.log",
    "venv",
    ".venv",
    "dist",
    "build",
    ".next",
    ".cache",
    "coverage",
];

#[derive(Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct WatchOptions {
    pub paths: Vec<PathBuf>,
    pub ignore: Vec<String>,
    debounce_ms: u64,
}

impl Default for WatchOptions {
    fn default() -> Self {
        Self {
            paths: vec![".".into()],
            ignore: Vec::new(),
            debounce_ms: 200,
        }
    }
}

impl WatchOptions {
    /// The status reply preserves the option names and hides their values.
    pub fn from_redacted(redacted: &Value) -> serde_json::Result<Self> {
        let defaults = serde_json::to_value(Self::default())?;
        let mut options = redacted.clone();
        if let Some(fields) = options.as_object_mut() {
            for (name, value) in fields {
                if let Some(default) = defaults.get(name) {
                    *value = default.clone();
                }
            }
        }
        serde_json::from_value(options)
    }

    pub fn debounce(&self) -> Duration {
        Duration::from_millis(self.debounce_ms.clamp(10, 60_000))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_watch_options_are_rejected() {
        let error = serde_json::from_value::<WatchOptions>(serde_json::json!({"debouce_ms": 50}))
            .err()
            .expect("unknown option was accepted");
        assert!(error.to_string().contains("unknown field"));
    }
}
