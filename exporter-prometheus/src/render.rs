//! The daemon's answer to `Status`, and the Prometheus text exposition format made from it.

use serde::Deserialize;
use serde_json::Value;

/// One app, as far as the metrics go.
#[derive(Debug, Clone, PartialEq)]
pub struct App {
    pub name: String,
    pub id: u64,
    pub mode: String,
    /// There is a child process (starting, running, unhealthy or stopping).
    pub up: bool,
    pub uptime_secs: u64,
    pub restarts: u64,
    pub memory_bytes: u64,
    pub cpu_percent: f64,
}

#[derive(Deserialize)]
struct Info {
    spec: Spec,
    state: State,
}

#[derive(Deserialize)]
struct Spec {
    name: String,
    #[serde(default)]
    id: u64,
    #[serde(default)]
    mode: String,
}

#[derive(Deserialize)]
struct State {
    #[serde(default)]
    status: String,
    #[serde(default)]
    uptime_secs: u64,
    #[serde(default)]
    restarts: u64,
    #[serde(default)]
    memory_bytes: u64,
    #[serde(default)]
    cpu_percent: f64,
}

/// The apps in a `Status` response. A response of any other type, an error in particular, is an
/// `Err` with a sentence: an exporter that cannot tell what the daemon said must not report an
/// empty, healthy-looking list.
pub fn parse_status(response: &Value) -> Result<Vec<App>, String> {
    match response.get("type").and_then(Value::as_str) {
        Some("status") => {}
        Some("error") => {
            let message = response
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("no message");
            let code = response
                .get("code")
                .and_then(Value::as_str)
                .unwrap_or("error");
            return Err(format!(
                "the daemon refused the status request ({code}): {message}"
            ));
        }
        other => {
            return Err(format!(
                "the daemon answered a status request with {other:?}"
            ))
        }
    }
    let list = response
        .get("apps")
        .ok_or("the status answer has no list of apps")?;
    let infos: Vec<Info> = serde_json::from_value(list.clone())
        .map_err(|e| format!("the status answer is not understood: {e}"))?;
    Ok(infos
        .into_iter()
        .map(|i| App {
            up: matches!(
                i.state.status.as_str(),
                "starting" | "running" | "unhealthy" | "stopping"
            ),
            name: i.spec.name,
            id: i.spec.id,
            mode: i.spec.mode,
            uptime_secs: i.state.uptime_secs,
            restarts: i.state.restarts,
            memory_bytes: i.state.memory_bytes,
            cpu_percent: i.state.cpu_percent,
        })
        .collect())
}

/// A label value as the text format wants it: backslash, double quote and line feed escaped.
pub fn escape_label(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out
}

fn float(value: f64) -> String {
    if value.is_nan() {
        "NaN".to_string()
    } else if value.is_infinite() {
        if value > 0.0 { "+Inf" } else { "-Inf" }.to_string()
    } else {
        format!("{value:.2}")
    }
}

fn family(out: &mut String, name: &str, kind: &str, help: &str) {
    out.push_str(&format!("# HELP {name} {help}\n# TYPE {name} {kind}\n"));
}

/// The metrics when the daemon answered. Each family is written in one piece.
pub fn render(apps: &[App]) -> String {
    let mut out = String::new();
    family(
        &mut out,
        "oxidepm_up",
        "gauge",
        "Whether the exporter could read the process list from the daemon (1) or not (0)",
    );
    out.push_str("oxidepm_up 1\n");
    family(
        &mut out,
        "oxidepm_processes_total",
        "gauge",
        "Total number of managed processes",
    );
    out.push_str(&format!("oxidepm_processes_total {}\n", apps.len()));

    type Value = fn(&App) -> String;
    let families: [(&str, &str, &str, Value); 5] = [
        (
            "oxidepm_process_up",
            "gauge",
            "Whether the process is running (1) or not (0)",
            |a| u8::from(a.up).to_string(),
        ),
        (
            "oxidepm_process_uptime_seconds",
            "gauge",
            "Process uptime in seconds",
            |a| a.uptime_secs.to_string(),
        ),
        (
            "oxidepm_process_restarts_total",
            "counter",
            "Total number of process restarts",
            |a| a.restarts.to_string(),
        ),
        (
            "oxidepm_process_memory_bytes",
            "gauge",
            "Process memory usage in bytes",
            |a| a.memory_bytes.to_string(),
        ),
        (
            "oxidepm_process_cpu_percent",
            "gauge",
            "Process CPU usage percentage",
            |a| float(a.cpu_percent),
        ),
    ];
    for (name, kind, help, value) in families {
        family(&mut out, name, kind, help);
        for app in apps {
            out.push_str(&format!(
                "{name}{{name=\"{}\",id=\"{}\",mode=\"{}\"}} {}\n",
                escape_label(&app.name),
                app.id,
                escape_label(&app.mode),
                value(app)
            ));
        }
    }
    out
}

/// The metrics when the daemon could not be asked: only `oxidepm_up 0`, never an empty list that
/// looks like a healthy host with nothing on it.
pub fn render_down() -> String {
    let mut out = String::new();
    family(
        &mut out,
        "oxidepm_up",
        "gauge",
        "Whether the exporter could read the process list from the daemon (1) or not (0)",
    );
    out.push_str("oxidepm_up 0\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn app(name: &str) -> App {
        App {
            name: name.into(),
            id: 3,
            mode: "node".into(),
            up: true,
            uptime_secs: 7,
            restarts: 2,
            memory_bytes: 1024,
            cpu_percent: 1.5,
        }
    }

    #[test]
    fn label_values_are_escaped() {
        assert_eq!(escape_label("plain-name_1"), "plain-name_1");
        assert_eq!(escape_label("a\"b"), "a\\\"b");
        assert_eq!(escape_label("a\\b"), "a\\\\b");
        assert_eq!(escape_label("a\nb"), "a\\nb");
        assert_eq!(escape_label("\\\"\n"), "\\\\\\\"\\n");
        // text outside ASCII is passed through as UTF-8
        assert_eq!(escape_label("caf\u{e9}"), "caf\u{e9}");
    }

    #[test]
    fn a_hostile_name_cannot_break_out_of_its_label() {
        let out = render(&[app("x\"} 1\noxidepm_process_up{name=\"forged")]);
        // one app is one line in each of the five families, plus the two lines of the first two
        // families: nothing the name contains made another line
        let samples: Vec<&str> = out.lines().filter(|l| !l.starts_with('#')).collect();
        assert_eq!(samples.len(), 2 + 5, "{out}");
        assert!(
            !out.lines()
                .any(|l| l.starts_with("oxidepm_process_up{name=\"forged")),
            "{out}"
        );
        for line in &samples[2..] {
            // the three label values are quoted: six quotes that are not escaped
            let bare = line.replace("\\\"", "");
            assert_eq!(bare.matches('"').count(), 6, "{line}");
        }
    }

    #[test]
    fn every_family_is_written_in_one_piece() {
        let out = render(&[app("a"), app("b")]);
        let samples: Vec<&str> = out
            .lines()
            .filter(|l| !l.starts_with('#'))
            .map(|l| l.split(['{', ' ']).next().unwrap())
            .collect();
        let mut seen: Vec<&str> = Vec::new();
        for name in samples {
            if seen.last() != Some(&name) {
                assert!(
                    !seen.contains(&name),
                    "{name} appears in two pieces:\n{out}"
                );
                seen.push(name);
            }
        }
        assert!(out.contains("oxidepm_up 1\n") && out.contains("oxidepm_processes_total 2\n"));
        assert!(
            out.contains("oxidepm_process_restarts_total{name=\"a\",id=\"3\",mode=\"node\"} 2\n")
        );
        assert!(
            out.contains("oxidepm_process_cpu_percent{name=\"b\",id=\"3\",mode=\"node\"} 1.50\n")
        );
        assert!(out.contains("# TYPE oxidepm_process_restarts_total counter\n"));
    }

    #[test]
    fn the_down_answer_has_no_process_list() {
        let out = render_down();
        assert!(out.contains("oxidepm_up 0\n"));
        assert!(!out.contains("oxidepm_processes_total") && !out.contains("oxidepm_process_up"));
    }

    #[test]
    fn non_finite_numbers_use_the_text_formats_spelling() {
        let mut a = app("a");
        a.cpu_percent = f64::NAN;
        assert!(render(&[a.clone()]).contains("} NaN\n"));
        a.cpu_percent = f64::INFINITY;
        assert!(render(&[a]).contains("} +Inf\n"));
    }

    #[test]
    fn a_status_answer_is_read_and_a_child_means_up() {
        let answer = json!({"type": "status", "apps": [
            {"spec": {"name": "web", "id": 1, "mode": "cmd", "command": "x", "other": true}, "state": {"app_id": 1, "status": "running", "restarts": 4, "uptime_secs": 9, "cpu_percent": 2.5, "memory_bytes": 5}},
            {"spec": {"name": "old", "id": 2, "mode": "node"}, "state": {"status": "backoff"}},
            {"spec": {"name": "u", "id": 3, "mode": "cmd"}, "state": {"status": "unhealthy"}},
        ]});
        let apps = parse_status(&answer).unwrap();
        assert_eq!(apps.len(), 3);
        assert_eq!(
            (
                apps[0].name.as_str(),
                apps[0].up,
                apps[0].restarts,
                apps[0].memory_bytes
            ),
            ("web", true, 4, 5)
        );
        assert!(!apps[1].up && apps[2].up);
        assert_eq!(
            parse_status(&json!({"type": "status", "apps": []})).unwrap(),
            vec![]
        );
    }

    #[test]
    fn anything_but_a_status_answer_is_an_error() {
        let e = parse_status(
            &json!({"type": "error", "code": "E_RATE_LIMITED", "message": "slow down"}),
        )
        .unwrap_err();
        assert!(
            e.contains("E_RATE_LIMITED") && e.contains("slow down"),
            "{e}"
        );
        assert!(parse_status(&json!({"type": "pong"})).is_err());
        assert!(parse_status(&json!({"type": "status"})).is_err());
        assert!(parse_status(&json!({"type": "status", "apps": [{"spec": {}}]})).is_err());
        assert!(parse_status(&json!("nonsense")).is_err());
    }
}
