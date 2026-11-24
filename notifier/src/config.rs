//! `config.toml` in the adapter's state directory.
//!
//! A file that does not parse or does not validate is an error that is shown to the person, never
//! a reason to start from the defaults: the file is not replaced, and while it is broken nothing is
//! sent. Saving keeps the previous version as `config.toml.bak` and writes through
//! `oxidepm_core::fsutil::write_atomic` (a 0600 temporary file and a rename).

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::event::{Severity, VALID_EVENTS};

/// The environment variable the manifest passes to the adapter.
pub const TOKEN_ENV: &str = "OXIDEPM_TELEGRAM_TOKEN";

pub const CHANNELS: &[&str] = &["telegram", "discord", "slack", "webhook"];

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telegram: Option<Telegram>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discord: Option<Hook>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slack: Option<Hook>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook: Option<Webhook>,
    /// Event types to send (empty: all).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<String>,
    /// The lowest severity to send: info, warning or critical.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_severity: Option<String>,
    /// How long one request to a channel may take, 1 to 60 seconds (default 10).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Telegram {
    /// Prefer `bot_token_env`: the token then is not in a file.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub bot_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bot_token_env: Option<String>,
    pub chat_id: String,
    /// The Bot API server, for a self-hosted one (default `https://api.telegram.org`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_base: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hook {
    pub webhook_url: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Webhook {
    pub url: String,
    /// Signs every request: HMAC-SHA256 over `timestamp.body`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
}

impl Telegram {
    /// The token: the environment variable when it is set and not empty, else `bot_token`.
    pub fn resolve_token(&self) -> Option<String> {
        if let Some(name) = &self.bot_token_env {
            if let Ok(value) = std::env::var(name) {
                if !value.is_empty() {
                    return Some(value);
                }
            }
        }
        (!self.bot_token.is_empty()).then(|| self.bot_token.clone())
    }
}

impl Config {
    pub fn min_severity(&self) -> Option<Severity> {
        self.min_severity.as_deref().and_then(|s| s.parse().ok())
    }

    pub fn timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(
            self.timeout_secs
                .unwrap_or(crate::channels::DEFAULT_TIMEOUT_SECS),
        )
    }

    /// Everything wrong with the values, in one message; `Ok` if nothing is. Checked when the file
    /// is loaded and before it is saved.
    pub fn validate(&self) -> Result<(), String> {
        let mut problems = Vec::new();
        for event in &self.events {
            if !VALID_EVENTS.contains(&event.as_str()) {
                problems.push(format!(
                    "events: '{}' is not an event type (valid: {})",
                    event.escape_debug(),
                    VALID_EVENTS.join(", ")
                ));
            }
        }
        if let Some(level) = &self.min_severity {
            if level.parse::<Severity>().is_err() {
                problems.push(format!(
                    "min_severity: '{}' is not a severity (info, warning or critical)",
                    level.escape_debug()
                ));
            }
        }
        if let Some(secs) = self.timeout_secs {
            if !(1..=60).contains(&secs) {
                problems.push("timeout_secs: must be between 1 and 60".to_string());
            }
        }
        if let Some(t) = &self.telegram {
            if t.chat_id.trim().is_empty() {
                problems.push("telegram: chat_id is empty".to_string());
            }
            if let Some(name) = &t.bot_token_env {
                if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                {
                    problems.push(
                        "telegram: bot_token_env is not an environment variable name".to_string(),
                    );
                }
            }
            if let Some(base) = &t.api_base {
                if let Err(why) = check_url(base) {
                    problems.push(format!("telegram: api_base {why}"));
                }
            }
        }
        for (channel, hook) in [("discord", &self.discord), ("slack", &self.slack)] {
            if let Some(h) = hook {
                if let Err(why) = check_url(&h.webhook_url) {
                    problems.push(format!("{channel}: webhook_url {why}"));
                }
            }
        }
        if let Some(w) = &self.webhook {
            if let Err(why) = check_url(&w.url) {
                problems.push(format!("webhook: url {why}"));
            }
            if w.secret.as_deref() == Some("") {
                problems.push(
                    "webhook: secret is empty (leave it out for an unsigned webhook)".to_string(),
                );
            }
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(problems.join("; "))
        }
    }
}

/// A URL is never put in a message: only what is wrong with it.
fn check_url(text: &str) -> Result<(), &'static str> {
    match url::Url::parse(text) {
        Ok(u) if matches!(u.scheme(), "http" | "https") && u.host_str().is_some() => Ok(()),
        Ok(_) => Err("must be an http or https URL with a host"),
        Err(_) => Err("is not a URL"),
    }
}

/// Parse a file's text. The error names the line and the problem and never quotes the file (it
/// holds tokens).
pub fn parse(text: &str) -> Result<Config, String> {
    let config: Config = toml::from_str(text).map_err(|e| {
        let (line, col) = e.span().map(|s| line_col(text, s.start)).unwrap_or((0, 0));
        if line > 0 {
            format!("line {line}, column {col}: {}", e.message())
        } else {
            e.message().to_string()
        }
    })?;
    config.validate()?;
    Ok(config)
}

fn line_col(text: &str, offset: usize) -> (usize, usize) {
    let before = &text[..offset.min(text.len())];
    let line = before.matches('\n').count() + 1;
    let col = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
    (line, col)
}

/// What the file looked like when it was read, to notice an edit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stamp {
    modified: Option<SystemTime>,
    len: u64,
}

pub fn stamp(path: &Path) -> Option<Stamp> {
    let meta = std::fs::metadata(path).ok()?;
    Some(Stamp {
        modified: meta.modified().ok(),
        len: meta.len(),
    })
}

/// Read `path`. A missing file is an empty configuration; any other failure is an error.
pub fn load(path: &Path) -> Result<Config, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse(&text).map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(format!("{}: cannot be read ({})", path.display(), e.kind())),
    }
}

pub fn backup_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".bak");
    path.with_file_name(name)
}

/// Save `config`: validated, the previous file kept as `.bak`, written atomically with mode 0600.
pub fn save(path: &Path, config: &Config) -> Result<(), String> {
    config.validate()?;
    let text =
        toml::to_string(config).map_err(|e| format!("cannot write the configuration: {}", e))?;
    if let Ok(previous) = std::fs::read(path) {
        oxidepm_core::fsutil::write_atomic(&backup_path(path), &previous, true)
            .map_err(|e| format!("cannot keep the backup ({})", e.kind()))?;
    }
    oxidepm_core::fsutil::write_atomic(path, text.as_bytes(), true)
        .map_err(|e| format!("{}: cannot be written ({})", path.display(), e.kind()))
}

/// Show only the edges of a long secret; a short one is hidden completely. Works on characters,
/// so non-ASCII input cannot panic.
pub fn mask(secret: &str) -> String {
    let chars: Vec<char> = secret.chars().collect();
    if chars.len() < 16 {
        return "*".repeat(8);
    }
    let head: String = chars[..4].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}...{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_never_reveals_short_secrets_and_is_char_safe() {
        assert_eq!(mask(""), "********");
        assert_eq!(mask("abcde"), "********");
        assert_eq!(mask("ééééé"), "********");
        assert_eq!(mask("123456789:ABCdefGHIjklMNO"), "1234...lMNO");
        assert_eq!(mask("ééééééééééééééééé"), "éééé...éééé");
    }

    #[test]
    fn a_parse_error_names_the_line_and_never_quotes_the_file() {
        let text = "events = []\n[telegram]\nbot_token = \"123456:SECRET\nchat_id = \"1\"\n";
        let err = parse(text).unwrap_err();
        assert!(err.starts_with("line 3"), "{err}");
        assert!(!err.contains("SECRET"), "{err}");
    }

    #[test]
    fn values_are_validated_at_load() {
        let err =
            parse("events = [\"crash\", \"explode\"]\nmin_severity = \"loud\"\n").unwrap_err();
        assert!(
            err.contains("'explode' is not an event type")
                && err.contains("'loud' is not a severity"),
            "{err}"
        );
        assert!(
            err.contains("crash, ") || err.contains("valid: start"),
            "the valid list is shown: {err}"
        );
        let err = parse("[webhook]\nurl = \"ftp://x/y\"\n").unwrap_err();
        assert_eq!(err, "webhook: url must be an http or https URL with a host");
        let err = parse("[discord]\nwebhook_url = \"not a url SECRET\"\n").unwrap_err();
        assert!(!err.contains("SECRET"), "a URL is not repeated: {err}");
        assert!(parse("surprise = 1\n")
            .unwrap_err()
            .contains("unknown field"));
        assert_eq!(parse("").unwrap(), Config::default());
    }

    #[test]
    fn save_keeps_a_backup_and_writes_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("config.toml");
        let mut config = Config {
            events: vec!["crash".into()],
            ..Config::default()
        };
        save(&path, &config).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(
            !backup_path(&path).exists(),
            "nothing to keep the first time"
        );
        let first = std::fs::read(&path).unwrap();
        config.events = vec!["stop".into()];
        save(&path, &config).unwrap();
        assert_eq!(std::fs::read(backup_path(&path)).unwrap(), first);
        assert_eq!(load(&path).unwrap(), config);
        assert_eq!(
            std::fs::metadata(backup_path(&path))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        // an invalid configuration is never written
        config.events = vec!["nope".into()];
        assert!(save(&path, &config).is_err());
        assert_eq!(load(&path).unwrap().events, vec!["stop".to_string()]);
    }

    #[test]
    fn token_comes_from_the_environment_first() {
        let t = Telegram {
            bot_token: "from-file".into(),
            bot_token_env: Some("OXIDEPM_NOTIFIER_TEST_TOKEN_A".into()),
            chat_id: "1".into(),
            api_base: None,
        };
        assert_eq!(t.resolve_token().as_deref(), Some("from-file"));
        std::env::set_var("OXIDEPM_NOTIFIER_TEST_TOKEN_A", "from-env");
        assert_eq!(t.resolve_token().as_deref(), Some("from-env"));
        let none = Telegram {
            bot_token: String::new(),
            bot_token_env: Some("OXIDEPM_NOTIFIER_TEST_TOKEN_UNSET".into()),
            chat_id: "1".into(),
            api_base: None,
        };
        assert_eq!(none.resolve_token(), None);
    }
}
