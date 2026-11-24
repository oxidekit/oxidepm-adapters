use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use oxidepm_runtime_testkit::{hex, wait_for, Adapter, Env, Run};
use serde_json::{json, Value};

fn new_adapter() -> Adapter {
    Adapter::new(
        env!("CARGO_MANIFEST_DIR"),
        env!("CARGO_BIN_EXE_oxidepm-deploy"),
    )
}

fn calls_file(env: &Env) -> PathBuf {
    env.home.join("adapters/state/deploy/ssh-argv")
}

fn shim_env() -> (Env, tempfile::TempDir) {
    let shim_dir = tempfile::tempdir().unwrap();
    let mut env = Env::bare(&[("PATH", shim_dir.path().to_str().unwrap())]);
    let shim = shim_dir.path().join("ssh");
    fs::write(
        &shim,
        format!(
            r#"#!/bin/sh
printf '%s\n' "$@" >> '{}'
case "$7" in
    *"git rev-parse --abbrev-ref HEAD") printf '%s\n' main ;;
    *"git pull origin -- 'main'") printf '%s\n' updated ;;
    "'oxidepm' restart 'all'") printf '%s\n' 'restart refused' >&2; exit 1 ;;
    *"git log --oneline --no-decorate -n 5") printf '%s\n' 'abc123 latest' ;;
    *"git status --short") printf '%s\n' ' M file' ;;
    *"git diff --stat HEAD") printf '%s\n' ' file | 1 +' ;;
    *) printf '%s\n' 'unexpected remote command' >&2; exit 5 ;;
esac
"#,
            calls_file(&env).display(),
        ),
    )
    .unwrap();
    fs::set_permissions(&shim, fs::Permissions::from_mode(0o755)).unwrap();

    // The test SSH is a shell script. Pass its directory in and allow its interpreter and log.
    let mut adapter = new_adapter();
    let mut manifest: Value = serde_json::from_str(&adapter.manifest).unwrap();
    manifest["permissions"]["env_pass"] = json!(["PATH", "SSH_AUTH_SOCK"]);
    manifest["permissions"]["spawn"] = json!({ "mode": "any" });
    manifest["permissions"]["fs"]["write"] = json!(["$STATE"]);
    adapter.manifest = manifest.to_string();
    env.install(&adapter);
    (env, shim_dir)
}

fn structured(run: &Run) -> Value {
    serde_json::from_str(&run.stdout).unwrap_or_else(|error| panic!("{error}: {}", run.all()))
}

#[test]
fn shipped_deploy_manifest_runs_a_dry_run_through_the_daemon() {
    let env = Env::new(&new_adapter());
    let run = env.cli(&[
        "--json",
        "--dry-run",
        "deploy",
        "deploy-test.invalid",
        "--branch",
        "main",
    ]);
    assert_eq!(run.code, 0, "{}", run.all());
    let data = structured(&run);
    assert_eq!(data["result"]["dry_run"], true);
    let steps = data["result"]["steps"].as_array().unwrap();
    assert_eq!(steps.len(), 2);
    for (step, command) in steps
        .iter()
        .zip(["git pull origin -- 'main'", "'oxidepm' restart 'all'"])
    {
        assert_eq!(
            step["argv"],
            json!([
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=10",
                "--",
                "deploy-test.invalid",
                command
            ])
        );
        assert!(step["exit_code"].is_null());
    }
    let grant = &env.adapter_info()["grant"];
    assert_eq!(grant["connect_env_socket"], json!(["SSH_AUTH_SOCK"]));
    assert_eq!(grant["spawn"], json!({ "mode": "named", "names": ["ssh"] }));
}

#[test]
fn deploy_dry_run_prints_exact_argv_and_spawns_nothing() {
    let (env, _shim) = shim_env();
    for args in [
        [
            "--dry-run",
            "deploy",
            "myhost",
            "--branch",
            "main",
            "--cwd",
            "/srv/a'b",
        ],
        [
            "deploy",
            "myhost",
            "--branch",
            "main",
            "--cwd",
            "/srv/a'b",
            "--dry-run",
        ],
    ] {
        let run = env.cli(&args);
        assert_eq!(run.code, 0, "{}", run.all());
        let argv: Vec<Value> = run
            .stdout
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(
            argv,
            [
                json!([
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "ConnectTimeout=10",
                    "--",
                    "myhost",
                    r#"cd '/srv/a'"'"'b' && git pull origin -- 'main'"#
                ]),
                json!([
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "ConnectTimeout=10",
                    "--",
                    "myhost",
                    "'oxidepm' restart 'all'"
                ]),
            ],
        );
        assert!(!calls_file(&env).exists());
    }
}

#[test]
fn deploy_and_diff_reject_leading_dash_host_and_bad_branch() {
    let (env, _shim) = shim_env();
    for (args, expected) in [
        (
            &["deploy", "--", "-oProxyCommand=x"][..],
            "Invalid SSH host",
        ),
        (
            &["deploy", "myhost", "--branch", "-x"][..],
            "Invalid Git branch",
        ),
        (&["diff", "--", "-oProxyCommand=x"][..], "Invalid SSH host"),
    ] {
        let run = env.cli(args);
        assert_eq!(run.code, 2, "{}", run.all());
        assert!(run.stderr.contains(expected), "{}", run.all());
    }
    assert!(!calls_file(&env).exists());
}

#[test]
fn deploy_failed_restart_exits_nonzero_with_fake_ssh_shim() {
    let (env, _shim) = shim_env();
    let run = env.cli(&["deploy", "myhost"]);
    assert_eq!(run.code, 3, "{}", run.all());
    assert!(
        run.stderr
            .contains("Deploy incomplete: remote restart failed"),
        "{}",
        run.all()
    );
    assert!(!run.all().contains("Deploy complete"), "{}", run.all());

    let run = env.cli(&["--json", "deploy", "myhost"]);
    assert_eq!(run.code, 3, "{}", run.all());
    let data = structured(&run);
    assert_eq!(data["exit_code"], 3);
    assert_eq!(data["result"]["branch"], "main");
    let steps = data["result"]["steps"].as_array().unwrap();
    assert_eq!(steps.len(), 3);
    assert_eq!(steps[0]["step"], "resolve_branch");
    assert_eq!(steps[0]["stdout"], "main\n");
    assert_eq!(steps[1]["step"], "git_pull");
    assert_eq!(steps[1]["command"], "git pull origin -- 'main'");
    assert_eq!(steps[1]["ok"], true);
    assert_eq!(steps[2]["step"], "restart");
    assert_eq!(steps[2]["ok"], false);
    assert_eq!(steps[2]["exit_code"], 1);
    assert_eq!(steps[2]["stderr"], "restart refused\n");
}

#[test]
fn diff_quotes_cwd_and_returns_steps_with_fake_ssh_shim() {
    let (env, _shim) = shim_env();
    let run = env.cli(&["--json", "diff", "myhost", "--cwd", "/srv/a'b"]);
    assert_eq!(run.code, 0, "{}", run.all());
    let data = structured(&run);
    let steps = data["result"]["steps"].as_array().unwrap();
    let captured = fs::read_to_string(calls_file(&env)).unwrap();
    let calls: Vec<_> = captured.lines().collect();
    assert_eq!(steps.len(), 3);
    assert_eq!(calls.len(), 21);
    for (i, (command, stdout)) in [
        (
            r#"cd '/srv/a'"'"'b' && git log --oneline --no-decorate -n 5"#,
            "abc123 latest\n",
        ),
        (r#"cd '/srv/a'"'"'b' && git status --short"#, " M file\n"),
        (
            r#"cd '/srv/a'"'"'b' && git diff --stat HEAD"#,
            " file | 1 +\n",
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let expected = [
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=10",
            "--",
            "myhost",
            command,
        ];
        assert_eq!(&calls[i * 7..(i + 1) * 7], &expected);
        assert_eq!(steps[i]["argv"], json!(expected));
        assert_eq!(steps[i]["command"], command);
        assert_eq!(steps[i]["stdout"], stdout);
        assert_eq!(steps[i]["ok"], true);
    }

    let dry = env.cli(&["--json", "--dry-run", "diff", "myhost", "--cwd", "/srv/a'b"]);
    assert_eq!(dry.code, 0, "{}", dry.all());
    let preview = structured(&dry);
    assert_eq!(preview["result"]["dry_run"], true);
    for (planned, actual) in preview["result"]["steps"]
        .as_array()
        .unwrap()
        .iter()
        .zip(steps)
    {
        assert_eq!(planned["argv"], actual["argv"]);
        assert!(planned["exit_code"].is_null());
    }
    assert_eq!(fs::read_to_string(calls_file(&env)).unwrap(), captured);
}

#[test]
fn deploy_invocation_is_audited_by_daemon() {
    let (env, _shim) = shim_env();
    let args = ["myhost", "--branch", "main", "--post", "npm run build"];
    let run = env.cli(&[&["--dry-run", "deploy"][..], &args].concat());
    assert_eq!(run.code, 0, "{}", run.all());
    assert!(!calls_file(&env).exists());
    let digest = hex(&format!("{}\0", args.join("\0")).into_bytes());
    let audit = env.home.join("audit/audit.jsonl");
    assert!(
        wait_for(10, || {
            fs::read_to_string(&audit)
                .unwrap_or_default()
                .lines()
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .any(|row| {
                    row["op"] == "adapter invoke"
                        && row["selector"] == "deploy"
                        && row["decision"] == "allow"
                        && row["dry_run"] == true
                        && row["args_sha256"] == digest
                })
        }),
        "{}",
        fs::read_to_string(&audit).unwrap_or_default()
    );
}
