//! The runtime-node adapter against the real `oxidepmd` and `oxidepm`: the daemon asks it what to
//! run, runs the steps and the plan itself, and the app is the daemon's child.

use std::path::Path;

use oxidepm_runtime_testkit::{hex, wait_for, Adapter, Env};
use serde_json::{json, Value};

fn adapter() -> Adapter {
    Adapter::new(
        env!("CARGO_MANIFEST_DIR"),
        env!("CARGO_BIN_EXE_oxidepm-runtime-node"),
    )
}

fn write(dir: &Path, name: &str, text: &str) {
    std::fs::write(dir.join(name), text).unwrap();
}

/// A script that records its arguments in `args.json` and then stays up.
const SHOW_ARGS: &str = "require('fs').writeFileSync('args.json', JSON.stringify(process.argv.slice(2))); setInterval(() => {}, 1000);";

fn args_seen(dir: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(dir.join("args.json")).ok()?).ok()
}

fn parent_of(pid: u32) -> u32 {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    // pid (comm) state ppid ...; the name may hold spaces and parentheses
    let after = &stat[stat.rfind(')').unwrap() + 2..];
    after.split_whitespace().nth(1).unwrap().parse().unwrap()
}

#[test]
fn a_node_script_runs_through_the_adapter_as_a_child_of_the_daemon() {
    let env = Env::new(&adapter());
    let dir = env.project();
    write(
        &dir,
        "server.js",
        "console.log('server is up'); setInterval(() => {}, 1000);",
    );
    let reply = env.start("web", "node", "server.js", &[], &dir);
    assert_eq!(reply["type"], "started", "{reply}");
    let pid = env.wait_running("web");
    assert!(
        wait_for(20, || env.app_log("web", "out").contains("server is up")),
        "log: {:?}",
        env.app_log("web", "out")
    );
    // the adapter answered with data; the daemon started the process
    assert_eq!(
        parent_of(pid),
        env.daemon_pid(),
        "the app is the daemon's child"
    );
    assert!(
        env.adapter_pids()
            .iter()
            .all(|p| *p as u32 != parent_of(pid)),
        "the adapter did not start the app"
    );
}

#[test]
fn arguments_reach_a_node_script_unchanged() {
    let env = Env::new(&adapter());
    let dir = env.project();
    write(&dir, "show.js", SHOW_ARGS);
    let reply = env.start(
        "show",
        "node",
        "show.js",
        &["--port", "3000", "a b", "--", "-x"],
        &dir,
    );
    assert_eq!(reply["type"], "started", "{reply}");
    env.wait_running("show");
    assert!(wait_for(20, || args_seen(&dir).is_some()));
    assert_eq!(
        args_seen(&dir).unwrap(),
        json!(["--port", "3000", "a b", "--", "-x"])
    );
}

#[test]
fn an_absolute_script_path_runs_from_the_apps_directory() {
    let env = Env::new(&adapter());
    let dir = env.project();
    let other = dir.join("elsewhere");
    std::fs::create_dir_all(&other).unwrap();
    write(
        &other,
        "where.js",
        "require('fs').writeFileSync('cwd.txt', process.cwd()); setInterval(() => {}, 1000);",
    );
    let reply = env.start(
        "where",
        "node",
        other.join("where.js").to_str().unwrap(),
        &[],
        &dir,
    );
    assert_eq!(reply["type"], "started", "{reply}");
    env.wait_running("where");
    assert!(wait_for(20, || dir.join("cwd.txt").exists()));
    assert_eq!(
        std::fs::read_to_string(dir.join("cwd.txt")).unwrap(),
        dir.to_str().unwrap()
    );
}

#[test]
fn npm_passes_flag_like_arguments_to_the_script() {
    // npm takes `--port 80` for its own option unless a `--` comes before it
    let env = Env::new(&adapter());
    let dir = env.project();
    write(&dir, "show.js", SHOW_ARGS);
    write(
        &dir,
        "package.json",
        r#"{"name":"t","version":"1.0.0","scripts":{"start":"node show.js"}}"#,
    );
    let reply = env.start("pkg", "npm", "start", &["--port", "80"], &dir);
    assert_eq!(reply["type"], "started", "{reply}");
    env.wait_running("pkg");
    assert!(
        wait_for(30, || args_seen(&dir).is_some()),
        "log: {} {}",
        env.app_log("pkg", "out"),
        env.app_log("pkg", "err")
    );
    assert_eq!(args_seen(&dir).unwrap(), json!(["--port", "80"]));
}

#[test]
fn an_npm_script_without_arguments_runs() {
    let env = Env::new(&adapter());
    let dir = env.project();
    write(&dir, "show.js", SHOW_ARGS);
    write(
        &dir,
        "package.json",
        r#"{"name":"t","version":"1.0.0","scripts":{"start":"node show.js"}}"#,
    );
    let reply = env.start("pkg", "npm", "start", &[], &dir);
    assert_eq!(reply["type"], "started", "{reply}");
    env.wait_running("pkg");
    assert!(wait_for(30, || args_seen(&dir).is_some()));
    assert_eq!(args_seen(&dir).unwrap(), json!([]));
}

/// A directory with a `pnpm` and a `yarn` that record their arguments, one per line, in
/// `<tool>.argv` of their working directory and then wait.
fn fake_package_tools() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;
    let bin = tempfile::tempdir().unwrap();
    for tool in ["pnpm", "yarn"] {
        let path = bin.path().join(tool);
        std::fs::write(
            &path,
            format!("#!/bin/sh\nprintf '%s\\n' \"$@\" > {tool}.argv\nexec sleep 300\n"),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    bin
}

#[test]
fn pnpm_and_yarn_are_given_run_the_script_and_the_arguments() {
    let bin = fake_package_tools();
    let path = format!(
        "{}:{}",
        bin.path().display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut env = Env::bare(&[("PATH", &path)]);
    env.install(&adapter());
    let dir = env.project();
    for tool in ["pnpm", "yarn"] {
        let reply = env.start(tool, tool, "dev", &["--host", "0.0.0.0"], &dir);
        assert_eq!(reply["type"], "started", "{reply}");
        env.wait_running(tool);
        let file = dir.join(format!("{tool}.argv"));
        assert!(wait_for(20, || file.exists()), "{tool} was not run");
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "run\ndev\n--host\n0.0.0.0\n",
            "{tool}"
        );
    }
}

#[test]
fn a_machine_without_node_fails_the_start_and_says_which_program() {
    let empty = tempfile::tempdir().unwrap();
    let mut env = Env::bare(&[("PATH", empty.path().to_str().unwrap())]);
    env.install(&adapter());
    let dir = env.project();
    write(&dir, "server.js", "setInterval(() => {}, 1000);");
    let reply = env.start("web", "node", "server.js", &[], &dir);
    let text = reply.to_string();
    let failed = reply["type"] == "error" || env.wait_state("web", "errored");
    assert!(failed, "{text}");
    let all = format!("{text} {:?} {}", env.app("web"), env.daemon_stderr());
    assert!(all.contains("'node' not found in PATH"), "{all}");
}

#[test]
fn without_the_adapter_a_node_app_is_refused_with_the_command_to_run() {
    let env = Env::bare(&[]);
    let dir = env.project();
    write(&dir, "server.js", "setInterval(() => {}, 1000);");
    let reply = env.start("web", "node", "server.js", &[], &dir);
    assert_eq!(reply["type"], "error", "{reply}");
    assert_eq!(reply["code"], "E_ADAPTER_MISSING", "{reply}");
    assert!(
        reply["message"]
            .as_str()
            .unwrap()
            .contains("oxidepm adapter import runtime-node --enable"),
        "{reply}"
    );
    assert!(env.app("web").is_none(), "nothing was created");
}

#[test]
fn a_disabled_adapter_is_named_with_the_command_that_enables_it() {
    let env = Env::new(&adapter());
    let run = env.cli(&["adapter", "disable", "runtime-node"]);
    assert_eq!(run.code, 0, "{}", run.all());
    let dir = env.project();
    write(&dir, "server.js", "setInterval(() => {}, 1000);");
    let reply = env.start("web", "npm", "start", &[], &dir);
    assert_eq!(reply["code"], "E_ADAPTER_MISSING", "{reply}");
    assert!(
        reply["message"]
            .as_str()
            .unwrap()
            .contains("oxidepm adapter enable runtime-node"),
        "{reply}"
    );
}

#[test]
fn the_adapter_is_a_pure_runtime_and_asks_for_nothing_else() {
    let manifest = oxidepm_adapter_proto::Manifest::parse(adapter().manifest.as_bytes())
        .expect("a valid manifest");
    let p = &manifest.permissions;
    assert!(p.is_pure_class(), "`--yes` may enable it");
    assert_eq!(manifest.provides.runtimes, ["node", "npm", "pnpm", "yarn"]);
    // the only programs the daemon will run on its word
    assert_eq!(manifest.provides.programs, ["node", "npm", "pnpm", "yarn"]);
    assert!(manifest.provides.capture_programs.is_empty() && manifest.provides.env_set.is_empty());
    assert!(manifest.provides.verbs.is_empty() && p.events.is_empty());
    assert!(p.fs.write.is_empty() && p.fs.read == ["$STATE"]);
}

// ---- the bundle of a release ------------------------------------------------------------------

#[test]
fn the_bundle_tool_builds_a_bundle_that_oxidepm_can_import() {
    let env = Env::with_bundle(&[&adapter()], &[]);
    let run = env.cli(&["adapter", "import", "--bundled", "--tag", "runtime"]);
    assert_eq!(run.code, 0, "{}\n{}", run.all(), env.daemon_stderr());

    let info = env.cli(&["--json", "adapter", "info", "runtime-node"]);
    assert_eq!(info.code, 0, "{}", info.all());
    let info: Value = serde_json::from_str(&info.stdout).unwrap();
    assert_eq!(info["source"], "bundled", "{info}");
    assert_eq!(info["enabled"], false, "{info}");
}

#[test]
fn a_node_app_starts_from_the_bundle_of_a_release_with_nothing_fetched() {
    // The release carries the adapter in its local bundle. One command imports and enables it,
    // then a node app runs.
    let adapter = adapter();
    let env = Env::with_bundle(&[&adapter], &[]);
    let dir = env.project();
    write(
        &dir,
        "server.js",
        "console.log('from the bundle'); setInterval(() => {}, 1000);",
    );

    // before the import there is no provider, and the message names the way to get it
    let reply = env.start("web", "node", "server.js", &[], &dir);
    assert_eq!(reply["code"], "E_ADAPTER_MISSING", "{reply}");

    let run = env.cli(&[
        "adapter",
        "import",
        "--bundled",
        "--tag",
        "runtime",
        "--enable",
        "--yes",
    ]);
    assert_eq!(run.code, 0, "{}\n{}", run.all(), env.daemon_stderr());
    let info = env.cli(&["--json", "adapter", "info", "runtime-node"]);
    let info: Value =
        serde_json::from_str(&info.stdout).unwrap_or_else(|_| panic!("{}", info.all()));
    assert_eq!(info["enabled"], true, "{info}");
    assert_eq!(info["source"], "bundled", "{info}");
    assert_eq!(info["trust"], "bundled:verified", "{info}");

    let reply = env.start("web", "node", "server.js", &[], &dir);
    assert_eq!(reply["type"], "started", "{reply}");
    env.wait_running("web");
    assert!(
        wait_for(20, || env.app_log("web", "out").contains("from the bundle")),
        "log: {:?}",
        env.app_log("web", "out")
    );
}

#[test]
fn a_bundle_with_a_changed_index_imports_nothing() {
    let adapter = adapter();
    let env = Env::with_bundle(&[&adapter], &[]);
    let index = env.bundle_dir().unwrap().join("index.json");
    let mut bytes = std::fs::read(&index).unwrap();
    let expected = hex(&bytes);
    bytes.push(b'\n');
    let actual = hex(&bytes);
    std::fs::write(&index, bytes).unwrap();
    let run = env.cli(&[
        "adapter",
        "import",
        "--bundled",
        "--tag",
        "runtime",
        "--enable",
        "--yes",
    ]);
    assert_eq!(run.code, 1, "{}", run.all());
    assert!(run.stderr.contains("E_CHECKSUM_MISMATCH"), "{}", run.stderr);
    assert!(
        run.stderr.contains(&format!(
            "index.json has sha256 {actual}, expected {expected}"
        )),
        "{}",
        run.stderr
    );
    let list = env.cli(&["--json", "adapter", "list"]);
    assert_eq!(list.code, 0, "{}", list.all());
    assert_eq!(
        serde_json::from_str::<Value>(&list.stdout).unwrap(),
        json!([])
    );
    let dir = env.project();
    let reply = env.start("web", "node", "server.js", &[], &dir);
    assert_eq!(reply["code"], "E_ADAPTER_MISSING", "{reply}");
}
