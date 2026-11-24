//! The runtime-rust adapter against the real `oxidepmd` and `oxidepm`, with the real `rustc` and
//! `cargo`: the daemon runs the build steps itself (with the app's environment, a time limit and
//! a bounded log) and then starts the binary they made.

use std::path::{Path, PathBuf};

use oxidepm_runtime_testkit::{wait_for, Adapter, Env};
use serde_json::Value;

fn adapter() -> Adapter {
    Adapter::new(
        env!("CARGO_MANIFEST_DIR"),
        env!("CARGO_BIN_EXE_oxidepm-runtime-rust"),
    )
}

/// The directory of the toolchain's own `cargo` and `rustc`, so that no rustup proxy, which needs
/// the real home directory, is in the way of a daemon whose home is a temporary directory.
fn toolchain_bin() -> PathBuf {
    let out = std::process::Command::new("rustc")
        .args(["--print", "sysroot"])
        .output()
        .expect("rustc");
    assert!(out.status.success(), "rustc is needed for these tests");
    PathBuf::from(String::from_utf8(out.stdout).unwrap().trim()).join("bin")
}

fn with_toolchain() -> Env {
    let path = format!(
        "{}:{}",
        toolchain_bin().display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut env = Env::bare(&[("PATH", &path)]);
    env.install(&adapter());
    env
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// A program that prints a line, records its arguments and a compile-time variable, and waits.
const SERVER: &str = r#"fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::fs::write("seen.txt", format!("{:?}|{}", args, option_env!("APP_BUILD_VAR").unwrap_or("unset"))).unwrap();
    println!("server up");
    std::thread::sleep(std::time::Duration::from_secs(300));
}
"#;

fn manifest(name: &str, extra: &str) -> String {
    format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n{extra}\n")
}

fn seen(dir: &Path) -> Option<String> {
    std::fs::read_to_string(dir.join("seen.txt")).ok()
}

fn parent_of(pid: u32) -> u32 {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let after = &stat[stat.rfind(')').unwrap() + 2..];
    after.split_whitespace().nth(1).unwrap().parse().unwrap()
}

fn status_of(env: &Env, name: &str) -> String {
    env.app(name)
        .and_then(|a| a["state"]["status"].as_str().map(str::to_string))
        .unwrap_or_default()
}

#[test]
fn a_single_rust_file_is_compiled_and_run_as_a_child_of_the_daemon() {
    let env = with_toolchain();
    let dir = env.project();
    write(&dir.join("my-tool.rs"), SERVER);
    let reply = env.start("tool", "rust", "my-tool.rs", &["--port", "3000"], &dir);
    assert_eq!(reply["type"], "started", "{reply}");
    let pid = env.wait_running("tool");
    assert!(
        wait_for(20, || seen(&dir).is_some()),
        "log: {} {}",
        env.app_log("tool", "out"),
        env.app_log("tool", "err")
    );
    assert_eq!(seen(&dir).unwrap(), "[\"--port\", \"3000\"]|unset");
    assert!(dir.join(".oxidepm/bin/my-tool").is_file());
    assert_eq!(
        parent_of(pid),
        env.daemon_pid(),
        "the app is the daemon's child"
    );
    // the compiler's output went to the build log
    assert!(env.home.join("logs").join("tool-build.log").exists());
}

#[test]
fn a_file_that_does_not_compile_fails_the_start_and_shows_the_compiler() {
    let env = with_toolchain();
    let dir = env.project();
    write(
        &dir.join("broken.rs"),
        "fn main() { let x: u32 = \"no\"; }\n",
    );
    let reply = env.start("broken", "rust", "broken.rs", &[], &dir);
    let all = format!("{reply} {:?}", env.app("broken"));
    assert!(
        reply["type"] == "error" || env.wait_state("broken", "errored"),
        "{all}"
    );
    let log =
        std::fs::read_to_string(env.home.join("logs").join("broken-build.log")).unwrap_or_default();
    assert!(
        log.contains("mismatched types") || all.contains("mismatched types"),
        "{all}\n{log}"
    );
    assert!(!dir.join(".oxidepm/bin/broken").exists());
}

#[test]
fn a_file_with_another_extension_is_refused_with_the_reason() {
    let env = with_toolchain();
    let dir = env.project();
    write(&dir.join("notes.txt"), "x");
    let reply = env.start("notes", "rust", "notes.txt", &[], &dir);
    let all = format!("{reply} {:?} {}", env.app("notes"), env.daemon_stderr());
    assert!(
        reply["type"] == "error" || env.wait_state("notes", "errored"),
        "{all}"
    );
    assert!(all.contains("expected a .rs file"), "{all}");
}

#[test]
fn a_cargo_project_is_built_in_release_mode_and_its_binary_run() {
    let env = with_toolchain();
    let dir = env.project();
    write(&dir.join("Cargo.toml"), &manifest("demo", ""));
    write(&dir.join("src/main.rs"), SERVER);
    let reply = env.start("demo", "cargo", ".", &["-v"], &dir);
    assert_eq!(reply["type"], "started", "{reply}");
    let pid = env.wait_running("demo");
    assert!(wait_for(20, || seen(&dir).is_some()));
    assert_eq!(seen(&dir).unwrap(), "[\"-v\"]|unset");
    assert!(dir.join("target/release/demo").is_file());
    assert_eq!(parent_of(pid), env.daemon_pid());
}

#[test]
fn the_build_is_reported_as_building_while_it_runs() {
    let env = with_toolchain();
    let dir = env.project();
    write(
        &dir.join("Cargo.toml"),
        &manifest("slow", "build = \"build.rs\""),
    );
    write(
        &dir.join("build.rs"),
        "fn main() { std::thread::sleep(std::time::Duration::from_secs(4)); }\n",
    );
    write(&dir.join("src/main.rs"), SERVER);
    // the start is answered when the app is up, so it is asked from another thread
    std::thread::scope(|scope| {
        let starter = scope.spawn(|| env.start("slow", "cargo", ".", &[], &dir));
        assert!(
            wait_for(30, || status_of(&env, "slow") == "building"),
            "status: {:?}",
            env.app("slow")
        );
        let reply = starter.join().unwrap();
        assert_eq!(reply["type"], "started", "{reply}");
    });
    env.wait_running("slow");
}

#[test]
fn the_build_gets_the_environment_of_the_app() {
    // the old build ignored it: a variable that only the compiler reads never reached cargo
    let env = with_toolchain();
    let dir = env.project();
    write(&dir.join("Cargo.toml"), &manifest("envd", ""));
    write(&dir.join("src/main.rs"), SERVER);
    let reply = env.start_with(
        "envd",
        "cargo",
        ".",
        &[],
        &dir,
        &[("APP_BUILD_VAR", "from-the-app")],
    );
    assert_eq!(reply["type"], "started", "{reply}");
    env.wait_running("envd");
    assert!(wait_for(20, || seen(&dir).is_some()));
    assert_eq!(seen(&dir).unwrap(), "[]|from-the-app");
}

#[test]
fn a_workspace_member_with_a_library_and_a_configured_target_directory_is_found() {
    // the package is `app-server`, its binary `server`, its library `x`; the workspace puts the
    // build in `out/` through its cargo configuration; the guess of the old runner (the package
    // name under ./target/release) is wrong on all three counts
    let env = with_toolchain();
    let dir = env.project();
    write(
        &dir.join("Cargo.toml"),
        "[workspace]\nmembers = [\"app-server\", \"tools\"]\nresolver = \"2\"\n",
    );
    write(
        &dir.join(".cargo/config.toml"),
        "[build]\ntarget-dir = \"out\"\n",
    );
    write(
        &dir.join("app-server/Cargo.toml"),
        &manifest("app-server", "\n[lib]\nname = \"x\"\npath = \"src/lib.rs\"\n\n[[bin]]\nname = \"server\"\npath = \"src/main.rs\""),
    );
    write(&dir.join("app-server/src/lib.rs"), "pub fn f() {}\n");
    write(&dir.join("app-server/src/main.rs"), SERVER);
    write(&dir.join("tools/Cargo.toml"), &manifest("tools", ""));
    write(
        &dir.join("tools/src/main.rs"),
        "fn main() { println!(\"tools\"); }\n",
    );
    let reply = env.start("ws", "cargo", "server", &[], &dir);
    assert_eq!(reply["type"], "started", "{reply}");
    env.wait_running("ws");
    assert!(dir.join("out/release/server").is_file());
    assert!(
        !dir.join("target").exists(),
        "the configured directory was used"
    );
    assert!(wait_for(20, || seen(&dir).is_some()));
}

#[test]
fn several_binaries_and_no_name_is_refused_with_the_list() {
    let env = with_toolchain();
    let dir = env.project();
    write(
        &dir.join("Cargo.toml"),
        "[workspace]\nmembers = [\"a\", \"b\"]\nresolver = \"2\"\n",
    );
    for (member, bin) in [("a", "alpha"), ("b", "beta")] {
        write(
            &dir.join(member).join("Cargo.toml"),
            &manifest(
                member,
                &format!("\n[[bin]]\nname = \"{bin}\"\npath = \"src/main.rs\""),
            ),
        );
        write(&dir.join(member).join("src/main.rs"), "fn main() {}\n");
    }
    let reply = env.start("multi", "cargo", ".", &[], &dir);
    let all = format!("{reply} {:?} {}", env.app("multi"), env.daemon_stderr());
    assert!(
        reply["type"] == "error" || env.wait_state("multi", "errored"),
        "{all}"
    );
    assert!(
        all.contains("several binaries") && all.contains("alpha, beta"),
        "{all}"
    );
}

#[test]
fn a_directory_that_is_not_a_cargo_project_fails_with_the_cargo_message() {
    let env = with_toolchain();
    let dir = env.project();
    let reply = env.start("empty", "cargo", ".", &[], &dir);
    let all = format!("{reply} {:?}", env.app("empty"));
    assert!(
        reply["type"] == "error" || env.wait_state("empty", "errored"),
        "{all}"
    );
    let log =
        std::fs::read_to_string(env.home.join("logs").join("empty-build.log")).unwrap_or_default();
    assert!(
        all.contains("Cargo.toml") || log.contains("Cargo.toml"),
        "{all}\n{log}"
    );
}

#[test]
fn a_target_directory_in_the_apps_environment_is_refused_with_the_other_ways() {
    let env = with_toolchain();
    let dir = env.project();
    write(&dir.join("Cargo.toml"), &manifest("demo", ""));
    write(&dir.join("src/main.rs"), SERVER);
    let reply = env.start_with(
        "td",
        "cargo",
        ".",
        &[],
        &dir,
        &[("CARGO_TARGET_DIR", "/somewhere")],
    );
    let all = format!("{reply} {:?} {}", env.app("td"), env.daemon_stderr());
    assert!(
        reply["type"] == "error" || env.wait_state("td", "errored"),
        "{all}"
    );
    assert!(all.contains(".cargo/config.toml"), "{all}");
    assert!(!dir.join("target").exists(), "nothing was built");
}

#[test]
fn a_machine_without_cargo_fails_the_start_and_says_which_program() {
    let empty = tempfile::tempdir().unwrap();
    let mut env = Env::bare(&[("PATH", empty.path().to_str().unwrap())]);
    env.install(&adapter());
    let dir = env.project();
    let reply = env.start("nocargo", "cargo", ".", &[], &dir);
    let all = format!("{reply} {:?} {}", env.app("nocargo"), env.daemon_stderr());
    assert!(
        reply["type"] == "error" || env.wait_state("nocargo", "errored"),
        "{all}"
    );
    assert!(all.contains("'cargo' not found in PATH"), "{all}");
}

#[test]
fn without_the_adapter_rust_and_cargo_apps_are_refused_with_the_command_to_run() {
    let env = Env::bare(&[]);
    let dir = env.project();
    for runtime in ["rust", "cargo"] {
        let reply: Value = env.start("x", runtime, ".", &[], &dir);
        assert_eq!(reply["code"], "E_ADAPTER_MISSING", "{runtime}: {reply}");
        assert!(
            reply["message"]
                .as_str()
                .unwrap()
                .contains("oxidepm adapter import runtime-rust --enable"),
            "{reply}"
        );
    }
}

#[test]
fn the_adapter_is_a_pure_runtime_and_asks_for_nothing_else() {
    let manifest = oxidepm_adapter_proto::Manifest::parse(adapter().manifest.as_bytes())
        .expect("a valid manifest");
    let p = &manifest.permissions;
    assert!(p.is_pure_class(), "`--yes` may enable it");
    assert_eq!(manifest.provides.runtimes, ["rust", "cargo"]);
    assert_eq!(manifest.provides.programs, ["cargo", "rustc"]);
    assert_eq!(manifest.provides.capture_programs, ["cargo"]);
    assert!(
        manifest.provides.env_set.is_empty()
            && manifest.provides.verbs.is_empty()
            && p.events.is_empty()
    );
    assert!(p.fs.write.is_empty() && p.fs.read == ["$STATE"]);
}
