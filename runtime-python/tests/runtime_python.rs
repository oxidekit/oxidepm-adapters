//! The runtime-python adapter against the real `oxidepmd` and `oxidepm`.

use std::path::Path;

use oxidepm_runtime_testkit::{wait_for, Adapter, Env};
use serde_json::{json, Value};

fn adapter() -> Adapter {
    Adapter::new(
        env!("CARGO_MANIFEST_DIR"),
        env!("CARGO_BIN_EXE_oxidepm-runtime-python"),
    )
}

fn has_python3() -> bool {
    std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

fn write(dir: &Path, name: &str, text: &str) {
    std::fs::write(dir.join(name), text).unwrap();
}

/// A script that records its interpreter, its environment's `PYTHONUNBUFFERED` and its arguments
/// in `seen.json` and then stays up.
const SHOW: &str = "import json, os, sys, time\njson.dump({'argv': sys.argv[1:], 'prefix': sys.prefix, 'unbuffered': os.environ.get('PYTHONUNBUFFERED'), 'cwd': os.getcwd()}, open('seen.json', 'w'))\nprint('ready', flush=True)\ntime.sleep(300)\n";

fn seen(dir: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(dir.join("seen.json")).ok()?).ok()
}

fn parent_of(pid: u32) -> u32 {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let after = &stat[stat.rfind(')').unwrap() + 2..];
    after.split_whitespace().nth(1).unwrap().parse().unwrap()
}

#[test]
fn a_python_script_runs_as_a_child_of_the_daemon_with_unbuffered_output() {
    if !has_python3() {
        eprintln!("python3 is not installed here; skipped");
        return;
    }
    let env = Env::new(&adapter());
    let dir = env.project();
    write(&dir, "app.py", SHOW);
    let reply = env.start("py", "python", "app.py", &["--port", "3000"], &dir);
    assert_eq!(reply["type"], "started", "{reply}");
    let pid = env.wait_running("py");
    assert!(
        wait_for(30, || seen(&dir).is_some()),
        "log: {} {}",
        env.app_log("py", "out"),
        env.app_log("py", "err")
    );
    let seen = seen(&dir).unwrap();
    assert_eq!(seen["argv"], json!(["--port", "3000"]));
    assert_eq!(seen["unbuffered"], "1");
    assert_eq!(seen["cwd"], dir.to_str().unwrap());
    assert_ne!(
        Path::new(seen["prefix"].as_str().unwrap()),
        dir.join("venv"),
        "no virtualenv here"
    );
    assert_eq!(
        parent_of(pid),
        env.daemon_pid(),
        "the app is the daemon's child"
    );
    // the line was printed with flush, but the variable is what makes an unflushed print appear
    assert!(wait_for(20, || env.app_log("py", "out").contains("ready")));
}

#[test]
fn a_virtualenv_of_the_project_is_the_interpreter() {
    if !has_python3() {
        eprintln!("python3 is not installed here; skipped");
        return;
    }
    let env = Env::new(&adapter());
    let dir = env.project();
    write(&dir, "app.py", SHOW);
    let make = |name: &str| {
        let made = std::process::Command::new("python3")
            .args(["-m", "venv", "--without-pip", name])
            .current_dir(&dir)
            .output()
            .unwrap();
        assert!(
            made.status.success(),
            "{}",
            String::from_utf8_lossy(&made.stderr)
        );
    };
    // the prefix of the interpreter the app was started with
    let prefix_of = |app: &str| {
        let _ = std::fs::remove_file(dir.join("seen.json"));
        let reply = env.start(app, "python", "app.py", &[], &dir);
        assert_eq!(reply["type"], "started", "{reply}");
        assert!(
            wait_for(30, || seen(&dir).is_some()),
            "log: {} {}",
            env.app_log(app, "out"),
            env.app_log(app, "err")
        );
        let prefix = seen(&dir).unwrap()["prefix"].as_str().unwrap().to_string();
        env.delete_all();
        std::path::PathBuf::from(prefix)
    };
    make("venv");
    assert_eq!(prefix_of("with-venv"), dir.join("venv"));
    // with both, venv comes first
    make(".venv");
    assert_eq!(prefix_of("with-both"), dir.join("venv"));
    std::fs::remove_dir_all(dir.join("venv")).unwrap();
    assert_eq!(prefix_of("with-dot-venv"), dir.join(".venv"));
}

#[test]
fn a_command_of_several_words_is_split_and_its_arguments_follow() {
    if !has_python3() {
        eprintln!("python3 is not installed here; skipped");
        return;
    }
    let env = Env::new(&adapter());
    let dir = env.project();
    write(&dir, "show.py", SHOW);
    let reply = env.start("multi", "python", "-u 'show.py' --flag", &["x y"], &dir);
    assert_eq!(reply["type"], "started", "{reply}");
    assert!(
        wait_for(30, || seen(&dir).is_some()),
        "log: {} {}",
        env.app_log("multi", "out"),
        env.app_log("multi", "err")
    );
    assert_eq!(seen(&dir).unwrap()["argv"], json!(["--flag", "x y"]));
}

#[test]
fn a_script_with_a_space_in_its_name_is_started_when_it_is_quoted() {
    if !has_python3() {
        eprintln!("python3 is not installed here; skipped");
        return;
    }
    let env = Env::new(&adapter());
    let dir = env.project();
    write(&dir, "my script.py", SHOW);
    let reply = env.start("spaced", "python", "'my script.py'", &[], &dir);
    assert_eq!(reply["type"], "started", "{reply}");
    assert!(wait_for(30, || seen(&dir).is_some()));
}

#[test]
fn the_apps_environment_does_not_reach_the_probe() {
    // The step that looks for the virtualenv runs without the app's own variables. A
    // `sitecustomize.py` on the app's PYTHONPATH records which program imported it: the app
    // does, the probe (`-c`) must not.
    if !has_python3() {
        eprintln!("python3 is not installed here; skipped");
        return;
    }
    let env = Env::new(&adapter());
    let dir = env.project();
    let site = dir.join("site");
    std::fs::create_dir_all(&site).unwrap();
    write(
        &site,
        "sitecustomize.py",
        "import sys\nopen('imported-by.txt', 'a').write(repr(sys.argv[:1]) + '\\n')\n",
    );
    write(&dir, "app.py", "import os, time\nopen('var.txt', 'w').write(os.environ.get('APP_VAR', ''))\ntime.sleep(300)\n");
    let reply = env.start_with(
        "envd",
        "python",
        "app.py",
        &[],
        &dir,
        &[("APP_VAR", "kept"), ("PYTHONPATH", site.to_str().unwrap())],
    );
    assert_eq!(reply["type"], "started", "{reply}");
    assert!(wait_for(30, || dir.join("var.txt").exists()));
    assert_eq!(
        std::fs::read_to_string(dir.join("var.txt")).unwrap(),
        "kept"
    );
    let imported = std::fs::read_to_string(dir.join("imported-by.txt")).unwrap_or_default();
    assert!(
        imported.contains("app.py"),
        "the app gets its PYTHONPATH: {imported:?}"
    );
    assert!(
        !imported.contains("'-c'"),
        "the probe does not: {imported:?}"
    );
}

#[test]
fn a_machine_without_python_fails_the_start_and_says_which_program() {
    let empty = tempfile::tempdir().unwrap();
    let mut env = Env::bare(&[("PATH", empty.path().to_str().unwrap())]);
    env.install(&adapter());
    let dir = env.project();
    write(&dir, "app.py", "print('x')\n");
    let reply = env.start("nopy", "python", "app.py", &[], &dir);
    let failed = reply["type"] == "error" || env.wait_state("nopy", "errored");
    let all = format!("{reply} {:?} {}", env.app("nopy"), env.daemon_stderr());
    assert!(failed, "{all}");
    assert!(all.contains("'python3' not found in PATH"), "{all}");
}

#[test]
fn without_the_adapter_a_python_app_is_refused_with_the_command_to_run() {
    let env = Env::bare(&[]);
    let dir = env.project();
    let reply = env.start("py", "python", "app.py", &[], &dir);
    assert_eq!(reply["code"], "E_ADAPTER_MISSING", "{reply}");
    assert!(
        reply["message"]
            .as_str()
            .unwrap()
            .contains("oxidepm adapter import runtime-python --enable"),
        "{reply}"
    );
    // the short name works too
    let reply = env.start("py2", "py", "app.py", &[], &dir);
    assert_eq!(reply["code"], "E_ADAPTER_MISSING", "{reply}");
}

#[test]
fn the_adapter_is_a_pure_runtime_and_asks_for_nothing_else() {
    let manifest = oxidepm_adapter_proto::Manifest::parse(adapter().manifest.as_bytes())
        .expect("a valid manifest");
    let p = &manifest.permissions;
    assert!(p.is_pure_class(), "`--yes` may enable it");
    assert_eq!(manifest.provides.runtimes, ["python"]);
    assert_eq!(manifest.provides.programs, ["python3", "python"]);
    assert_eq!(manifest.provides.capture_programs, ["python3", "python"]);
    assert_eq!(manifest.provides.env_set, ["PYTHONUNBUFFERED"]);
    assert!(manifest.provides.verbs.is_empty() && p.events.is_empty());
    assert!(p.fs.write.is_empty() && p.fs.read == ["$STATE"]);
}
