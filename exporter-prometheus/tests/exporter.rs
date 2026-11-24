//! The exporter against the real `oxidepmd` of the core repository: the adapter is packaged, imported
//! by its pinned hash, enabled, and scraped over real TCP while the daemon runs real apps. Each test
//! has a private home and its own port (the manifest the test packages lists that port, because the
//! sandbox lets the adapter bind exactly the ports its manifest lists).

mod support;

use std::time::{Duration, Instant};

use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use serde_json::json;
use support::*;

const KEY: &str = "0123456789abcdef-test-key";

fn config(port: u16, extra: &str) -> String {
    format!("listen = \"127.0.0.1:{port}\"\n{extra}\n")
}

/// An exporter on loopback in a new home.
fn start() -> (Env, u16) {
    let port = free_port();
    let env = Env::new(port, Some(&config(port, "")));
    assert!(
        wait_for(20, || listening(port)),
        "nothing listens on {port}: {}\n{}",
        env.adapter_stderr(),
        env.daemon_stderr()
    );
    (env, port)
}

fn up_value(body: &str, name: &str) -> Option<String> {
    sample(body, "oxidepm_process_up", &format!("name=\"{name}\""))
}

// ---- what is served ----------------------------------------------------------------------------

#[test]
fn metrics_describe_the_apps_of_the_real_daemon() {
    let (env, port) = start();
    let empty = scrape(port);
    assert_eq!(empty.status, 200);
    assert_eq!(
        empty.header("content-type"),
        Some("text/plain; version=0.0.4; charset=utf-8")
    );
    assert_eq!(sample(&empty.body, "oxidepm_up", "").as_deref(), Some("1"));
    assert_eq!(
        sample(&empty.body, "oxidepm_processes_total", "").as_deref(),
        Some("0")
    );
    assert!(
        empty
            .body
            .contains("# TYPE oxidepm_process_restarts_total counter"),
        "{}",
        empty.body
    );

    assert_eq!(env.start_app("web", "sleep 30", false)["type"], "started");
    assert_eq!(
        env.start_app("done-app", "sleep 30", false)["type"],
        "started"
    );
    assert!(wait_for(20, || {
        let body = scrape(port).body;
        sample(&body, "oxidepm_processes_total", "").as_deref() == Some("2")
            && up_value(&body, "web").as_deref() == Some("1")
    }));
    let body = scrape(port).body;
    let line = body
        .lines()
        .find(|l| l.starts_with("oxidepm_process_up{name=\"web\""))
        .expect("a line for web");
    assert!(
        line.contains("mode=\"cmd\"") && line.contains("id=\""),
        "{line}"
    );
    assert!(sample(&body, "oxidepm_process_memory_bytes", "name=\"web\"").is_some());
    assert!(sample(&body, "oxidepm_process_cpu_percent", "name=\"web\"")
        .unwrap()
        .contains('.'));
    assert!(sample(&body, "oxidepm_process_uptime_seconds", "name=\"web\"").is_some());

    // a stopped app is still listed, with up 0
    let _ = env.rpc(json!({ "type": "stop", "selector": { "by": "name", "value": "done-app" } }));
    assert!(
        wait_for(20, || up_value(&scrape(port).body, "done-app").as_deref()
            == Some("0")),
        "{}",
        scrape(port).body
    );
    assert_eq!(up_value(&scrape(port).body, "web").as_deref(), Some("1"));
}

#[test]
fn restarts_are_counted() {
    let (env, port) = start();
    assert_eq!(
        env.start_app("flaky", "sleep 0.2; exit 1", true)["type"],
        "started"
    );
    assert!(
        wait_for(30, || sample(
            &scrape(port).body,
            "oxidepm_process_restarts_total",
            "name=\"flaky\""
        )
        .and_then(|v| v.parse::<u32>().ok())
        .is_some_and(|n| n >= 2)),
        "{}",
        scrape(port).body
    );
}

#[test]
fn only_metrics_is_served() {
    let (_env, port) = start();
    let addr = format!("127.0.0.1:{port}");
    for path in ["/", "/api/health", "/api/processes", "/ws", "/metrics/x"] {
        let r = request(&addr, "GET", path, Some(&addr), &[]).unwrap();
        assert_eq!(r.status, 404, "{path}: {}", r.body);
        assert!(!r.body.contains("oxidepm_"), "{path}");
    }
    for method in ["POST", "DELETE", "PUT"] {
        assert_eq!(
            request(&addr, method, "/metrics", Some(&addr), &[])
                .unwrap()
                .status,
            405,
            "{method}"
        );
    }
    let head = request(&addr, "HEAD", "/metrics", Some(&addr), &[]).unwrap();
    assert_eq!(head.status, 200);
    assert!(head.body.is_empty());
}

#[test]
fn the_exporter_only_reads() {
    let (env, port) = start();
    assert_eq!(env.start_app("web", "sleep 30", false)["type"], "started");
    for _ in 0..5 {
        assert_eq!(scrape(port).status, 200);
    }
    let info = env.adapter_info();
    assert_eq!(info["grant"]["host"]["level"], "read", "{info}");
    // the app is untouched by scraping, and by the exporter coming and going
    let pids = env.app_pids();
    env.restart_adapter();
    assert!(wait_for(20, || listening(port)));
    assert_eq!(env.app_pids(), pids);
}

// ---- never public by accident ------------------------------------------------------------------

#[test]
fn the_shipped_defaults_listen_on_loopback_only() {
    if listening(9616) {
        eprintln!("skipped: something already listens on 9616");
        return;
    }
    let env = Env::new(9616, None);
    assert!(
        wait_for(20, || listening(9616)),
        "{}\n{}",
        env.adapter_stderr(),
        env.daemon_stderr()
    );
    let on = listen_addresses(9616);
    assert!(!on.is_empty());
    assert!(
        on.iter()
            .all(|a| a.ends_with(":0100007F") || a.ends_with(":00000000000000000000000001000000")),
        "only loopback: {on:?}"
    );
    assert_eq!(scrape(9616).status, 200);
}

#[test]
fn a_non_loopback_address_without_a_key_is_not_bound() {
    let port = free_port();
    let env = Env::new(port, Some(&format!("listen = \"0.0.0.0:{port}\"\n")));
    assert!(
        wait_for(20, || env
            .adapter_stderr()
            .contains("refusing to listen on 0.0.0.0")),
        "{}",
        env.adapter_stderr()
    );
    std::thread::sleep(Duration::from_millis(300));
    assert!(!listening(port));
    assert!(
        listen_addresses(port).is_empty(),
        "{:?}",
        listen_addresses(port)
    );
    // the adapter itself is running; it says why it listens nowhere
    assert_eq!(env.adapter_info()["state"], "ready");
    assert!(
        env.adapter_stderr().contains("key"),
        "{}",
        env.adapter_stderr()
    );
}

#[test]
fn a_non_loopback_address_with_a_key_needs_the_key() {
    let port = free_port();
    let env = Env::new(
        port,
        Some(&format!("listen = \"0.0.0.0:{port}\"\nkey = \"{KEY}\"\n")),
    );
    assert!(wait_for(20, || listening(port)), "{}", env.adapter_stderr());
    let addr = format!("127.0.0.1:{port}");
    let none = request(&addr, "GET", "/metrics", Some(&addr), &[]).unwrap();
    assert_eq!(none.status, 401);
    assert_eq!(none.header("www-authenticate"), Some("Bearer"));
    assert!(!none.body.contains("oxidepm_"));
    for wrong in [
        "Bearer nope",
        &format!("Bearer {KEY}x"),
        &format!("Bearer {}", &KEY[..KEY.len() - 1]),
        KEY,
        "Bearer",
    ] {
        assert_eq!(
            request(
                &addr,
                "GET",
                "/metrics",
                Some(&addr),
                &[("Authorization", wrong)]
            )
            .unwrap()
            .status,
            401,
            "{wrong}"
        );
    }
    let ok = request(
        &addr,
        "GET",
        "/metrics",
        Some(&addr),
        &[("Authorization", &format!("Bearer {KEY}"))],
    )
    .unwrap();
    assert_eq!(ok.status, 200);
    assert!(ok.body.contains("oxidepm_up 1"));
    // the key is the only thing that matters on this address: any Host header is accepted with it
    let named = request(
        &addr,
        "GET",
        "/metrics",
        Some("metrics.example.org"),
        &[("Authorization", &format!("Bearer {KEY}"))],
    )
    .unwrap();
    assert_eq!(named.status, 200);
    // and the key is nowhere in the adapter's log or the daemon's
    assert!(!env.adapter_stderr().contains(KEY) && !env.daemon_stderr().contains(KEY));
}

#[test]
fn loopback_with_a_key_needs_the_key() {
    let port = free_port();
    let env = Env::new(port, Some(&config(port, &format!("key = \"{KEY}\""))));
    assert!(wait_for(20, || listening(port)), "{}", env.adapter_stderr());
    assert_eq!(scrape(port).status, 401);
    let addr = format!("127.0.0.1:{port}");
    assert_eq!(
        request(
            &addr,
            "GET",
            "/metrics",
            Some(&addr),
            &[("Authorization", &format!("Bearer {KEY}"))]
        )
        .unwrap()
        .status,
        200
    );
}

#[test]
fn rebound_host_header_is_refused() {
    let (_env, port) = start();
    let addr = format!("127.0.0.1:{port}");
    for host in [
        "evil.example",
        &format!("evil.example:{port}"),
        &format!("127.0.0.1.evil.example:{port}"),
        "127.0.0.1:1",
        &format!("10.1.2.3:{port}"),
    ] {
        let r = request(&addr, "GET", "/metrics", Some(host), &[]).unwrap();
        assert_eq!(r.status, 403, "{host}");
        assert!(!r.body.contains("oxidepm_"), "{host}");
    }
    for host in [
        format!("localhost:{port}"),
        format!("127.0.0.1:{port}"),
        format!("[::1]:{port}"),
    ] {
        assert_eq!(
            request(&addr, "GET", "/metrics", Some(&host), &[])
                .unwrap()
                .status,
            200,
            "{host}"
        );
    }
}

#[test]
fn a_broken_configuration_binds_nothing_and_says_why() {
    let port = free_port();
    let env = Env::new(
        port,
        Some(&format!("listen = \"127.0.0.1:{port}\"\nkee = \"x\"\n")),
    );
    assert!(
        wait_for(20, || env.adapter_stderr().contains("nothing is listening")),
        "{}",
        env.adapter_stderr()
    );
    assert!(
        env.adapter_stderr().contains("unknown field"),
        "{}",
        env.adapter_stderr()
    );
    assert!(!listening(port));
    assert_eq!(env.adapter_info()["state"], "ready");
    // fixing the file and restarting the adapter brings it up
    env.write_config(&config(port, ""));
    env.restart_adapter();
    assert!(wait_for(20, || listening(port)), "{}", env.adapter_stderr());
}

#[test]
fn a_port_the_manifest_does_not_list_is_not_bound_under_the_sandbox() {
    let listed = free_port();
    let other = free_port();
    let mut env = Env::bare(&[]);
    let sandbox = env.rpc(json!({ "type": "daemon_status" }));
    let abi = sandbox["sandbox"]["landlock_abi"].as_u64().unwrap_or(0);
    if abi < 4 {
        eprintln!("skipped: Landlock ABI {abi} cannot limit TCP bind");
        return;
    }
    env.write_config(&config(other, ""));
    env.install(listed);
    assert!(
        wait_for(20, || env.adapter_stderr().contains("not permitted")),
        "{}",
        env.adapter_stderr()
    );
    assert!(!listening(other) && !listening(listed));
}

// ---- when the daemon cannot be asked -----------------------------------------------------------

/// Continues a stopped daemon even when the test fails on the way.
struct Frozen(i32);

impl Frozen {
    fn new(pid: i32) -> Frozen {
        kill(Pid::from_raw(pid), Signal::SIGSTOP).unwrap();
        Frozen(pid)
    }
}

impl Drop for Frozen {
    fn drop(&mut self) {
        let _ = kill(Pid::from_raw(self.0), Signal::SIGCONT);
    }
}

#[test]
fn a_daemon_that_does_not_answer_gives_503_and_up_0() {
    let (env, port) = start();
    assert_eq!(env.start_app("web", "sleep 30", false)["type"], "started");
    assert!(wait_for(20, || up_value(&scrape(port).body, "web")
        .as_deref()
        == Some("1")));
    std::thread::sleep(Duration::from_millis(1200));
    let frozen = Frozen::new(env.daemon_pid());
    let began = Instant::now();
    let r = scrape(port);
    assert_eq!(r.status, 503, "{}", r.body);
    assert!(r.body.contains("oxidepm_up 0"), "{}", r.body);
    assert!(
        !r.body.contains("oxidepm_process") && !r.body.contains("web"),
        "no list that looks like a healthy host: {}",
        r.body
    );
    assert!(
        began.elapsed() < Duration::from_secs(15),
        "the scrape waits a bounded time"
    );
    drop(frozen);
    // the daemon copies the adapter's stderr to its log file, so the line shows up once it is running again
    assert!(
        wait_for(20, || env.adapter_stderr().contains("a scrape got 503")),
        "{}",
        env.adapter_stderr()
    );
    // the daemon is back: the next scrape (after the one second a failure is remembered) is good again
    assert!(wait_for(30, || {
        let r = scrape(port);
        r.status == 200 && up_value(&r.body, "web").as_deref() == Some("1")
    }));
}

#[test]
fn a_burst_of_scrapes_does_not_get_the_adapter_struck() {
    let (env, port) = start();
    assert_eq!(env.start_app("web", "sleep 30", false)["type"], "started");
    let threads: Vec<_> = (0..8)
        .map(|_| {
            std::thread::spawn(move || {
                let mut bad = 0;
                for _ in 0..60 {
                    let r = scrape(port);
                    if r.status != 200 || !r.body.contains("oxidepm_up 1") {
                        bad += 1;
                    }
                }
                bad
            })
        })
        .collect();
    let bad: i32 = threads.into_iter().map(|t| t.join().unwrap()).sum();
    // more than the 50 requests a second the daemon allows an adapter, if every scrape asked it
    assert_eq!(
        bad,
        0,
        "480 scrapes, none refused: {}",
        env.adapter_stderr()
    );
    assert_eq!(
        env.adapter_info()["state"],
        "ready",
        "{}",
        env.adapter_info()
    );
    assert!(
        env.daemon_stderr().matches("strike").count() == 0,
        "{}",
        env.daemon_stderr()
    );
}

// ---- stopping ----------------------------------------------------------------------------------

#[test]
fn the_port_is_released_when_the_adapter_or_the_daemon_stops() {
    let (mut env, port) = start();
    assert_eq!(scrape(port).status, 200);
    let off = env.cli(&["adapter", "disable", ADAPTER]);
    assert_eq!(off.code, 0, "{}", off.all());
    assert!(
        wait_for(20, || !listening(port)),
        "still listening after disable"
    );
    assert!(env.adapter_pids().is_empty());
    env.restart_adapter();
    assert!(wait_for(20, || listening(port)));
    env.shutdown_daemon();
    assert!(
        wait_for(20, || !listening(port)),
        "still listening after the daemon stopped"
    );
    assert!(wait_for(10, || env.adapter_pids().is_empty()));
}
