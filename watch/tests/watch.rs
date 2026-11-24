//! File changes in a project, through the sandboxed adapter and the real daemon.

#![cfg(target_os = "linux")]

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::{Duration, Instant};

use oxidepm_core::{AppSpec, HealthCheck, Runtime};
use oxidepm_runtime_testkit::{wait_for, Adapter, Env};
use serde_json::{json, Value};

const APP: &str = "watched";

fn adapter() -> Adapter {
    Adapter::new(
        env!("CARGO_MANIFEST_DIR"),
        env!("CARGO_BIN_EXE_oxidepm-watch"),
    )
}

fn app_spec(name: &str, cwd: &Path, options: Option<Value>) -> AppSpec {
    let mut spec = AppSpec::new(
        name.into(),
        Runtime::CMD,
        "/bin/sleep".into(),
        cwd.to_path_buf(),
    );
    spec.args.push("600".into());
    spec.restart_policy.auto_restart = false;
    if let Some(options) = options {
        spec.adapters.insert("watch".into(), options);
    }
    spec
}

fn start(env: &Env, spec: AppSpec) -> u32 {
    let reply = env.rpc(json!({ "type": "start", "spec": spec }));
    assert_eq!(reply["type"], "started", "{reply}");
    env.wait_running(&spec.name)
}

fn write(dir: &Path, name: &str, text: impl AsRef<[u8]>) {
    fs::write(dir.join(name), text).unwrap();
}

fn pid(env: &Env, name: &str) -> Option<u64> {
    env.app(name)?["state"]["pid"].as_u64()
}

fn restarts(env: &Env) -> u64 {
    env.app(APP).unwrap()["state"]["restarts"].as_u64().unwrap()
}

// Wait for the kernel watch to be armed before writing or testing that an event is ignored.
fn is_watched(env: &Env, dir: &Path) -> bool {
    let inode = format!("ino:{:x}", fs::metadata(dir).unwrap().ino());
    env.adapter_pids().into_iter().any(|pid| {
        fs::read_dir(format!("/proc/{pid}/fdinfo"))
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|fd| fs::read_to_string(fd.path()).ok())
            .any(|info| {
                info.lines().any(|line| {
                    line.starts_with("inotify ")
                        && line.split_whitespace().any(|field| field == inode)
                })
            })
    })
}

fn wait_watching(env: &Env, dir: &Path) {
    assert!(
        wait_for(12, || is_watched(env, dir)),
        "no watch for {}\n{}\n{}",
        dir.display(),
        env.adapter_info(),
        env.daemon_stderr()
    );
}

fn start_watching(env: &Env, cwd: &Path, debounce_ms: u64) -> u32 {
    let spec = app_spec(
        APP,
        cwd,
        Some(json!({ "paths": ["."], "debounce_ms": debounce_ms })),
    );
    let first = start(env, spec);
    wait_watching(env, cwd);
    first
}

fn wait_restarted(env: &Env, first: u32) {
    assert!(
        wait_for(15, || pid(env, APP)
            .is_some_and(|pid| pid != u64::from(first))),
        "app did not restart: {:?}\n{}\n{}",
        env.app(APP),
        env.adapter_info(),
        env.daemon_stderr()
    );
}

#[test]
fn file_change_restarts_opted_in_app() {
    let env = Env::new(&adapter());
    let dir = env.project();
    let first = start_watching(&env, &dir, 100);
    write(&dir, "source.txt", "changed");
    wait_restarted(&env, first);
}

#[test]
fn watches_apps_while_the_first_health_probe_is_pending() {
    let env = Env::new(&adapter());
    let dir = env.project();
    let mut spec = app_spec(APP, &dir, Some(json!({ "debounce_ms": 100 })));
    spec.health_check = Some(HealthCheck {
        start_period_secs: 30,
        ..HealthCheck::script("true")
    });
    let first = start(&env, spec);
    assert_eq!(env.app(APP).unwrap()["state"]["status"], "starting");
    wait_watching(&env, &dir);
    write(&dir, "source.txt", "changed");
    wait_restarted(&env, first);
}

#[test]
fn project_under_ignored_parent_name_still_restarts() {
    let env = Env::new(&adapter());
    let dir = env.project().join("target/my-app");
    fs::create_dir_all(dir.join("src")).unwrap();
    let first = start_watching(&env, &dir, 100);
    wait_watching(&env, &dir.join("src"));
    write(&dir, "src/a.txt", "changed");
    wait_restarted(&env, first);
}

#[test]
fn ignored_subdir_and_file_do_not_trigger_restart() {
    let env = Env::new(&adapter());
    let dir = env.project();
    fs::create_dir(dir.join("node_modules")).unwrap();
    let first = start_watching(&env, &dir, 100);
    assert!(!is_watched(&env, &dir.join("node_modules")));
    write(&dir, "node_modules/x.js", "changed");
    write(&dir, "app.log", "changed");
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(pid(&env, APP), Some(u64::from(first)));
}

#[test]
fn reading_a_file_does_not_trigger_restart() {
    let env = Env::new(&adapter());
    let dir = env.project();
    write(&dir, "source.txt", "contents");
    let first = start_watching(&env, &dir, 100);
    for _ in 0..5 {
        assert_eq!(
            fs::read_to_string(dir.join("source.txt")).unwrap(),
            "contents"
        );
    }
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(pid(&env, APP), Some(u64::from(first)));
}

#[test]
fn burst_of_writes_causes_one_trailing_restart() {
    let env = Env::new(&adapter());
    let dir = env.project();
    let first = start_watching(&env, &dir, 150);
    let before = restarts(&env);
    for index in 0..4 {
        write(&dir, "source.txt", index.to_string());
        std::thread::sleep(Duration::from_millis(20));
    }
    let last = Instant::now();
    write(&dir, "source.txt", "final");
    wait_restarted(&env, first);
    assert!(
        last.elapsed() >= Duration::from_millis(150),
        "restarted before the quiet period"
    );
    wait_watching(&env, &dir);
    std::thread::sleep(Duration::from_millis(450));
    assert_eq!(restarts(&env), before + 1);
}

#[test]
fn non_opted_in_app_is_never_touched() {
    let env = Env::new(&adapter());
    let dir = env.project();
    let other = start(&env, app_spec("unwatched", &dir, None));
    let first = start_watching(&env, &dir, 100);
    write(&dir, "source.txt", "changed");
    wait_restarted(&env, first);
    assert_eq!(pid(&env, "unwatched"), Some(u64::from(other)));
}

#[test]
fn stopping_or_deleting_app_stops_watching() {
    let env = Env::new(&adapter());
    let dir = env.project();
    start_watching(&env, &dir, 100);
    env.stop_all();
    assert!(env.wait_state(APP, "stopped"));
    assert!(
        wait_for(12, || !is_watched(&env, &dir)),
        "watch survived stop"
    );
    write(&dir, "source.txt", "after stop");
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(env.app(APP).unwrap()["state"]["status"], "stopped");

    start_watching(&env, &dir, 100);
    env.delete_all();
    assert!(
        wait_for(12, || !is_watched(&env, &dir)),
        "watch survived delete"
    );
    write(&dir, "source.txt", "after delete");
    std::thread::sleep(Duration::from_millis(300));
    assert!(env.app(APP).is_none());
}

#[test]
fn directories_created_after_start_are_watched() {
    let env = Env::new(&adapter());
    let dir = env.project();
    let first = start_watching(&env, &dir, 60_000);
    fs::create_dir_all(dir.join("src/sub")).unwrap();
    wait_watching(&env, &dir.join("src/sub"));
    assert_eq!(pid(&env, APP), Some(u64::from(first)));
}

#[test]
fn enabling_the_service_discovers_apps_that_are_already_running() {
    let adapter = adapter();
    let env = Env::new(&adapter);
    let dir = env.project();
    let fingerprint = adapter.package().fingerprint;
    let enable = |env: &Env| {
        let result = env.cli(&["adapter", "enable", "watch", "--accept", &fingerprint]);
        assert_eq!(result.code, 0, "{}", result.all());
    };
    let first = start_watching(&env, &dir, 100);
    assert_eq!(env.cli(&["adapter", "disable", "watch"]).code, 0);
    assert!(wait_for(12, || !is_watched(&env, &dir)));
    enable(&env);
    wait_watching(&env, &dir);
    write(&dir, "source.txt", "changed");
    wait_restarted(&env, first);
}

#[test]
fn discovery_rejects_unknown_options_for_an_already_running_app() {
    let adapter = adapter();
    let env = Env::new(&adapter);
    let dir = env.project();
    start(&env, app_spec(APP, &dir, Some(json!({"debouce_ms": 20}))));
    assert_eq!(env.cli(&["adapter", "disable", "watch"]).code, 0);

    let fingerprint = adapter.package().fingerprint;
    let enabled = env.cli(&["adapter", "enable", "watch", "--accept", &fingerprint]);
    assert_eq!(enabled.code, 0, "{}", enabled.all());

    assert!(
        wait_for(5, || {
            env.adapter_info()["stderr_tail"]
                .as_array()
                .is_some_and(|lines| {
                    lines.iter().any(|line| {
                        line.as_str().is_some_and(|line| {
                            line.contains("unknown field") && line.contains("debouce_ms")
                        })
                    })
                })
        }),
        "discovery accepted unknown watch options: {}",
        env.adapter_info()
    );
    assert!(!is_watched(&env, &dir));
}
