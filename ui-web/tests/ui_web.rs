//! The web adapter against the real `oxidepmd`: a private home per test, the adapter built here,
//! packaged, imported by its pinned hash and enabled, then driven over HTTP and WebSocket and
//! with the daemon's own requests. The sandbox is the real one, so the ports the manifest lists
//! are the ones that can be bound; each test lists its own free port in its copy of the manifest.

mod support;

use std::io::Read;
use std::time::Duration;

use serde_json::{json, Value};
use support::*;

fn start() -> (Env, u16) {
    let port = free_port();
    let env = Env::new(port, "");
    assert!(
        wait_for(20, || listening(port)),
        "the adapter does not listen: {}\n{}",
        env.adapter_stderr(),
        env.daemon_stderr()
    );
    (env, port)
}

fn app_pid(env: &Env, name: &str) -> Option<u64> {
    let status = env.rpc(json!({ "type": "status" }));
    status["apps"]
        .as_array()?
        .iter()
        .find(|a| a["spec"]["name"] == name)?["state"]["pid"]
        .as_u64()
}

fn app_count(env: &Env) -> usize {
    env.rpc(json!({ "type": "status" }))["apps"]
        .as_array()
        .map_or(0, Vec::len)
}

/// The adapter's own files and logs, the daemon's logs and the audit trail, as one text.
fn everything_written(env: &Env) -> String {
    let mut text = env.adapter_stderr() + &env.daemon_stderr() + &env.adapter_info().to_string();
    for dir in ["audit", "logs"] {
        let Ok(entries) = std::fs::read_dir(env.home.join(dir)) else {
            continue;
        };
        for entry in entries.flatten() {
            if let Ok(mut file) = std::fs::File::open(entry.path()) {
                let mut s = String::new();
                let _ = file.read_to_string(&mut s);
                text.push_str(&s);
            }
        }
    }
    text
}

// ---- who may ask -------------------------------------------------------------------------------

#[test]
fn loopback_without_key_is_refused() {
    let (env, port) = start();
    assert_eq!(
        env.start_app("secret-app-name", "sleep 60", false)["type"],
        "started"
    );
    let before = app_count(&env);
    for (method, path, body) in [
        ("GET", "/api/processes", ""),
        ("GET", "/api/processes/secret-app-name", ""),
        ("GET", "/api/processes/secret-app-name/logs", ""),
        (
            "POST",
            "/api/processes",
            r#"{"name":"x","command":"true","cwd":"/tmp"}"#,
        ),
        ("POST", "/api/processes/secret-app-name/stop", ""),
        ("POST", "/api/processes/secret-app-name/restart", ""),
        ("DELETE", "/api/processes/all", ""),
        ("POST", "/api/save", ""),
        ("POST", "/api/resurrect", ""),
        ("GET", "/api/ping", ""),
    ] {
        for headers in [
            vec![],
            vec![("X-API-Key", "wrong-key-wrong-key-wrong")],
            vec![("X-API-Key", "")],
        ] {
            let r = api_with(port, method, path, &headers, body);
            assert_eq!(r.status, 401, "{method} {path} {headers:?}: {}", r.body);
            assert!(!r.body.contains("secret-app-name"), "{}", r.body);
        }
    }
    assert!(matches!(Ws::connect(port, &[]), Err(WsError::Refused(401))));
    assert_eq!(app_count(&env), before, "nothing was created");
    assert!(
        app_pid(&env, "secret-app-name").is_some(),
        "and the app was not touched"
    );
    // with the key all of it works
    let r = api(port, "GET", "/api/processes", "");
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.json()["data"][0]["spec"]["name"], "secret-app-name");
}

#[test]
fn rebound_host_header_is_refused() {
    let (_env, port) = start();
    let addr = format!("127.0.0.1:{port}");
    let key = [("X-API-Key", KEY)];
    // the page's own name, pointed at 127.0.0.1 by its DNS, arrives as the Host header
    for host in [
        format!("evil.example:{port}"),
        "evil.example".to_string(),
        format!("127.0.0.1:{}", port.wrapping_add(1)),
        format!("localhost.evil.example:{port}"),
        "127.0.0.1".to_string(),
    ] {
        for (method, path) in [
            ("GET", "/api/processes"),
            ("POST", "/api/processes/x/restart"),
            ("GET", "/api/health"),
        ] {
            let r = request_with(&addr, method, path, Some(&host), &key, "").unwrap();
            assert_eq!(
                r.status, 403,
                "{method} {path} with Host {host}: {}",
                r.body
            );
        }
    }
    // no Host at all
    assert_eq!(
        request_with(&addr, "GET", "/api/health", None, &key, "")
            .unwrap()
            .status,
        403
    );
    // an upgrade too
    assert!(matches!(
        Ws::connect(
            port,
            &[
                ("Host", &format!("evil.example:{port}")),
                ("X-API-Key", KEY)
            ]
        ),
        Err(WsError::Refused(403))
    ));
    // the names that are the same place are fine
    for host in [addr.clone(), format!("localhost:{port}")] {
        assert_eq!(
            request_with(&addr, "GET", "/api/processes", Some(&host), &key, "")
                .unwrap()
                .status,
            200,
            "{host}"
        );
    }
}

#[test]
fn cross_origin_websocket_upgrade_is_refused() {
    let (_env, port) = start();
    for origin in [
        "http://evil.example",
        "null",
        "http://127.0.0.1",
        &format!("http://127.0.0.1:{port}"),
    ] {
        let refused = Ws::connect(port, &[("X-API-Key", KEY), ("Origin", origin)]);
        assert!(
            matches!(refused, Err(WsError::Refused(403))),
            "Origin {origin}: no stream"
        );
    }
    // a client that is not a browser sends none
    let mut ws = match Ws::connect(port, &[("X-API-Key", KEY)]) {
        Ok(ws) => ws,
        Err(WsError::Refused(code)) => panic!("refused with {code}"),
        Err(WsError::Other(e)) => panic!("{e}"),
    };
    assert_eq!(ws.next_json(10).unwrap()["type"], "hello");
}

#[test]
fn state_changing_requests_from_another_origin_are_refused() {
    let (env, port) = start();
    assert_eq!(env.start_app("web", "sleep 60", false)["type"], "started");
    let pid = app_pid(&env, "web");
    for (method, path) in [
        ("POST", "/api/processes/web/restart"),
        ("POST", "/api/processes/web/stop"),
        ("DELETE", "/api/processes/web"),
        ("POST", "/api/save"),
    ] {
        let r = api_with(
            port,
            method,
            path,
            &[("X-API-Key", KEY), ("Origin", "http://evil.example")],
            "",
        );
        assert_eq!(r.status, 403, "{method} {path}: {}", r.body);
        // and no CORS header that would let that page read the answer
        assert!(r.header("access-control-allow-origin").is_none());
    }
    assert_eq!(app_pid(&env, "web"), pid, "the app was not touched");
    // the same request without an Origin goes through
    assert_eq!(
        api(port, "POST", "/api/processes/web/restart", "").status,
        200
    );
}

#[test]
fn the_configured_origin_may_call_and_stream() {
    let port = free_port();
    let env = Env::new(port, "origin = \"http://localhost:3000\"\n");
    assert!(
        wait_for(20, || listening(port)),
        "the adapter does not listen: {}\n{}",
        env.adapter_stderr(),
        env.daemon_stderr()
    );
    let from_page = [("X-API-Key", KEY), ("Origin", "http://localhost:3000")];
    let r = api_with(port, "GET", "/api/processes", &from_page, "");
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(
        r.header("access-control-allow-origin"),
        Some("http://localhost:3000"),
        "{:?}",
        r.headers
    );
    // the preflight carries no key
    let addr = format!("127.0.0.1:{port}");
    let pre = request_with(
        &addr,
        "OPTIONS",
        "/api/processes/x/restart",
        Some(&addr),
        &[
            ("Origin", "http://localhost:3000"),
            ("Access-Control-Request-Method", "POST"),
        ],
        "",
    )
    .unwrap();
    assert_eq!(pre.status, 204, "{}", pre.body);
    assert_eq!(
        pre.header("access-control-allow-origin"),
        Some("http://localhost:3000"),
        "{:?}",
        pre.headers
    );
    let other = request_with(
        &addr,
        "OPTIONS",
        "/api/processes/x/restart",
        Some(&addr),
        &[("Origin", "http://evil.example")],
        "",
    )
    .unwrap();
    assert_eq!(other.status, 403, "{}", other.body);
    match Ws::connect(port, &from_page) {
        Ok(mut ws) => assert_eq!(
            ws.next_json(10).map(|m| m["type"].clone()),
            Some(json!("hello")),
            "the stream starts with a hello"
        ),
        Err(WsError::Refused(code)) => panic!(
            "the configured origin was refused with {code}: {}",
            env.adapter_stderr()
        ),
        Err(WsError::Other(e)) => panic!("the stream did not open: {e}: {}", env.adapter_stderr()),
    }
    let foreign = Ws::connect(
        port,
        &[("X-API-Key", KEY), ("Origin", "http://localhost:3001")],
    );
    assert!(
        matches!(foreign, Err(WsError::Refused(403))),
        "another origin gets no stream"
    );
}

#[test]
fn allowed_hosts_let_a_proxy_name_through() {
    let port = free_port();
    let env = Env::new(
        port,
        &format!("allowed_hosts = [\"pm.example.org\", \"pm.example.org:{port}\"]\n"),
    );
    assert!(wait_for(20, || listening(port)), "{}", env.adapter_stderr());
    let addr = format!("127.0.0.1:{port}");
    for host in ["pm.example.org", "PM.example.org"] {
        assert_eq!(
            request_with(
                &addr,
                "GET",
                "/api/processes",
                Some(host),
                &[("X-API-Key", KEY)],
                ""
            )
            .unwrap()
            .status,
            200,
            "{host}"
        );
    }
    // the key is still needed behind a proxy
    assert_eq!(
        request_with(
            &addr,
            "GET",
            "/api/processes",
            Some("pm.example.org"),
            &[],
            ""
        )
        .unwrap()
        .status,
        401
    );
    assert_eq!(
        request_with(
            &addr,
            "GET",
            "/api/processes",
            Some("other.example.org"),
            &[("X-API-Key", KEY)],
            ""
        )
        .unwrap()
        .status,
        403
    );
}

// ---- the key ------------------------------------------------------------------------------------

#[test]
fn the_first_start_makes_a_private_key_and_the_verb_shows_it_once() {
    let port = free_port();
    // settings and no key: the adapter adds one, keeps the settings and the old file
    let env = Env::with(
        port,
        Some(&format!("listen = \"127.0.0.1:{port}\"\n")),
        false,
    );
    assert!(wait_for(20, || listening(port)), "{}", env.adapter_stderr());
    let config = std::fs::read_to_string(env.config_path()).unwrap();
    let key = config
        .lines()
        .find_map(|l| l.strip_prefix("key = \""))
        .and_then(|l| l.strip_suffix('"'))
        .expect("a key in config.toml")
        .to_string();
    assert_eq!(key.len(), 64, "{config}");
    assert!(config.contains(&format!("listen = \"127.0.0.1:{port}\"")));
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(env.config_path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    assert!(backup_path(&env.config_path()).exists());
    // not yet shown: the adapter's log says where, not what
    assert!(!everything_written(&env).contains(&key));

    let first = env.cli(&["web"]);
    assert_eq!(first.code, 0, "{}", first.all());
    assert!(
        first.stdout.contains(&format!("key: {key}")),
        "{}",
        first.all()
    );
    assert!(
        first
            .stdout
            .contains(&format!("listens on http://127.0.0.1:{port}")),
        "{}",
        first.stdout
    );
    assert!(
        first.stdout.contains("level lifecycle, scope opted-in"),
        "{}",
        first.stdout
    );
    let second = env.cli(&["web", "status"]);
    assert_eq!(second.code, 0);
    assert!(
        !second.all().contains(&key),
        "the key is shown once: {}",
        second.all()
    );
    assert!(second.stdout.contains("new-key"), "{}", second.stdout);

    // the key works, the test key does not
    let addr = format!("127.0.0.1:{port}");
    assert_eq!(
        request_with(
            &addr,
            "GET",
            "/api/processes",
            Some(&addr),
            &[("X-API-Key", &key)],
            ""
        )
        .unwrap()
        .status,
        200
    );
    assert_eq!(api(port, "GET", "/api/processes", "").status, 401);
    // and it is in no log, no audit line and no `adapter info`
    assert!(!everything_written(&env).contains(&key));
}

#[test]
fn new_key_replaces_the_key_at_once_and_prints_it_once() {
    let (env, port) = start();
    assert_eq!(api(port, "GET", "/api/processes", "").status, 200);
    let run = env.cli(&["web", "new-key"]);
    assert_eq!(run.code, 0, "{}", run.all());
    let new = run
        .stdout
        .lines()
        .find_map(|l| l.strip_prefix("key: "))
        .expect("the new key is printed")
        .to_string();
    assert_ne!(new, KEY);
    assert_eq!(
        api(port, "GET", "/api/processes", "").status,
        401,
        "the old key is dead"
    );
    let addr = format!("127.0.0.1:{port}");
    assert_eq!(
        request_with(
            &addr,
            "GET",
            "/api/processes",
            Some(&addr),
            &[("X-API-Key", &new)],
            ""
        )
        .unwrap()
        .status,
        200
    );
    // the old file is kept
    assert!(std::fs::read_to_string(backup_path(&env.config_path()))
        .unwrap()
        .contains(KEY));
    assert!(std::fs::read_to_string(env.config_path())
        .unwrap()
        .contains(&new));
    // survives a restart of the adapter
    env.restart_adapter(false);
    assert!(wait_for(20, || listening(port)));
    assert_eq!(
        request_with(
            &addr,
            "GET",
            "/api/processes",
            Some(&addr),
            &[("X-API-Key", &new)],
            ""
        )
        .unwrap()
        .status,
        200
    );
    // never shown again
    let status = env.cli(&["web"]);
    assert!(!status.all().contains(&new), "{}", status.all());
    let written = everything_written(&env);
    assert!(
        !written.contains(&new) && !written.contains(KEY),
        "a key is in a log or the audit trail"
    );
    // an unknown word is a usage error
    let bad = env.cli(&["web", "frobnicate"]);
    assert_eq!(bad.code, 2, "{}", bad.all());
    assert!(bad.stderr.contains("usage"), "{}", bad.stderr);
}

#[test]
fn a_broken_configuration_binds_nothing_and_says_why() {
    let port = free_port();
    let env = Env::with(port, Some("key = \"short\"\n"), false);
    assert_eq!(env.adapter_info()["state"], "ready", "the adapter runs");
    std::thread::sleep(Duration::from_millis(300));
    assert!(!listening(port));
    assert!(
        wait_for(10, || env.adapter_stderr().contains("nothing is listening")),
        "{}",
        env.adapter_stderr()
    );
    let status = env.cli(&["web"]);
    assert!(
        status.stdout.contains("is not listening") && status.stdout.contains("at least 24"),
        "{}",
        status.stdout
    );
    // the file was not replaced
    assert_eq!(
        std::fs::read_to_string(env.config_path()).unwrap(),
        "key = \"short\"\n"
    );
    let new_key = env.cli(&["web", "new-key"]);
    assert_eq!(new_key.code, 1, "{}", new_key.all());
    assert_eq!(
        std::fs::read_to_string(env.config_path()).unwrap(),
        "key = \"short\"\n"
    );
}

#[test]
fn the_shipped_defaults_listen_on_loopback_only_and_make_their_own_key() {
    if listening(9615) {
        eprintln!("skipped: something already listens on 9615");
        return;
    }
    // no configuration at all: the default address of the manifest, a key of its own
    let env = Env::with(9615, None, false);
    assert!(
        wait_for(20, || listening(9615)),
        "{}\n{}",
        env.adapter_stderr(),
        env.daemon_stderr()
    );
    let on = listen_addresses(9615);
    assert!(!on.is_empty());
    assert!(
        on.iter()
            .all(|a| a.ends_with(":0100007F") || a.ends_with(":00000000000000000000000001000000")),
        "only loopback: {on:?}"
    );
    let shown = env.cli(&["web"]);
    let key = shown
        .stdout
        .lines()
        .find_map(|l| l.strip_prefix("key: "))
        .expect("the key is shown")
        .to_string();
    assert_eq!(
        api_with(9615, "GET", "/api/processes", &[("X-API-Key", &key)], "").status,
        200
    );
    assert_eq!(api_with(9615, "GET", "/api/processes", &[], "").status, 401);
}

// ---- what it may do -----------------------------------------------------------------------------

#[test]
fn health_answers_only_ok_and_there_is_no_metrics() {
    let (_env, port) = start();
    let r = api_with(port, "GET", "/api/health", &[], "");
    assert_eq!(r.status, 200);
    assert_eq!(r.body.trim(), r#"{"ok":true}"#);
    for path in ["/metrics", "/api/metrics", "/"] {
        assert_eq!(api(port, "GET", path, "").status, 404, "{path}");
    }
}

#[test]
fn the_default_scope_is_the_apps_that_opted_in() {
    let (env, port) = start();
    assert_eq!(
        env.start_app_with("mine", "sleep 60", false, &[], true)["type"],
        "started"
    );
    assert_eq!(
        env.start_app_with("other", "sleep 60", false, &[], false)["type"],
        "started"
    );
    let (mine, other) = (env.wait_running("mine"), env.wait_running("other"));
    let info = env.adapter_info();
    assert_eq!(info["grant"]["host"]["level"], "lifecycle", "{info}");
    assert_eq!(info["grant"]["host"]["scope"], "opted-in", "{info}");
    // everything can be looked at
    let list = api(port, "GET", "/api/processes", "").json();
    assert_eq!(list["data"].as_array().unwrap().len(), 2);
    assert_eq!(api(port, "GET", "/api/processes/other", "").status, 200);
    // an app that did not opt in cannot be touched
    for action in ["stop", "restart"] {
        let r = api(port, "POST", &format!("/api/processes/other/{action}"), "");
        assert_eq!(r.status, 403, "{action}: {}", r.body);
        assert_eq!(r.json()["code"], "E_POLICY_DENIED");
        assert!(
            r.json()["error"].as_str().unwrap().contains("adapters"),
            "{}",
            r.body
        );
    }
    assert_eq!(app_pid(&env, "other"), Some(u64::from(other)));
    // one that did can be restarted
    let r = api(port, "POST", "/api/processes/mine/restart", "");
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(
        wait_for(20, || app_pid(&env, "mine")
            .is_some_and(|p| p != u64::from(mine))),
        "the app was not restarted"
    );
    // and all-or-nothing: `all` includes one that did not opt in
    assert_eq!(api(port, "POST", "/api/processes/all/stop", "").status, 403);
    assert_eq!(
        api(port, "POST", "/api/processes/mine/stop", "").status,
        200
    );
}

#[test]
fn what_the_adapter_does_is_audited_as_the_adapter() {
    let (env, port) = start();
    assert_eq!(env.start_app("web", "sleep 60", false)["type"], "started");
    assert_eq!(
        api(port, "POST", "/api/processes/web/restart", "").status,
        200
    );
    let log = env.home.join("audit").join("audit.jsonl");
    assert!(
        wait_for(10, || {
            std::fs::read_to_string(&log)
                .unwrap_or_default()
                .lines()
                .any(|l| {
                    l.contains(r#""actor":{"class":"adapter","label":"ui-web""#)
                        && l.contains(r#""op":"restart""#)
                        && l.contains(r#""decision":"allow""#)
                })
        }),
        "{}",
        std::fs::read_to_string(&log).unwrap_or_default()
    );
}

#[test]
fn at_the_default_level_it_cannot_create_delete_or_flush() {
    let (env, port) = start();
    assert_eq!(env.start_app("web", "sleep 60", false)["type"], "started");
    let before = app_count(&env);
    // creating: refused by the adapter, with the reason, whatever the body is
    for body in [
        r#"{"name":"x","command":"true","cwd":"/tmp"}"#,
        "{}",
        "nonsense",
    ] {
        let r = api(port, "POST", "/api/processes", body);
        assert_eq!(r.status, 403, "{body}: {}", r.body);
        assert!(
            r.json()["error"].as_str().unwrap().contains("admin"),
            "{}",
            r.body
        );
    }
    // deleting: refused by the daemon
    let r = api(port, "DELETE", "/api/processes/web", "");
    assert_eq!(r.status, 403, "{}", r.body);
    assert_eq!(r.json()["code"], "E_POLICY_DENIED");
    let r = api(port, "DELETE", "/api/processes/all?confirm=all", "");
    assert_eq!(r.status, 403, "{}", r.body);
    assert_eq!(app_count(&env), before);
}

#[test]
fn an_adapter_with_the_admin_level_creates_cmd_apps_and_deletes_only_what_is_confirmed() {
    let port = free_port();
    let env = Env::new_admin(port, "");
    assert!(
        wait_for(20, || listening(port)),
        "{}\n{}",
        env.adapter_stderr(),
        env.daemon_stderr()
    );
    assert_eq!(env.adapter_info()["grant"]["host"]["level"], "admin");
    let project = env.project();
    // a directory that looks like a node project: nothing is guessed from it
    std::fs::write(
        project.join("package.json"),
        r#"{"name":"p","scripts":{"start":"true"}}"#,
    )
    .unwrap();
    let cwd = project.to_str().unwrap();

    // refused before the daemon is asked, and the adapter survives each
    for (body, why) in [
        (
            json!({ "name": "x", "command": "sleep", "cwd": "." }),
            "absolute",
        ),
        (
            json!({ "name": "x", "command": "sleep", "cwd": "relative/dir" }),
            "absolute",
        ),
        (
            json!({ "name": "bad name!", "command": "sleep", "cwd": cwd }),
            "invalid app name",
        ),
        (
            json!({ "name": "../x", "command": "sleep", "cwd": cwd }),
            "invalid app name",
        ),
        (
            json!({ "name": "all", "command": "sleep", "cwd": cwd }),
            "reserved",
        ),
        (
            json!({ "name": "x", "command": "sleep", "cwd": cwd, "watch": true }),
            "unknown field",
        ),
        (
            json!({ "name": "x", "command": "sleep", "cwd": cwd, "runtime": "No Such;" }),
            "runtime",
        ),
    ] {
        let r = api(port, "POST", "/api/processes", &body.to_string());
        assert_eq!(r.status, 400, "{body}: {}", r.body);
        assert!(
            r.json()["error"].as_str().unwrap().contains(why),
            "{body}: {}",
            r.body
        );
    }
    assert_eq!(app_count(&env), 0);
    assert_eq!(
        api(port, "GET", "/api/processes", "").status,
        200,
        "still alive"
    );

    // a good one: the runtime is cmd, not npm
    let r = api(
        port,
        "POST",
        "/api/processes",
        &json!({ "name": "made", "command": "sleep", "args": ["60"], "cwd": cwd }).to_string(),
    );
    assert_eq!(r.status, 200, "{}", r.body);
    let shown = api(port, "GET", "/api/processes/made", "").json();
    assert_eq!(shown["data"]["spec"]["mode"], "cmd", "{shown}");
    assert_eq!(shown["data"]["spec"]["cwd"], cwd);
    assert!(
        shown["data"]["spec"]["adapters"].get("ui-web").is_some(),
        "the scope opted-in needs it"
    );
    env.wait_running("made");

    // delete all needs the field; one app by name does not
    assert_eq!(api(port, "DELETE", "/api/processes/all", "").status, 400);
    assert_eq!(app_count(&env), 1);
    assert_eq!(
        api(port, "DELETE", "/api/processes/all", r#"{"confirm":"all"}"#).status,
        200
    );
    assert_eq!(app_count(&env), 0);
}

// ---- the stream ---------------------------------------------------------------------------------

#[test]
fn the_websocket_carries_events_the_daemon_made() {
    let (env, port) = start();
    let mut ws = Ws::connect(port, &[("X-API-Key", KEY)])
        .ok()
        .expect("connect");
    assert_eq!(ws.next_json(10).unwrap()["type"], "hello");

    // a start and a stop made through the daemon's socket as a person: nothing in this adapter made them
    assert_eq!(
        env.start_app("watched", "sleep 60", false)["type"],
        "started"
    );
    let started = ws
        .wait_for(15, |m| {
            m["type"] == "event" && m["kind"] == "started" && m["app"]["name"] == "watched"
        })
        .expect("a started event");
    assert_eq!(started["family"], "lifecycle");
    assert!(
        started["seq"].as_u64().is_some() && started["at"].is_string(),
        "{started}"
    );
    let pid = env.wait_running("watched");

    // a crash the supervisor sees by itself
    let killed = std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status()
        .unwrap();
    assert!(killed.success());
    let crashed = ws
        .wait_for(15, |m| {
            m["type"] == "event"
                && m["app"]["name"] == "watched"
                && matches!(m["kind"].as_str(), Some("crashed" | "errored"))
        })
        .expect("a crash event");
    assert_eq!(crashed["data"]["exit_signal"], 9, "{crashed}");

    // a stop made through the daemon's socket (the crashed app is past stopping, so another one)
    assert_eq!(
        env.start_app("second", "sleep 60", false)["type"],
        "started"
    );
    env.wait_running("second");
    let _ = env.rpc(json!({ "type": "stop", "selector": { "by": "name", "value": "second" } }));
    let stopped = ws.wait_for(15, |m| {
        m["type"] == "event" && m["kind"] == "stopped" && m["app"]["name"] == "second"
    });
    assert!(stopped.is_some(), "a stopped event");
    // and the restart by the CLI
    let run = env.cli(&["restart", "second"]);
    assert_eq!(run.code, 0, "{}", run.all());
    assert!(ws
        .wait_for(15, |m| m["type"] == "event"
            && m["kind"] == "started"
            && m["app"]["name"] == "second")
        .is_some());
}

#[test]
fn the_websocket_key_travels_as_a_subprotocol_never_in_the_url() {
    let (_env, port) = start();
    let offered = format!("oxidepm.v1, oxidepm-key.{KEY}");
    let ws = Ws::connect(port, &[("Sec-WebSocket-Protocol", &offered)])
        .ok()
        .expect("the subprotocol key is accepted");
    assert_eq!(
        ws.protocol.as_deref(),
        Some("oxidepm.v1"),
        "the key is not echoed back as the protocol"
    );
    let wrong = "oxidepm.v1, oxidepm-key.not-the-key-not-the-key-not";
    assert!(matches!(
        Ws::connect(port, &[("Sec-WebSocket-Protocol", wrong)]),
        Err(WsError::Refused(401))
    ));
    // in the URL it counts for nothing
    assert!(matches!(
        Ws::connect_path(port, &format!("/ws?key={KEY}"), &[]),
        Err(WsError::Refused(401))
    ));
    assert!(matches!(
        Ws::connect_path(port, &format!("/ws?api_key={KEY}&token={KEY}"), &[]),
        Err(WsError::Refused(401))
    ));
    // and a subprotocol is not a key for the REST routes
    let addr = format!("127.0.0.1:{port}");
    let r = request_with(
        &addr,
        "GET",
        "/api/processes",
        Some(&addr),
        &[("Sec-WebSocket-Protocol", &offered)],
        "",
    )
    .unwrap();
    assert_eq!(r.status, 401);
}

#[test]
fn key_rotation_disconnects_existing_event_streams() {
    let (env, port) = start();
    let mut old = Ws::connect(port, &[("X-API-Key", KEY)])
        .ok()
        .expect("connect with the old key");
    assert_eq!(old.next_json(10).unwrap()["type"], "hello");

    let run = env.cli(&["web", "new-key"]);
    assert_eq!(run.code, 0, "{}", run.all());
    let key = run
        .stdout
        .lines()
        .find_map(|line| line.strip_prefix("key: "))
        .unwrap();
    assert!(
        old.disconnected(),
        "the stream authenticated with the old key stayed open"
    );
    assert!(matches!(
        Ws::connect(port, &[("X-API-Key", KEY)]),
        Err(WsError::Refused(401))
    ));
    let mut new = Ws::connect(port, &[("X-API-Key", key)])
        .ok()
        .expect("connect after rotation");
    assert_eq!(new.next_json(10).unwrap()["type"], "hello");
    assert_eq!(
        env.start_app("after-rotation", "sleep 60", false)["type"],
        "started"
    );
    assert!(new
        .wait_for(15, |m| m["kind"] == "started"
            && m["app"]["name"] == "after-rotation")
        .is_some());
}

#[test]
fn the_websocket_log_excludes_client_text() {
    let (env, port) = start();
    let mut ws = Ws::connect(port, &[("X-API-Key", KEY)]).ok().unwrap();
    assert_eq!(ws.next_json(10).unwrap()["type"], "hello");
    let text = "hello\nui-web: FORGED LINE\r\n\u{1b}[31mred";
    ws.send_text(text);
    assert!(
        wait_for(10, || env.adapter_stderr().contains("is ignored")),
        "{}",
        env.adapter_stderr()
    );
    let log = env.adapter_stderr();
    assert!(
        !log.lines().any(|l| l.starts_with("ui-web: FORGED")),
        "a client forged a log line:\n{log}"
    );
    assert!(!log.contains('\u{1b}'));
    assert!(
        !log.contains("FORGED LINE"),
        "client text leaked into stderr"
    );
    assert!(
        log.contains(&format!("text ({} bytes)", text.len())),
        "{log}"
    );
    // the socket still works for the events afterwards
    assert_eq!(env.start_app("after", "sleep 60", false)["type"], "started");
    assert!(ws
        .wait_for(15, |m| m["kind"] == "started"
            && m["app"]["name"] == "after")
        .is_some());
}

#[test]
fn only_so_many_event_streams_and_connections_at_once() {
    let (_env, port) = start();
    // 16 streams are allowed; the 17th is told to come back later
    let mut streams = Vec::new();
    for _ in 0..16 {
        streams.push(
            Ws::connect(port, &[("X-API-Key", KEY)])
                .ok()
                .expect("a stream"),
        );
    }
    assert!(matches!(
        Ws::connect(port, &[("X-API-Key", KEY)]),
        Err(WsError::Refused(503))
    ));
    drop(streams);
    assert!(
        wait_for(10, || Ws::connect(port, &[("X-API-Key", KEY)]).is_ok()),
        "a freed slot is usable again"
    );
    // idle connections: the cap is 64, the next one is closed at once and the API still answers
    let idle: Vec<_> = (0..64)
        .filter_map(|_| std::net::TcpStream::connect(("127.0.0.1", port)).ok())
        .collect();
    assert_eq!(idle.len(), 64);
    std::thread::sleep(Duration::from_millis(200));
    let mut extra = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    extra
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut buf = [0u8; 16];
    assert!(
        matches!(extra.read(&mut buf), Ok(0) | Err(_)),
        "the connection over the cap is dropped"
    );
    drop(idle);
    assert!(
        wait_for(10, || api(port, "GET", "/api/processes", "").status == 200),
        "after they leave it answers again"
    );
}

// ---- the data ----------------------------------------------------------------------------------

#[test]
fn logs_of_a_tag_come_from_every_app_that_has_it() {
    let (env, port) = start();
    assert_eq!(
        env.start_app_with(
            "one",
            "echo hello-from-one; sleep 60",
            false,
            &["grp"],
            true
        )["type"],
        "started"
    );
    assert_eq!(
        env.start_app_with(
            "two",
            "echo hello-from-two; sleep 60",
            false,
            &["grp"],
            true
        )["type"],
        "started"
    );
    assert_eq!(
        env.start_app_with(
            "three",
            "echo not-in-the-group; sleep 60",
            false,
            &["other"],
            true
        )["type"],
        "started"
    );
    let mut body = Value::Null;
    assert!(
        wait_for(20, || {
            body = api(port, "GET", "/api/processes/@grp/logs", "").json();
            let text = body["data"].to_string();
            text.contains("hello-from-one") && text.contains("hello-from-two")
        }),
        "{body}"
    );
    let lines: Vec<String> = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l.as_str().unwrap().to_string())
        .collect();
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("[one] ") && l.contains("hello-from-one")),
        "{lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("[two] ") && l.contains("hello-from-two")),
        "{lines:?}"
    );
    assert!(
        !lines.iter().any(|l| l.contains("not-in-the-group")),
        "{lines:?}"
    );
    // one app, no tag: the lines as they are
    let single = api(port, "GET", "/api/processes/one/logs?lines=5", "").json();
    assert!(
        single["data"].to_string().contains("hello-from-one")
            && !single["data"].to_string().contains("[one]"),
        "{single}"
    );
    assert_eq!(
        api(port, "GET", "/api/processes/@nobody/logs", "").status,
        404
    );
    assert_eq!(api(port, "GET", "/api/processes/all/logs", "").status, 400);
}

#[test]
fn hammering_the_api_is_answered_by_the_adapter_and_never_gets_it_struck() {
    let (env, port) = start();
    assert_eq!(env.start_app("web", "sleep 60", false)["type"], "started");
    // the list is one question to the daemon a second
    for _ in 0..200 {
        assert_eq!(api(port, "GET", "/api/processes", "").status, 200);
    }
    // a call that is not cached is cut off by the adapter's own rate, long before the daemon's
    let mut codes = Vec::new();
    for _ in 0..200 {
        codes.push(api(port, "GET", "/api/processes/web", "").status);
    }
    assert!(codes.iter().all(|c| *c == 200 || *c == 429), "{codes:?}");
    assert!(codes.contains(&429), "the adapter's rate was never reached");
    assert!(codes.contains(&200));
    std::thread::sleep(Duration::from_millis(1100));
    assert_eq!(
        api(port, "GET", "/api/processes/web", "").status,
        200,
        "it recovers"
    );
    assert_eq!(
        env.adapter_info()["state"],
        "ready",
        "the daemon did not strike the adapter"
    );
}

#[test]
fn a_body_over_the_limit_is_refused() {
    let (_env, port) = start();
    let big = format!(
        r#"{{"name":"x","command":"{}","cwd":"/tmp"}}"#,
        "a".repeat(100_000)
    );
    let r = api(port, "POST", "/api/processes", &big);
    assert!(r.status == 413 || r.status == 403, "{}", r.status); // 403 at the default level; the limit applies before the body is read
    let r = api(port, "DELETE", "/api/processes/web", &"x".repeat(100_000));
    assert_eq!(r.status, 413, "{}", r.body);
}

#[test]
fn the_adapter_stays_up_when_apps_come_and_go() {
    let (env, port) = start();
    for i in 0..5 {
        let name = format!("app{i}");
        assert_eq!(env.start_app(&name, "sleep 60", false)["type"], "started");
    }
    assert_eq!(
        api(port, "POST", "/api/processes/@none/restart", "").status,
        404
    );
    env.stop_all();
    env.delete_all();
    assert_eq!(
        api(port, "GET", "/api/processes", "").json()["data"],
        json!([])
    );
    let pids_before = env.adapter_pids();
    assert_eq!(pids_before.len(), 1, "{pids_before:?}");
    env.restart_adapter(false);
    assert!(wait_for(20, || listening(port)));
    assert_eq!(env.adapter_pids().len(), 1);
}
