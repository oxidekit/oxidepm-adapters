use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use oxidepm_core::{AppSpec, Runtime};
use oxidepm_runtime_testkit::{core_bins, core_dir, Adapter, Env};
use serde_json::{json, Value};

const URL: &str = "https://example.invalid/a/b.git";

fn adapter() -> &'static Adapter {
    static ADAPTER: OnceLock<Adapter> = OnceLock::new();
    ADAPTER.get_or_init(|| {
        Adapter::new(
            env!("CARGO_MANIFEST_DIR"),
            env!("CARGO_BIN_EXE_oxidepm-git-start"),
        )
    })
}

fn state(env: &Env) -> PathBuf {
    env.home.join("adapters/state/git-start")
}

fn set_manifest(adapter: &mut Adapter, change: impl FnOnce(&mut Value)) {
    let mut manifest = serde_json::from_str(&adapter.manifest).unwrap();
    change(&mut manifest);
    adapter.manifest = manifest.to_string();
}

#[test]
fn git_start_dry_run_validates_url_and_clones_nothing() {
    let env = Env::new(adapter());
    let run = env.cli(&["--dry-run", "git-start", URL, "--install"]);
    assert_eq!(run.code, 0, "{}", run.all());
    assert!(
        run.stdout
            .contains(&state(&env).join("clones").display().to_string()),
        "{}",
        run.all()
    );
    assert!(
        run.stdout
            .contains("git clone --depth 1 -- https://example.invalid/a/b.git"),
        "{}",
        run.all()
    );
    assert!(
        run.stdout.contains("npm install --ignore-scripts"),
        "{}",
        run.all()
    );
    assert!(!state(&env).join("clones").exists());
    assert!(env.app("b").is_none());

    let invalid = env.cli(&["--dry-run", "git-start", "http://example.invalid/b"]);
    assert_eq!(invalid.code, 2, "{}", invalid.all());
    assert!(!state(&env).join("clones").exists());
}

#[test]
fn git_start_rejects_disallowed_url_scheme_and_leading_dash() {
    let env = Env::new(adapter());
    for args in [
        &["git-start", "--", "--upload-pack=id"][..],
        &["git-start", "file:///nonexistent/repo"],
    ] {
        let run = env.cli(args);
        assert_eq!(run.code, 2, "{}", run.all());
        assert!(
            run.stderr
                .contains("Expected an https://, ssh:// or git@ repository URL"),
            "{}",
            run.all()
        );
    }
    assert!(!state(&env).join("clones").exists());
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().into()
}

fn new_checkout(files: &[(&str, &str)]) -> (tempfile::TempDir, String, Env) {
    new_checkout_with_path(files, None)
}

fn new_checkout_with_path(
    files: &[(&str, &str)],
    path: Option<&str>,
) -> (tempfile::TempDir, String, Env) {
    let fixture = tempfile::tempdir().unwrap();
    let src = fixture.path().join("work");
    fs::create_dir(&src).unwrap();
    git(&src, &["init", "--quiet", "--initial-branch=main"]);
    let script = src.join("run.sh");
    fs::write(&script, "#!/bin/sh\nexec /bin/sleep 60\n").unwrap();
    fs::set_permissions(script, fs::Permissions::from_mode(0o755)).unwrap();
    for (name, contents) in files {
        fs::write(src.join(name), contents).unwrap();
    }
    git(&src, &["add", "--", "."]);
    let out = Command::new("git")
        .current_dir(&src)
        .args(["commit", "--quiet", "-m", "Seed the checkout"])
        .env("GIT_AUTHOR_NAME", "OxidePM Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "OxidePM Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let commit = git(&src, &["rev-parse", "--short", "HEAD"]);
    let bare = fixture.path().join("source.git");
    git(
        &src,
        &["clone", "--quiet", "--bare", ".", bare.to_str().unwrap()],
    );

    // Use a local transport for this URL without changing the checkout or machine's Git config.
    let config = fixture.path().join("gitconfig");
    fs::write(
        &config,
        format!("[url \"file://{}\"]\n\tinsteadOf = {URL}\n", bare.display()),
    )
    .unwrap();
    let mut daemon_env = vec![("GIT_CONFIG_GLOBAL", config.to_str().unwrap())];
    if let Some(path) = path {
        daemon_env.push(("PATH", path));
    }
    let mut env = Env::bare(&daemon_env);
    let mut test_adapter = Adapter::new(
        env!("CARGO_MANIFEST_DIR"),
        env!("CARGO_BIN_EXE_oxidepm-git-start"),
    );
    set_manifest(&mut test_adapter, |manifest| {
        let env_pass = manifest["permissions"]["env_pass"].as_array_mut().unwrap();
        env_pass.push(json!("GIT_CONFIG_GLOBAL"));
        if path.is_some() {
            env_pass.push(json!("PATH"));
        }
    });
    env.install(&test_adapter);
    (fixture, commit, env)
}

#[test]
fn git_start_clones_prints_commit_and_cli_starts_app() {
    let (_fixture, commit, env) = new_checkout(&[]);
    let run = env.cli(&["git-start", URL, "--name", "demo", "--bin", "./run.sh"]);
    assert_eq!(run.code, 0, "{}", run.all());
    assert!(
        run.stdout
            .contains(&format!("Commit: {commit} Seed the checkout")),
        "{}",
        run.all()
    );
    assert!(run.stdout.contains("Started demo"), "{}", run.all());
    env.wait_running("demo");
    let app = env.app("demo").unwrap();
    let dir = Path::new(app["spec"]["cwd"].as_str().unwrap());
    assert_eq!(dir.parent().unwrap(), state(&env).join("clones"));
    assert!(dir.join(".git").is_dir());
}

#[test]
fn git_start_json_includes_start_outcome_and_honors_no_start() {
    let (_fixture, commit, env) = new_checkout(&[]);
    let run = env.cli(&[
        "--json",
        "git-start",
        URL,
        "--name",
        "prepared",
        "--bin",
        "./run.sh",
        "--no-start",
        "--mode",
        "cmd",
        "--port",
        "3000",
        "--env",
        "PORT=4000",
        "--env",
        "KEY=value",
    ]);
    assert_eq!(run.code, 0, "{}", run.all());
    let json: Value = serde_json::from_str(&run.stdout).unwrap();
    assert_eq!(json["result"]["commit"], commit);
    assert_eq!(json["result"]["auto_start"], false);
    assert!(json.get("start").is_none(), "{json}");
    assert_eq!(json["result"]["suggested_spec"]["port"], 3000);
    assert_eq!(
        json["result"]["suggested_spec"]["env"],
        json!({"PORT": "4000", "KEY": "value"})
    );
    assert!(env.app("prepared").is_none());

    let (_fixture, commit, env) = new_checkout(&[]);
    let run = env.cli(&[
        "--json",
        "git-start",
        URL,
        "--name",
        "demo",
        "--bin",
        "./run.sh",
    ]);
    assert_eq!(run.code, 0, "{}", run.all());
    let json: Value = serde_json::from_str(&run.stdout).unwrap();
    assert_eq!(json["result"]["commit"], commit);
    assert_eq!(json["start"]["type"], "started", "{json}");
    assert_eq!(json["start"]["name"], "demo", "{json}");
    env.wait_running("demo");
}

#[test]
fn git_start_installs_node_dependencies_without_running_scripts() {
    if system_program("npm").is_none() {
        eprintln!("npm is unavailable on the adapter search path");
        return;
    }
    let (_fixture, _, env) = new_checkout(node_project());
    check_node_install(&env);
}

fn system_program(name: &str) -> Option<PathBuf> {
    // The sandbox searches the system path rather than the test runner's toolchain.
    ["/usr/local/bin", "/usr/bin", "/bin"]
        .iter()
        .map(|dir| Path::new(dir).join(name))
        .find(|path| path.is_file())
}

fn node_project() -> &'static [(&'static str, &'static str)] {
    &[
        (
            "package.json",
            r#"{"name":"fixture","version":"1.0.0","scripts":{"postinstall":"touch ran-install"}}"#,
        ),
        (".npmrc", "audit=false\n"),
    ]
}

fn check_node_install(env: &Env) {
    let run = env.cli(&["--json", "git-start", URL, "--no-start", "--install"]);
    assert_eq!(run.code, 0, "{}", run.all());
    let done: Value = serde_json::from_str(&run.stdout).unwrap();
    let dir = Path::new(done["result"]["clone_dir"].as_str().unwrap());
    assert!(dir.join("package-lock.json").is_file());
    assert!(!dir.join("ran-install").exists());
}

#[cfg(target_os = "linux")]
#[test]
fn git_start_installs_node_from_a_symlinked_prefix_with_a_large_stack_limit() {
    use nix::libc;
    use std::os::unix::fs::symlink;

    let (Some(node), Some(npm)) = (system_program("node"), system_program("npm")) else {
        eprintln!("Node and npm are required on the adapter search path");
        return;
    };
    let tools = tempfile::tempdir().unwrap();
    let installed_bin = tools.path().join("toolcache/node/bin");
    fs::create_dir_all(&installed_bin).unwrap();
    fs::copy(node, installed_bin.join("node")).unwrap();
    symlink(npm, installed_bin.join("npm")).unwrap();
    let bin = tools.path().join("bin");
    symlink(&installed_bin, &bin).unwrap();
    let path = format!("{}:/usr/local/bin:/usr/bin:/bin", bin.display());
    let (_fixture, _, env) = new_checkout_with_path(node_project(), Some(&path));

    let pid = env.daemon_pid() as libc::pid_t;
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: the daemon is this test's child and the kernel writes into a valid rlimit.
    let result = unsafe { libc::prlimit(pid, libc::RLIMIT_STACK, std::ptr::null(), &mut limit) };
    assert_eq!(result, 0, "{}", std::io::Error::last_os_error());
    const LARGE_STACK: libc::rlim_t = 16 * 1024 * 1024;
    if limit.rlim_max < LARGE_STACK {
        eprintln!("The inherited hard stack limit is below 16 MiB");
        return;
    }
    limit.rlim_cur = LARGE_STACK;
    // SAFETY: the kernel reads a valid rlimit and applies it only to this test's daemon.
    let result = unsafe { libc::prlimit(pid, libc::RLIMIT_STACK, &limit, std::ptr::null_mut()) };
    assert_eq!(result, 0, "{}", std::io::Error::last_os_error());

    check_node_install(&env);
}

#[test]
fn git_start_adapter_cannot_call_start_directly() {
    let core = core_bins();
    let fixture = core.daemon.with_file_name("oxidepm-fixture-adapter");
    let target = fixture.parent().unwrap().parent().unwrap();
    let out = Command::new(env!("CARGO"))
        .current_dir(core_dir())
        .env("CARGO_TARGET_DIR", target)
        .args([
            "build",
            "--offline",
            "--quiet",
            "-p",
            "oxidepm-adapter-fixture",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let inputs = tempfile::tempdir().unwrap();
    let script = inputs.path().join("script.json");
    let spec = AppSpec::new(
        "forbidden".into(),
        Runtime::CMD,
        "/bin/true".into(),
        inputs.path().into(),
    );
    fs::write(
        &script,
        json!([{"on": "command.invoke", "do": [
            {"host_request": {"request": {"type": "start", "spec": spec}}},
            {"sleep": {"ms": 1500}},
            {"reply": {"result": {"exit_code": 0}}}
        ]}])
        .to_string(),
    )
    .unwrap();
    let mut env = Env::bare(&[
        ("FIXTURE_SCRIPT", script.to_str().unwrap()),
        ("FIXTURE_REPORT", "report.jsonl"),
    ]);
    // Use the production manifest's host grant with a fixture that attempts the forbidden call.
    let mut probe = Adapter::new(env!("CARGO_MANIFEST_DIR"), fixture.to_str().unwrap());
    set_manifest(&mut probe, |manifest| {
        assert_eq!(manifest["permissions"]["host"]["level"], "read");
        manifest["permissions"]["env_pass"] = json!(["FIXTURE_SCRIPT", "FIXTURE_REPORT"]);
    });
    env.install(&probe);
    let run = env.cli(&["git-start", URL]);
    assert_eq!(run.code, 0, "{}", run.all());

    let report = fs::read_to_string(state(&env).join("report.jsonl")).unwrap();
    let answer = report
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|record| record["what"] == "host_answer")
        .expect("the host answered");
    let response = &answer["frame"]["result"]["response"];
    assert_eq!(response["code"], "E_POLICY_DENIED", "{answer}");
    assert!(
        response["message"]
            .as_str()
            .unwrap()
            .contains("adapter:git-start"),
        "{answer}"
    );
    assert!(env.app("forbidden").is_none());
}
