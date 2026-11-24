use std::{fs, sync::OnceLock};

use oxidepm_runtime_testkit::{Adapter, Env};

fn setup() -> Env {
    static ADAPTER: OnceLock<Adapter> = OnceLock::new();
    Env::new(ADAPTER.get_or_init(|| {
        Adapter::new(
            env!("CARGO_MANIFEST_DIR"),
            env!("CARGO_BIN_EXE_oxidepm-check"),
        )
    }))
}

#[test]
fn check_exits_nonzero_on_errors_and_zero_when_clean() {
    let env = setup();
    let run = env.cli(&["check", "."]);
    assert_eq!(run.code, 0, "{}", run.all());
    assert!(run.stdout.contains("All checks passed"), "{}", run.all());

    let run = env.cli(&["check", "./missing"]);
    assert_eq!(run.code, 1, "{}", run.all());

    fs::write(env.project().join("package.json"), "{").unwrap();
    let run = env.cli(&["check", "."]);
    assert_eq!(run.code, 1, "{}", run.all());
    assert!(run.all().contains("Invalid package.json"), "{}", run.all());
}

#[test]
fn check_strict_exits_nonzero_on_warnings() {
    let env = setup();
    fs::write(env.project().join("package.json"), "{}").unwrap();
    fs::create_dir(env.project().join("node_modules")).unwrap();
    for (args, code) in [(vec!["check", "."], 0), (vec!["check", ".", "--strict"], 1)] {
        let run = env.cli(&args);
        assert_eq!(run.code, code, "{}", run.all());
        assert!(run.stdout.contains("No lockfile"), "{}", run.all());
    }
}

#[cfg(target_os = "linux")]
#[test]
fn a_denied_bind_warns_unless_a_loopback_connection_finds_a_listener() {
    use std::net::{TcpListener, TcpStream};

    let env = setup();
    let sandbox = env.rpc(serde_json::json!({ "type": "daemon_status" }));
    let abi = sandbox["sandbox"]["landlock_abi"].as_u64().unwrap_or(0);
    if abi < 4 {
        eprintln!("skipped: Landlock ABI {abi} cannot limit TCP bind");
        return;
    }

    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let addr = listener.local_addr().unwrap();
    let connected = TcpStream::connect(addr).unwrap();
    // The client's port is occupied but won't answer a loopback connection.
    let port = connected.local_addr().unwrap().port();
    let file = env.project().join(".env");
    fs::write(&file, format!("PORT={port}\n")).unwrap();
    let run = env.cli(&["check", ".", "--strict"]);
    assert_eq!(run.code, 1, "{}", run.all());
    assert!(
        run.stdout
            .contains(&format!("[WARN] Could not check port {port}")),
        "{}",
        run.all()
    );

    fs::write(&file, format!("PORT={}\n", addr.port())).unwrap();
    let run = env.cli(&["check", "."]);
    assert_eq!(run.code, 1, "{}", run.all());
    assert!(
        run.stderr
            .contains(&format!("Port {} is already in use", addr.port())),
        "{}",
        run.all()
    );
    assert!(
        !run.all().contains("configure the app to use port"),
        "{}",
        run.all()
    );
}

#[test]
fn check_nonexistent_target_does_not_fall_back_to_cwd() {
    let env = setup();
    fs::write(env.project().join("package.json"), "{}").unwrap();
    let run = env.cli(&["check", "./does-not-exist"]);
    assert_eq!(run.code, 1, "{}", run.all());
    let output = run.all();
    assert!(
        output.contains("target does not exist:") && output.contains("does-not-exist"),
        "{output}"
    );
    assert!(
        !output.contains("package.json") && !output.contains("node_modules"),
        "{output}"
    );
}

#[test]
fn check_fix_creates_env_from_template_and_set_env_updates_it() {
    let env = setup();
    let file = env.project().join(".env");
    fs::write(env.project().join(".env.example"), "KEEP=value\n").unwrap();
    let run = env.cli(&["check", ".", "--fix"]);
    assert_eq!(run.code, 0, "{}", run.all());
    assert_eq!(fs::read_to_string(&file).unwrap(), "KEEP=value\n");

    for value in ["bar", "updated"] {
        let run = env.cli(&["check", ".", "--set-env", &format!("FOO={value}")]);
        assert_eq!(run.code, 0, "{}", run.all());
        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            format!("KEEP=value\nFOO={value}\n")
        );
    }
}

#[test]
fn check_rejects_fixes_outside_the_working_directory() {
    let env = setup();
    let outside = env.dir.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join(".env.example"), "KEEP=value\n").unwrap();
    for args in [
        vec!["check", "../outside", "--fix"],
        vec!["check", "../outside", "--set-env", "FOO=bar"],
    ] {
        let run = env.cli(&args);
        assert_eq!(run.code, 1, "{}", run.all());
        assert!(run.all().contains("working directory"), "{}", run.all());
        assert!(!outside.join(".env").exists());
    }
}

#[test]
fn check_json_returns_structured_summary() {
    let env = setup();
    let run = env.cli(&["--json", "check", "."]);
    assert_eq!(run.code, 0, "{}", run.all());
    let reply: serde_json::Value = serde_json::from_str(&run.stdout).unwrap();
    assert_eq!(reply["exit_code"], 0);
    let result = &reply["result"];
    assert_eq!(result["project_type"], "generic");
    assert!(result["checks"].is_array(), "{result}");
    for counter in ["warnings", "errors", "fixed"] {
        assert_eq!(result[counter], 0, "{result}");
    }
}

#[test]
fn start_check_runs_preflight_before_starting_the_app() {
    let env = setup();
    let run = env.cli(&["start", "--check", "-n", "missing", "./does-not-exist"]);
    assert_eq!(run.code, 1, "{}", run.all());
    assert!(
        run.all().contains("Preflight checks failed"),
        "{}",
        run.all()
    );
    assert!(run.all().contains("target does not exist"), "{}", run.all());
    assert!(env.app("missing").is_none());

    let run = env.cli(&[
        "start",
        "--check",
        "-n",
        "healthy",
        "/bin/sleep",
        "--",
        "30",
    ]);
    assert_eq!(run.code, 0, "{}", run.all());
    assert!(run.stdout.contains("All checks passed"), "{}", run.all());
    assert!(
        run.stdout.find("All checks passed") < run.stdout.find("Started healthy"),
        "{}",
        run.all()
    );
    assert!(env.app("healthy").is_some());
}
