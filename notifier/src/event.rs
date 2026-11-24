//! What the daemon's lifecycle events turn into: an alert with a severity, an event type that the
//! `events` filter names, and the text of the message.

use oxidepm_adapter_proto::messages::EventParams;
use serde_json::Value;

/// The event types a filter may name. The help of the `events` verb is made from this list.
pub const VALID_EVENTS: &[&str] = &[
    "start",
    "stop",
    "crash",
    "crash_loop",
    "restart",
    "memory_limit",
    "health_check",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Info,
    Warning,
    Critical,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Info => "info",
            Severity::Warning => "warning",
            Severity::Critical => "critical",
        }
    }
}

impl std::str::FromStr for Severity {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, ()> {
        match s.to_lowercase().as_str() {
            "info" => Ok(Severity::Info),
            "warning" | "warn" => Ok(Severity::Warning),
            "critical" | "crit" => Ok(Severity::Critical),
            _ => Err(()),
        }
    }
}

/// One thing to tell the person.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alert {
    /// One of [`VALID_EVENTS`], or `test` or `notice` for the messages the adapter makes itself.
    pub event: &'static str,
    pub severity: Severity,
    pub name: String,
    pub id: u32,
    pub plain: String,
    /// The same text for Telegram's HTML mode: every dynamic part escaped.
    pub html: String,
}

impl Alert {
    /// Critical alerts are never held back by the rate limit.
    pub fn is_critical(&self) -> bool {
        self.severity == Severity::Critical
    }

    /// A message the adapter makes itself (a test, a notice); it has no app.
    pub fn own(event: &'static str, text: &str) -> Alert {
        Alert {
            event,
            severity: Severity::Info,
            name: String::new(),
            id: 0,
            plain: text.to_string(),
            html: escape_html(text),
        }
    }
}

pub fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            c => out.push(c),
        }
    }
    out
}

fn ordinal(n: u64) -> String {
    match n {
        1 => "1st".to_string(),
        2 => "2nd".to_string(),
        3 => "3rd".to_string(),
        n => format!("{n}th"),
    }
}

fn number(data: &serde_json::Map<String, Value>, key: &str) -> Option<i64> {
    data.get(key).and_then(Value::as_i64)
}

fn text<'a>(data: &'a serde_json::Map<String, Value>, key: &str) -> Option<&'a str> {
    data.get(key).and_then(Value::as_str)
}

/// The alert for a `lifecycle` event, if the event is one the person hears about.
pub fn alert_for(ev: &EventParams) -> Option<Alert> {
    if ev.family != "lifecycle" {
        return None;
    }
    let app = ev.app.as_ref()?;
    let (name, id, data) = (app.name.as_str(), app.id, &ev.data);
    // (event type, severity, plain detail line after the headline, emoji + headline)
    let (event, severity, head, detail) = match ev.kind.as_str() {
        "started" => ("start", Severity::Info, "\u{1F7E2} Started", None),
        "stopped" => (
            "stop",
            Severity::Info,
            "\u{26AA} Stopped",
            number(data, "exit_code").map(|c| format!("Exit code {c}")),
        ),
        "restarted" => {
            let n = number(data, "restarts").unwrap_or(0).max(0) as u64;
            (
                "restart",
                Severity::Warning,
                "\u{1F504} Restarted",
                Some(format!("{} restart", ordinal(n))),
            )
        }
        "crashed" => {
            let error = match (number(data, "exit_code"), number(data, "exit_signal")) {
                (Some(code), _) => format!("Exit code {code}"),
                (None, Some(sig)) => format!("Killed by signal {sig}"),
                (None, None) => "Unknown error".to_string(),
            };
            (
                "crash",
                Severity::Critical,
                "\u{1F534} Crashed",
                Some(format!("Error: {error}")),
            )
        }
        "errored" => (
            "crash",
            Severity::Critical,
            "\u{1F534} Crashed",
            Some(format!(
                "Error: {}",
                text(data, "reason").unwrap_or("errored")
            )),
        ),
        "crash_loop" => (
            "crash_loop",
            Severity::Critical,
            "\u{1F501} Crash loop",
            Some(
                text(data, "reason")
                    .map_or_else(|| "it keeps crashing".to_string(), str::to_string),
            ),
        ),
        "unhealthy" => (
            "health_check",
            Severity::Critical,
            "\u{1F6A8} Health check failed",
            Some(
                text(data, "reason")
                    .map_or_else(|| "the health check failed".to_string(), str::to_string),
            ),
        ),
        "memory_limit" => {
            let used = number(data, "memory_mb")?;
            let limit = number(data, "limit_mb")?;
            (
                "memory_limit",
                Severity::Warning,
                "\u{26A0}\u{FE0F} Memory limit",
                Some(format!("Using {used}MB / {limit}MB limit")),
            )
        }
        _ => return None,
    };
    let plain = match &detail {
        Some(d) => format!("{head}: {name} (id: {id})\n{d}"),
        None => format!("{head}: {name} (id: {id})"),
    };
    let html = match &detail {
        Some(d) => format!(
            "{}: <code>{}</code> (id: {id})\n{}",
            escape_html(head),
            escape_html(name),
            escape_html(d)
        ),
        None => format!(
            "{}: <code>{}</code> (id: {id})",
            escape_html(head),
            escape_html(name)
        ),
    };
    Some(Alert {
        event,
        severity,
        name: name.to_string(),
        id,
        plain,
        html,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxidepm_adapter_proto::messages::EventApp;
    use serde_json::json;

    fn event(kind: &str, data: Value) -> EventParams {
        EventParams {
            seq: 1,
            kind: kind.into(),
            family: "lifecycle".into(),
            at: "2026-01-01T00:00:00Z".into(),
            app: Some(EventApp {
                id: 4,
                name: "a_b*c".into(),
                tags: vec![],
            }),
            data: data.as_object().cloned().unwrap_or_default(),
            options: None,
            dropped_before: None,
        }
    }

    #[test]
    fn lifecycle_events_map_to_event_types_and_severities() {
        let cases = [
            ("started", json!({}), "start", Severity::Info),
            ("stopped", json!({"exit_code": 0}), "stop", Severity::Info),
            (
                "restarted",
                json!({"restarts": 2}),
                "restart",
                Severity::Warning,
            ),
            (
                "crashed",
                json!({"exit_code": 1}),
                "crash",
                Severity::Critical,
            ),
            (
                "errored",
                json!({"reason": "bad"}),
                "crash",
                Severity::Critical,
            ),
            ("crash_loop", json!({}), "crash_loop", Severity::Critical),
            ("unhealthy", json!({}), "health_check", Severity::Critical),
            (
                "memory_limit",
                json!({"memory_mb": 512, "limit_mb": 256}),
                "memory_limit",
                Severity::Warning,
            ),
        ];
        for (kind, data, event_type, severity) in cases {
            let alert = alert_for(&event(kind, data)).unwrap_or_else(|| panic!("{kind} maps"));
            assert_eq!(
                (alert.event, alert.severity),
                (event_type, severity),
                "{kind}"
            );
            assert!(VALID_EVENTS.contains(&alert.event));
        }
        for kind in [
            "recovered",
            "backoff",
            "cron_fired",
            "uptime_limit",
            "surprise",
        ] {
            assert!(alert_for(&event(kind, json!({}))).is_none(), "{kind}");
        }
        // no figures, no message; another family is not ours to announce
        assert!(alert_for(&event("memory_limit", json!({}))).is_none());
        let mut adapter_event = event("started", json!({}));
        adapter_event.family = "adapter".into();
        assert!(alert_for(&adapter_event).is_none());
    }

    #[test]
    fn the_crash_text_follows_the_exit_code_then_the_signal() {
        assert!(alert_for(&event("crashed", json!({"exit_code": 3})))
            .unwrap()
            .plain
            .contains("Exit code 3"));
        assert!(alert_for(&event("crashed", json!({"exit_signal": 9})))
            .unwrap()
            .plain
            .contains("Killed by signal 9"));
        assert!(alert_for(&event("crashed", json!({})))
            .unwrap()
            .plain
            .contains("Unknown error"));
        assert!(alert_for(&event("restarted", json!({"restarts": 3})))
            .unwrap()
            .plain
            .contains("3rd restart"));
        assert!(alert_for(&event("restarted", json!({"restarts": 4})))
            .unwrap()
            .plain
            .contains("4th restart"));
    }

    #[test]
    fn the_html_text_escapes_what_is_dynamic() {
        let mut ev = event("crashed", json!({}));
        ev.app.as_mut().unwrap().name = "<b>x</b> & y".into();
        let alert = alert_for(&ev).unwrap();
        assert!(
            alert
                .html
                .contains("<code>&lt;b&gt;x&lt;/b&gt; &amp; y</code>"),
            "{}",
            alert.html
        );
        assert!(
            alert.plain.contains("<b>x</b> & y"),
            "the plain text is as it is"
        );
        let errored = alert_for(&event("errored", json!({"reason": "a_b*c <oops>"}))).unwrap();
        assert!(
            errored.html.contains("a_b*c &lt;oops&gt;"),
            "{}",
            errored.html
        );
    }

    #[test]
    fn severity_parses_the_old_spellings() {
        assert_eq!("WARN".parse::<Severity>(), Ok(Severity::Warning));
        assert_eq!("crit".parse::<Severity>(), Ok(Severity::Critical));
        assert!("loud".parse::<Severity>().is_err());
        assert!(Severity::Info < Severity::Warning && Severity::Warning < Severity::Critical);
    }
}
