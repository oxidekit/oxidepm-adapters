//! The runtime-go adapter against the real `oxidepmd` and `oxidepm`. A `go` that records what it
//! was given stands in for the toolchain, so the tests do not need Go.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use oxidepm_runtime_testkit::{wait_for, Adapter, Env};

fn adapter() -> Adapter {
    Adapter::new(
        env!("CARGO_MANIFEST_DIR"),
        env!("CARGO_BIN_EXE_oxidepm-runtime-go"),
    )
}

/// A directory with a `go` that writes its arguments, one per line, and its working directory to
/// `go.seen` of the directory it runs in, and then waits.
fn fake_go() -> tempfile::TempDir {
    let bin = tempfile::tempdir().unwrap();
    let path = bin.path().join("go");
    std::fs::write(
        &path,
        "#!/bin/sh\n{ pwd; printf '%s\\n' \"$@\"; } > go.seen\nexec sleep 300\n",
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

fn env_with_fake_go(bin: &Path) -> Env {
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut env = Env::bare(&[("PATH", &path)]);
    env.install(&adapter());
    env
}

fn parent_of(pid: u32) -> u32 {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let after = &stat[stat.rfind(')').unwrap() + 2..];
    after.split_whitespace().nth(1).unwrap().parse().unwrap()
}

#[test]
fn go_run_is_started_by_the_daemon_with_the_target_and_the_arguments() {
    let bin = fake_go();
    let env = env_with_fake_go(bin.path());
    let dir = env.project();
    let reply = env.start("srv", "go", ".", &["--port", "8080", "a b"], &dir);
    assert_eq!(reply["type"], "started", "{reply}");
    let pid = env.wait_running("srv");
    let seen = dir.join("go.seen");
    assert!(wait_for(20, || seen.exists()));
    assert_eq!(
        std::fs::read_to_string(&seen).unwrap(),
        format!("{}\nrun\n.\n--port\n8080\na b\n", dir.display())
    );
    assert_eq!(
        parent_of(pid),
        env.daemon_pid(),
        "the app is the daemon's child"
    );
}

#[test]
fn a_go_file_is_run_by_name() {
    let bin = fake_go();
    let env = env_with_fake_go(bin.path());
    let dir = env.project();
    let reply = env.start("one", "golang", "main.go", &[], &dir);
    assert_eq!(reply["type"], "started", "{reply}");
    env.wait_running("one");
    let seen = dir.join("go.seen");
    assert!(wait_for(20, || seen.exists()));
    assert_eq!(
        std::fs::read_to_string(&seen).unwrap(),
        format!("{}\nrun\nmain.go\n", dir.display())
    );
}

#[test]
fn a_machine_without_go_fails_the_start_and_says_which_program() {
    let empty = tempfile::tempdir().unwrap();
    let mut env = Env::bare(&[("PATH", empty.path().to_str().unwrap())]);
    env.install(&adapter());
    let dir = env.project();
    let reply = env.start("nogo", "go", ".", &[], &dir);
    let failed = reply["type"] == "error" || env.wait_state("nogo", "errored");
    let all = format!("{reply} {:?} {}", env.app("nogo"), env.daemon_stderr());
    assert!(failed, "{all}");
    assert!(all.contains("'go' not found in PATH"), "{all}");
}

#[test]
fn without_the_adapter_a_go_app_is_refused_with_the_command_to_run() {
    let env = Env::bare(&[]);
    let dir = env.project();
    let reply = env.start("srv", "go", ".", &[], &dir);
    assert_eq!(reply["code"], "E_ADAPTER_MISSING", "{reply}");
    assert!(
        reply["message"]
            .as_str()
            .unwrap()
            .contains("oxidepm adapter import runtime-go --enable"),
        "{reply}"
    );
    assert!(env.app("srv").is_none(), "nothing was created");
}

#[test]
fn the_adapter_is_a_pure_runtime_and_asks_for_nothing_else() {
    let manifest = oxidepm_adapter_proto::Manifest::parse(adapter().manifest.as_bytes())
        .expect("a valid manifest");
    let p = &manifest.permissions;
    assert!(p.is_pure_class(), "`--yes` may enable it");
    assert_eq!(manifest.provides.runtimes, ["go"]);
    assert_eq!(manifest.provides.programs, ["go"]);
    assert!(manifest.provides.capture_programs.is_empty() && manifest.provides.env_set.is_empty());
    assert!(manifest.provides.verbs.is_empty() && p.events.is_empty());
    assert!(p.fs.write.is_empty() && p.fs.read == ["$STATE"]);
}
