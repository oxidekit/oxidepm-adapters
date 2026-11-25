//! The notifier against the real `oxidepmd` of the core repository: the adapter is packaged,
//! imported by its pinned hash, enabled, and driven by the daemon's own lifecycle events. The
//! messaging services are played by a local HTTP server. Each test has a private home.

mod support;

use std::time::{Duration, Instant};

use serde_json::{json, Value};
use support::*;

const TOKEN: &str = "123456:SECRETTOKEN-abcdef";
const CHAT: &str = "-100777";

fn telegram_config(mock: &Mock, extra: &str) -> String {
    format!(
        "{extra}\n[telegram]\nbot_token = \"{TOKEN}\"\nchat_id = \"{CHAT}\"\napi_base = \"{}\"\n",
        mock.base()
    )
}

fn texts(mock: &Mock) -> Vec<String> {
    mock.requests_to("/bot")
        .iter()
        .map(|r| r.json()["text"].as_str().unwrap_or("").to_string())
        .collect()
}

fn count_with(mock: &Mock, needle: &str) -> usize {
    texts(mock).iter().filter(|t| t.contains(needle)).count()
}

// ---- the path from the daemon's events to a message --------------------------------------------

#[cfg(target_os = "linux")]
#[test]
fn bundled_notifier_sends_a_lifecycle_webhook() {
    use oxidepm_runtime_testkit::{Adapter, Env as BundleEnv};

    let mock = Mock::start();
    let adapter = Adapter::new(
        env!("CARGO_MANIFEST_DIR"),
        env!("CARGO_BIN_EXE_oxidepm-notifier"),
    );
    let mut env = BundleEnv::with_bundle(&[&adapter], &[]);
    let state = env.home.join("adapters/state/notifier");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(
        state.join("config.toml"),
        format!("[webhook]\nurl = \"{}\"\n", mock.url("/bundled")),
    )
    .unwrap();
    env.install_bundled("notifier");
    let reply = env.start("bundled-app", "cmd", "/bin/sleep", &["30"], &env.project());
    assert_eq!(reply["type"], "started", "{reply}");
    assert!(
        wait_for(20, || !mock.requests_to("/bundled").is_empty()),
        "{:?}\n{}",
        mock.requests(),
        env.daemon_stderr()
    );
    let body = mock.requests_to("/bundled")[0].json();
    assert_eq!(body["version"], 1, "{body}");
    assert_eq!(body["event"], "start", "{body}");
    assert_eq!(body["process"], "bundled-app", "{body}");
}

#[test]
fn lifecycle_events_reach_telegram_through_the_real_daemon() {
    let mock = Mock::start();
    let env = Env::new(&[]);
    env.write_config(&telegram_config(&mock, ""));
    assert_eq!(env.start_app("web", "sleep 30", false)["type"], "started");
    assert!(
        wait_for(20, || count_with(&mock, "Started") == 1),
        "{:?}",
        texts(&mock)
    );
    env.stop_all();
    assert!(
        wait_for(20, || count_with(&mock, "Stopped") == 1),
        "{:?}",
        texts(&mock)
    );
    let first = &mock.requests_to("/bot")[0];
    assert_eq!(first.path, format!("/bot{TOKEN}/sendMessage"));
    assert_eq!(first.json()["chat_id"], CHAT);
    assert!(
        first.json()["text"]
            .as_str()
            .unwrap()
            .contains("<code>web</code>"),
        "{first:?}"
    );
    assert_eq!(first.json()["parse_mode"], "HTML");
}

#[test]
fn stopped_notifications_are_delivered_when_the_daemon_shuts_down() {
    let mock = Mock::start();
    let mut env = Env::new(&[]);
    env.write_config(&telegram_config(&mock, ""));
    assert_eq!(env.start_app("web", "sleep 30", false)["type"], "started");
    assert!(wait_for(20, || count_with(&mock, "Started") == 1));
    // a SIGTERM stops the apps, and the notifier is still there to hear about it
    env.shutdown_daemon();
    assert!(
        wait_for(10, || count_with(&mock, "Stopped") == 1),
        "{:?}",
        texts(&mock)
    );
}

#[test]
fn events_and_severity_filters_apply() {
    let mock = Mock::start();
    let env = Env::new(&[]);
    env.write_config(&telegram_config(
        &mock,
        "events = [\"crash\"]\nmin_severity = \"critical\"",
    ));
    assert_eq!(env.start_app("quiet", "sleep 30", false)["type"], "started");
    assert_eq!(env.start_app("loud", "exit 3", false)["type"], "started");
    // the daemon reports a crash that is not restarted twice: `crashed`, then `errored`, and the
    // messages arrive in that order
    assert!(
        wait_for(20, || count_with(&mock, "Crashed") == 2),
        "{:?}",
        texts(&mock)
    );
    std::thread::sleep(Duration::from_millis(500));
    let all = texts(&mock);
    assert!(
        all.iter()
            .all(|t| t.contains("Crashed") && t.contains("loud")),
        "no start event, and nothing about the quiet app: {all:?}"
    );
    assert!(
        all[0].contains("Exit code 3") && all[1].contains("not restarted"),
        "{all:?}"
    );
}

#[test]
fn critical_events_are_not_rate_limited() {
    let mock = Mock::start();
    let env = Env::new(&[]);
    env.write_config(&telegram_config(&mock, "events = [\"crash\"]"));
    // one app that crashes over and over: every crash is delivered, the sixth and seventh included
    assert_eq!(env.start_app("flaky", "exit 1", true)["type"], "started");
    assert!(
        wait_for(40, || count_with(&mock, "Crashed") >= 7),
        "{} crash messages: {:?}",
        count_with(&mock, "Crashed"),
        texts(&mock)
    );
    assert_eq!(count_with(&mock, "rate limited"), 0);
}

#[test]
fn other_events_are_rate_limited_with_one_notice() {
    let mock = Mock::start();
    let env = Env::new(&[]);
    env.write_config(&telegram_config(&mock, "events = [\"restart\"]"));
    assert_eq!(env.start_app("web", "sleep 60", false)["type"], "started");
    for _ in 0..8 {
        let reply =
            env.rpc(json!({ "type": "restart", "selector": { "by": "name", "value": "web" } }));
        assert_eq!(reply["type"], "restarted", "{reply}");
    }
    assert!(
        wait_for(20, || count_with(&mock, "rate limited") == 1),
        "{:?}",
        texts(&mock)
    );
    std::thread::sleep(Duration::from_millis(700));
    assert_eq!(count_with(&mock, "Restarted"), 5, "{:?}", texts(&mock));
    assert_eq!(
        count_with(&mock, "rate limited"),
        1,
        "one notice, then silence: {:?}",
        texts(&mock)
    );
}

// ---- the test verb ------------------------------------------------------------------

#[test]
fn test_reports_every_channel_and_exits_non_zero_when_one_fails() {
    let mock = Mock::start();
    let env = Env::new(&[]);
    mock.respond_with(|req| {
        if req.path.starts_with("/hook") {
            Reply::status(500, "boom")
        } else {
            Reply::ok()
        }
    });
    env.write_config(&format!(
        "min_severity = \"critical\"\nevents = [\"crash\"]\n[telegram]\nbot_token = \"{TOKEN}\"\nchat_id = \"{CHAT}\"\napi_base = \"{}\"\n[webhook]\nurl = \"{}\"\n",
        mock.base(),
        mock.url("/hook/PRIVATEPATH")
    ));
    let run = env.cli(&["notify", "test"]);
    assert_ne!(run.code, 0, "{}", run.all());
    assert!(run.stdout.contains("telegram: ok"), "{}", run.all());
    assert!(
        run.stdout
            .contains("webhook: FAILED (the webhook answered HTTP 500)"),
        "{}",
        run.all()
    );
    // the filters (events = crash, min_severity = critical) did not hold the test back
    assert_eq!(mock.requests_to("/bot").len(), 1);
    assert_eq!(mock.requests_to("/hook").len(), 1);
    assert!(
        !run.all().contains("PRIVATEPATH") && !run.all().contains("SECRETTOKEN"),
        "{}",
        run.all()
    );

    // and when everything works, it exits zero
    mock.respond_with(|_| Reply::ok());
    let run = env.cli(&["notify", "test"]);
    assert_eq!(run.code, 0, "{}", run.all());
    assert!(
        run.stdout.contains("telegram: ok") && run.stdout.contains("webhook: ok"),
        "{}",
        run.all()
    );
}

#[test]
fn test_without_a_channel_fails() {
    let env = Env::new(&[]);
    let run = env.cli(&["notify", "test"]);
    assert_eq!(run.code, 1, "{}", run.all());
    assert!(
        run.stderr.contains("No channel is configured"),
        "{}",
        run.all()
    );
}

// ---- a configuration that is broken is shown, not replaced -----------------

#[test]
fn a_broken_configuration_is_reported_and_never_overwritten() {
    let mock = Mock::start();
    let env = Env::new(&[]);
    let broken = format!(
        "[telegram\nchat_id = \"{CHAT}\"\n[discord]\nwebhook_url = \"{}\"\n",
        mock.url("/d/DISCORDSECRET")
    );
    env.write_config(&broken);

    // every verb that would change it refuses and leaves the bytes alone
    for args in [
        vec!["notify", "events", "--set", "crash"],
        vec!["notify", "severity", "critical"],
        vec!["notify", "telegram", "--chat", "1"],
        vec!["notify", "remove", "telegram"],
    ] {
        let run = env.cli(&args);
        assert_eq!(run.code, 1, "{args:?}: {}", run.all());
        assert!(run.all().contains("nothing was changed"), "{}", run.all());
        assert!(
            run.all().contains("line 1"),
            "the error names the line: {}",
            run.all()
        );
        assert!(
            !run.all().contains("DISCORDSECRET"),
            "the file is not quoted: {}",
            run.all()
        );
        assert_eq!(
            std::fs::read_to_string(env.config_path()).unwrap(),
            broken,
            "{args:?}"
        );
    }
    assert!(!support::backup_path(&env.config_path()).exists());

    // status says so, with a failure exit
    let status = env.cli(&["notify", "status"]);
    assert_eq!(status.code, 1, "{}", status.all());
    assert!(status.stdout.contains("NOT USABLE"), "{}", status.all());

    // events are not sent with the defaults; the adapter says why on its own log
    assert_eq!(env.start_app("web", "sleep 30", false)["type"], "started");
    assert!(
        wait_for(20, || env
            .adapter_stderr()
            .contains("the configuration is not used")),
        "{}",
        env.adapter_stderr()
    );
    assert!(mock.requests().is_empty());
    assert!(!env.adapter_stderr().contains("DISCORDSECRET"));

    // the person fixes the file by hand: it takes effect without a restart
    env.write_config(&telegram_config(&mock, ""));
    assert_eq!(env.start_app("web2", "sleep 30", false)["type"], "started");
    assert!(
        wait_for(20, || count_with(&mock, "web2") == 1),
        "{:?}",
        texts(&mock)
    );
}

#[test]
fn values_are_validated_when_the_file_is_read() {
    let env = Env::new(&[]);
    env.write_config("events = [\"crash\", \"explode\"]\nmin_severity = \"loud\"\n[webhook]\nurl = \"gopher://x\"\n");
    let status = env.cli(&["notify", "status"]);
    assert_eq!(status.code, 1, "{}", status.all());
    for needle in [
        "'explode' is not an event type",
        "'loud' is not a severity",
        "webhook: url must be an http or https URL",
    ] {
        assert!(status.stdout.contains(needle), "{needle}: {}", status.all());
    }
}

#[test]
fn the_events_help_comes_from_the_list_of_valid_events() {
    let env = Env::new(&[]);
    let run = env.cli(&["notify", "events"]);
    assert_eq!(run.code, 0, "{}", run.all());
    for event in [
        "start",
        "stop",
        "crash",
        "crash_loop",
        "restart",
        "memory_limit",
        "health_check",
    ] {
        assert!(run.stdout.contains(event), "{event}: {}", run.all());
    }
    let bad = env.cli(&["notify", "events", "--set", "crash,bogus"]);
    assert_eq!(bad.code, 1, "{}", bad.all());
    assert!(
        bad.all().contains("'bogus' is not an event type"),
        "{}",
        bad.all()
    );
    assert!(!env.config_path().exists(), "nothing was written");
}

// ---- setting up and changing the file ---------------------------------------

#[test]
fn a_token_on_the_command_line_is_refused() {
    let env = Env::new(&[]);
    let run = env.cli(&[
        "notify",
        "telegram",
        "--token",
        "123:SHELLHISTORY",
        "--chat",
        "1",
    ]);
    assert_eq!(run.code, 1, "{}", run.all());
    assert!(run.stderr.contains("command line"), "{}", run.all());
    assert!(!env.config_path().exists());
    assert!(
        !run.all().contains("SHELLHISTORY"),
        "the token is not echoed back: {}",
        run.all()
    );
}

#[test]
fn setup_changes_are_written_atomically_owner_only_with_a_backup() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new(&[]);
    let run = env.cli(&["notify", "telegram", "--chat", "424242"]);
    assert_eq!(run.code, 0, "{}", run.all());
    assert!(run.stdout.contains("no bot token yet"), "{}", run.all());
    let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&env.config_path()), 0o600);
    let first = std::fs::read_to_string(env.config_path()).unwrap();
    assert!(
        first.contains("chat_id = \"424242\"")
            && first.contains("bot_token_env = \"OXIDEPM_TELEGRAM_TOKEN\""),
        "{first}"
    );
    assert!(
        !first.contains("bot_token ="),
        "an empty token is not written: {first}"
    );

    assert_eq!(
        env.cli(&["notify", "events", "--set", "crash, restart"])
            .code,
        0
    );
    assert_eq!(env.cli(&["notify", "severity", "warning"]).code, 0);
    let backup = support::backup_path(&env.config_path());
    assert_eq!(mode(&backup), 0o600);
    let now = std::fs::read_to_string(env.config_path()).unwrap();
    assert!(
        now.contains("events = [\"crash\", \"restart\"]")
            && now.contains("min_severity = \"warning\""),
        "{now}"
    );
    assert!(
        std::fs::read_to_string(&backup).unwrap().contains("events"),
        "the backup is the version before the last change"
    );
    assert_eq!(
        std::fs::read_dir(env.state_dir())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .count(),
        0
    );

    let status = env.cli(&["notify", "status"]);
    assert_eq!(status.code, 0, "{}", status.all());
    assert!(
        status.stdout.contains("Chat ID: 424242")
            && status.stdout.contains("Events: crash, restart")
            && status.stdout.contains("Minimum severity: warning"),
        "{}",
        status.all()
    );

    assert_eq!(env.cli(&["notify", "remove", "telegram"]).code, 0);
    assert!(!std::fs::read_to_string(env.config_path())
        .unwrap()
        .contains("telegram"));
    assert_eq!(env.cli(&["notify", "severity", "none"]).code, 0);
    assert_eq!(
        env.cli(&["notify", "severity", "loud"]).code,
        2,
        "usage errors exit 2"
    );
}

#[test]
fn the_bot_token_can_come_from_the_daemons_environment() {
    let mock = Mock::start();
    let env = Env::new(&[("OXIDEPM_TELEGRAM_TOKEN", "999:FROMENVIRONMENT")]);
    env.write_config(&format!("[telegram]\nbot_token_env = \"OXIDEPM_TELEGRAM_TOKEN\"\nchat_id = \"{CHAT}\"\napi_base = \"{}\"\n", mock.base()));
    let status = env.cli(&["notify", "status"]);
    assert!(
        status.stdout.contains("from OXIDEPM_TELEGRAM_TOKEN"),
        "{}",
        status.all()
    );
    assert!(
        !status.stdout.contains("FROMENVIRONMENT"),
        "the token is masked: {}",
        status.all()
    );
    assert_eq!(env.start_app("web", "sleep 30", false)["type"], "started");
    assert!(
        wait_for(20, || mock
            .requests_to("/bot999:FROMENVIRONMENT/sendMessage")
            .len()
            == 1),
        "{:?}",
        mock.requests()
    );
}

// ---- no secret leaves the adapter ---------------------------------------------------

#[test]
fn a_failed_send_never_puts_a_secret_in_a_log_an_error_or_the_audit_trail() {
    let env = Env::new(&[]);
    // nothing listens on port 1: every send fails to connect, with the secret in the URL
    env.write_config(&format!(
        "[telegram]\nbot_token = \"{TOKEN}\"\nchat_id = \"{CHAT}\"\napi_base = \"http://127.0.0.1:1\"\n[webhook]\nurl = \"http://127.0.0.1:1/hook/WEBHOOKSECRETPATH\"\nsecret = \"HMACSECRETVALUE\"\n[slack]\nwebhook_url = \"http://127.0.0.1:1/services/SLACKSECRETPATH\"\n"
    ));
    let run = env.cli(&["notify", "test"]);
    assert_eq!(run.code, 1, "{}", run.all());
    assert!(
        run.stdout.contains("telegram: FAILED (could not connect)"),
        "{}",
        run.all()
    );
    assert_eq!(env.start_app("web", "sleep 30", false)["type"], "started");
    assert!(
        wait_for(20, || env
            .adapter_stderr()
            .matches("failed: could not connect")
            .count()
            >= 3),
        "{}",
        env.adapter_stderr()
    );
    assert!(
        env.adapter_stderr()
            .contains("channel=telegram host=127.0.0.1"),
        "channel and host are logged: {}",
        env.adapter_stderr()
    );

    env.stop_all();
    let mut everything =
        run.all() + &env.adapter_stderr() + &env.daemon_stderr() + &env.adapter_info().to_string();
    for dir in ["audit", "logs"] {
        if let Ok(entries) = std::fs::read_dir(env.home.join(dir)) {
            for e in entries.flatten() {
                everything.push_str(&std::fs::read_to_string(e.path()).unwrap_or_default());
            }
        }
    }
    for secret in [
        "SECRETTOKEN",
        "WEBHOOKSECRETPATH",
        "HMACSECRETVALUE",
        "SLACKSECRETPATH",
    ] {
        assert!(
            !everything.contains(secret),
            "{secret} leaked:\n{everything}"
        );
    }
}

// ---- delivery -----------------------------------------------------------------------

#[test]
fn a_rejected_formatting_is_sent_again_as_plain_text() {
    let mock = Mock::start();
    let env = Env::new(&[]);
    // the first try has a parse_mode and is refused; the second has none
    mock.respond_with(|req| {
        if req.json().get("parse_mode").is_some() {
            Reply::status(
                400,
                "{\"ok\":false,\"description\":\"Bad Request: can't parse entities\"}",
            )
        } else {
            Reply::ok()
        }
    });
    env.write_config(&telegram_config(&mock, "events = [\"crash\"]"));
    // the app name carries underscores, which Markdown cannot take
    assert_eq!(env.start_app("a_b_c", "exit 2", false)["type"], "started");
    // (the daemon reports this crash twice, `crashed` and `errored`; each is tried, then retried)
    assert!(
        wait_for(20, || mock.requests_to("/bot").len() == 4),
        "{:?}",
        mock.requests()
    );
    let bodies: Vec<Value> = mock.requests().iter().map(Req::json).collect();
    assert_eq!(
        bodies.iter().filter(|b| b["parse_mode"] == "HTML").count(),
        2,
        "{bodies:?}"
    );
    let plain: Vec<&Value> = bodies
        .iter()
        .filter(|b| b.get("parse_mode").is_none())
        .collect();
    assert_eq!(plain.len(), 2, "{bodies:?}");
    assert!(
        plain
            .iter()
            .all(|b| b["text"].as_str().unwrap().contains("a_b_c")
                && !b["text"].as_str().unwrap().contains("<code>")),
        "{plain:?}"
    );
    assert!(
        !env.adapter_stderr().contains("failed"),
        "the retry worked, so nothing failed: {}",
        env.adapter_stderr()
    );
}

#[test]
fn a_stuck_channel_does_not_delay_the_others() {
    let mock = Mock::start();
    let env = Env::new(&[]);
    mock.respond_with(|req| {
        if req.path.starts_with("/slow") {
            Reply::ok().after(Duration::from_secs(8))
        } else {
            Reply::ok()
        }
    });
    env.write_config(&format!(
        "timeout_secs = 2\n[webhook]\nurl = \"{}\"\n[telegram]\nbot_token = \"{TOKEN}\"\nchat_id = \"{CHAT}\"\napi_base = \"{}\"\n",
        mock.url("/slow"),
        mock.base()
    ));
    let began = Instant::now();
    assert_eq!(env.start_app("web", "sleep 30", false)["type"], "started");
    assert!(
        wait_for(10, || count_with(&mock, "Started") == 1),
        "{:?}",
        texts(&mock)
    );
    assert!(
        began.elapsed() < Duration::from_millis(1800),
        "Telegram waited for the stuck webhook: {:?}",
        began.elapsed()
    );
    // the stuck one is given up at the time limit and said so on the log
    assert!(
        wait_for(10, || env.adapter_stderr().contains("channel=webhook")
            && env.adapter_stderr().contains("timed out")),
        "{}",
        env.adapter_stderr()
    );
}

#[test]
fn the_webhook_is_signed_and_every_message_has_the_same_schema() {
    let mock = Mock::start();
    let env = Env::new(&[]);
    env.write_config(&format!(
        "[webhook]\nurl = \"{}\"\nsecret = \"s3cret-value\"\n",
        mock.url("/wh")
    ));
    assert_eq!(env.cli(&["notify", "test"]).code, 0);
    assert_eq!(env.start_app("web", "sleep 30", false)["type"], "started");
    assert!(
        wait_for(20, || mock.requests_to("/wh").len() == 2),
        "{:?}",
        mock.requests()
    );
    let mut schemas = Vec::new();
    for req in mock.requests_to("/wh") {
        let timestamp = req
            .header("x-oxidepm-timestamp")
            .expect("a timestamp header");
        let signature = req
            .header("x-oxidepm-signature")
            .expect("a signature header");
        assert_eq!(
            signature,
            expected_signature("s3cret-value", timestamp, &req.body),
            "{req:?}"
        );
        assert!((chrono::Utc::now().timestamp() - timestamp.parse::<i64>().unwrap()).abs() < 60);
        assert!(
            req.header("x-webhook-secret").is_none(),
            "the secret itself is not sent"
        );
        let body: Value = req.json();
        let mut keys: Vec<String> = body.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        schemas.push(keys);
        assert_eq!(body["version"], 1);
    }
    assert_eq!(
        schemas[0], schemas[1],
        "the test message and an event have one schema"
    );
    assert_eq!(
        schemas[0],
        vec![
            "event",
            "id",
            "message",
            "process",
            "severity",
            "timestamp",
            "version"
        ]
    );

    // without a secret nothing is signed
    env.write_config(&format!("[webhook]\nurl = \"{}\"\n", mock.url("/plain")));
    assert_eq!(env.cli(&["notify", "test"]).code, 0);
    let plain = &mock.requests_to("/plain")[0];
    assert!(
        plain.header("x-oxidepm-signature").is_none()
            && plain.header("x-oxidepm-timestamp").is_none()
    );
}

fn expected_signature(secret: &str, timestamp: &str, body: &[u8]) -> String {
    use hmac::{Hmac, Mac};
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(timestamp.as_bytes());
    mac.update(b".");
    mac.update(body);
    format!(
        "v1={}",
        mac.finalize()
            .into_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    )
}

#[test]
fn discord_and_slack_get_their_own_payloads() {
    let mock = Mock::start();
    let env = Env::new(&[]);
    env.write_config(&format!(
        "[discord]\nwebhook_url = \"{}\"\n[slack]\nwebhook_url = \"{}\"\n",
        mock.url("/discord"),
        mock.url("/slack")
    ));
    assert_eq!(env.cli(&["notify", "test"]).code, 0);
    assert!(mock.requests_to("/discord")[0].json()["content"]
        .as_str()
        .unwrap()
        .contains("test message"));
    assert!(mock.requests_to("/slack")[0].json()["text"]
        .as_str()
        .unwrap()
        .contains("test message"));
}

#[test]
fn the_adapter_can_resolve_localhost_inside_its_sandbox() {
    let mock = Mock::start();
    let env = Env::new(&[]);
    env.write_config(&format!("[telegram]\nbot_token = \"{TOKEN}\"\nchat_id = \"{CHAT}\"\napi_base = \"http://localhost:{}\"\n", mock.port));
    assert_eq!(env.start_app("web", "sleep 30", false)["type"], "started");
    assert!(
        wait_for(20, || count_with(&mock, "Started") == 1),
        "{:?} {}",
        texts(&mock),
        env.adapter_stderr()
    );
}

// ---- the adapter and the daemon ----------------------------------------------------------------

#[test]
fn the_adapter_never_touches_the_apps_and_a_dead_adapter_does_not_either() {
    let mock = Mock::start();
    let env = Env::new(&[]);
    env.write_config(&telegram_config(&mock, ""));
    assert_eq!(env.start_app("web", "sleep 60", false)["type"], "started");
    let pids = env.app_pids();
    assert_eq!(pids.len(), 1);
    // kill the adapter process: the daemon restarts it and the app is where it was
    let pid = env
        .adapter_pids()
        .first()
        .copied()
        .expect("the adapter is running");
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid),
        nix::sys::signal::Signal::SIGKILL,
    )
    .unwrap();
    assert!(
        wait_for(30, || env.adapter_pids().iter().any(|p| *p != pid)
            && env.adapter_info()["state"] == "ready"),
        "{} {:?}",
        env.adapter_info(),
        env.adapter_pids()
    );
    assert_eq!(env.app_pids(), pids);
    // and it works again
    assert_eq!(env.start_app("web2", "sleep 30", false)["type"], "started");
    assert!(
        wait_for(20, || count_with(&mock, "web2") == 1),
        "{:?}",
        texts(&mock)
    );
}
