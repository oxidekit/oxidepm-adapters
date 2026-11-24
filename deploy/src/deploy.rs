use oxidepm_adapter_proto::messages::{InvokeParams, InvokeResult, OutputStream};
use serde_json::json;

use crate::service::{check_plan, finish, parse_args, Emit};
use crate::shell::{cwd_command, shell_quote, valid_branch};
use crate::ssh::{SshRunner, Step};

pub const USAGE: &str = "Usage: oxidepm [--dry-run] deploy <HOST> [--cwd DIR] [--branch BRANCH] [--restart TARGET] [--post CMD] [--remote-bin PATH]";

pub fn pull_command(cwd: &str, branch: &str) -> String {
    format!("{cwd}git pull origin -- {}", shell_quote(branch))
}

pub async fn run(
    runner: &impl SshRunner,
    call: &InvokeParams,
    emit: &mut Emit<'_>,
) -> Result<InvokeResult, String> {
    let mut branch = None;
    let mut restart = "all";
    let mut post = None;
    let mut remote_bin = "oxidepm";
    let args = parse_args(call, |flag, value| {
        match flag {
            "--branch" if valid_branch(value) => branch = Some(value),
            "--branch" => return Err("Invalid Git branch name".into()),
            "--restart" => restart = value,
            "--post" => post = Some(value),
            "--remote-bin" => remote_bin = value,
            _ => return Err(format!("Unknown argument {flag}")),
        }
        Ok(())
    })?;
    let host = args.host;
    let cwd = cwd_command(args.cwd);
    let dry_run = args.dry_run;
    let resolution = branch.is_none().then(|| {
        Step::new(
            "resolve_branch",
            host,
            format!("{cwd}git rev-parse --abbrev-ref HEAD"),
        )
    });
    let mut steps = vec![Step::new(
        "git_pull",
        host,
        pull_command(&cwd, branch.unwrap_or("<current branch>")),
    )];
    if let Some(command) = post {
        steps.push(Step::new("post", host, format!("{cwd}{command}")));
    }
    steps.push(Step::new(
        "restart",
        host,
        format!(
            "{} restart {}",
            shell_quote(remote_bin),
            shell_quote(restart)
        ),
    ));

    let mut fields = json!({
        "host": host,
        "branch": branch.or(dry_run.then_some("<current branch>")),
        "dry_run": dry_run,
        "post": post,
    });
    check_plan(&fields, resolution.iter().chain(&steps))?;

    let mut completed = Vec::new();
    if let Some(mut step) = resolution {
        step.run("Resolving the remote branch", dry_run, runner, emit)
            .await;
        if !dry_run {
            let remote = step.stdout.trim_end_matches(['\r', '\n']);
            if step.failed() || remote == "HEAD" || !valid_branch(remote) {
                step.ok = Some(false);
                emit(
                    OutputStream::Stderr,
                    "Could not resolve the remote branch. Pass --branch to select one.",
                );
                return Ok(finish(1, fields, vec![step]));
            }
            let mut resolved_fields = fields.clone();
            resolved_fields["branch"] = json!(remote);
            steps[0] = Step::new("git_pull", host, pull_command(&cwd, remote));
            if check_plan(&resolved_fields, std::iter::once(&step).chain(&steps)).is_err() {
                step.ok = Some(false);
                emit(
                    OutputStream::Stderr,
                    "The remote branch is too long to report. Pass --branch to select one.",
                );
                return Ok(finish(1, fields, vec![step]));
            }
            fields = resolved_fields;
        }
        completed.push(step);
    }

    for mut step in steps {
        let (label, failure, code) = match step.step {
            "git_pull" => ("Pulling from origin", "Git pull failed", 1),
            "post" => (
                "Running the post-deploy command",
                "Post-deploy command failed",
                1,
            ),
            _ => (
                "Restarting remote processes",
                "Deploy incomplete: remote restart failed",
                3,
            ),
        };
        step.run(label, dry_run, runner, emit).await;
        let failed = step.failed();
        completed.push(step);
        if failed {
            emit(OutputStream::Stderr, failure);
            return Ok(finish(code, fields, completed));
        }
    }
    if !dry_run {
        emit(OutputStream::Stdout, "Deploy complete");
    }
    Ok(finish(0, fields, completed))
}

#[cfg(test)]
mod tests {
    use oxidepm_adapter_proto::messages::OutputStream;
    use serde_json::json;

    use crate::service::{invoke, tests::call};
    use crate::ssh::tests::{output, FakeSsh};

    #[tokio::test]
    async fn deploy_defaults_branch_to_remote_head_not_prod() {
        let ssh = FakeSsh::new([output(0, "main\n"), output(0, ""), output(0, "")]);
        let result = invoke(&ssh, call("deploy", &["myhost"]), &mut |_, _| {}).await;
        assert_eq!(result.exit_code, 0);
        let calls = ssh.calls();
        assert_eq!(calls[0][6], "git rev-parse --abbrev-ref HEAD");
        assert_eq!(calls[1][6], "git pull origin -- 'main'");
        let data = result.result.unwrap();
        assert_eq!(data["branch"], "main");
        assert_eq!(data["steps"][0]["step"], "resolve_branch");
    }

    #[tokio::test]
    async fn deploy_failed_restart_exits_3_and_says_incomplete() {
        let ssh = FakeSsh::new([output(0, "updated\n"), output(1, "")]);
        let mut events = Vec::new();
        let result = invoke(
            &ssh,
            call("deploy", &["myhost", "--branch", "main"]),
            &mut |stream, text| events.push((stream, text.to_string())),
        )
        .await;
        assert_eq!(result.exit_code, 3);
        assert!(events.iter().any(|(stream, text)| {
            *stream == OutputStream::Stderr && text == "Deploy incomplete: remote restart failed"
        }));
        assert!(!events
            .iter()
            .any(|(_, text)| text.contains("Deploy complete")));
        let steps = result.result.unwrap()["steps"].as_array().unwrap().clone();
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0]["step"], "git_pull");
        assert_eq!(steps[0]["stdout"], "updated\n");
        assert_eq!(steps[0]["ok"], true);
        assert_eq!(steps[1]["step"], "restart");
        assert_eq!(steps[1]["ok"], false);
        assert_eq!(steps[1]["exit_code"], 1);
    }

    #[tokio::test]
    async fn deploy_dry_run_reports_exact_argv_without_double_quoting() {
        let ssh = FakeSsh::new([]);
        let mut events = Vec::new();
        let result = invoke(
            &ssh,
            call(
                "deploy",
                &[
                    "myhost",
                    "--cwd",
                    "/srv/a'b",
                    "--dry-run",
                    "--post",
                    "npm run build",
                    "--remote-bin",
                    "/srv/my bin/oxidepm",
                    "--restart",
                    "worker's job",
                ],
            ),
            &mut |stream, text| {
                assert_eq!(stream, OutputStream::Stdout);
                events.push(serde_json::from_str::<serde_json::Value>(text).unwrap());
            },
        )
        .await;
        assert_eq!(result.exit_code, 0);
        assert!(ssh.calls().is_empty());
        let data = result.result.unwrap();
        assert_eq!(data["branch"], "<current branch>");
        assert_eq!(data["post"], "npm run build");
        assert_eq!(data["dry_run"], true);
        let expected = [
            r#"cd '/srv/a'"'"'b' && git rev-parse --abbrev-ref HEAD"#,
            r#"cd '/srv/a'"'"'b' && git pull origin -- '<current branch>'"#,
            r#"cd '/srv/a'"'"'b' && npm run build"#,
            r#"'/srv/my bin/oxidepm' restart 'worker'"'"'s job'"#,
        ];
        assert_eq!(events.len(), expected.len());
        for (i, command) in expected.into_iter().enumerate() {
            let argv = json!([
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=10",
                "--",
                "myhost",
                command
            ]);
            assert_eq!(events[i], argv);
            assert_eq!(data["steps"][i]["argv"], argv);
            assert_eq!(data["steps"][i]["command"], command);
            assert!(data["steps"][i]["exit_code"].is_null());
        }
    }

    #[tokio::test]
    async fn deploy_stops_after_branch_resolution_pull_or_post_failure() {
        for (args, replies, message) in [
            (&["myhost"][..], vec![output(0, "HEAD\n")], "Pass --branch"),
            (&["myhost"][..], vec![output(255, "")], "Pass --branch"),
            (
                &["myhost", "--branch", "main", "--post", "build"][..],
                vec![output(1, "")],
                "Git pull failed",
            ),
            (
                &["myhost", "--branch", "main", "--post", "build"][..],
                vec![output(0, ""), output(1, "")],
                "Post-deploy command failed",
            ),
        ] {
            let expected = replies.len();
            let ssh = FakeSsh::new(replies);
            let mut stderr = String::new();
            let result = invoke(&ssh, call("deploy", args), &mut |stream, text| {
                if stream == OutputStream::Stderr {
                    stderr.push_str(text);
                }
            })
            .await;
            assert_eq!(result.exit_code, 1);
            assert_eq!(ssh.calls().len(), expected);
            assert_eq!(
                result.result.unwrap()["steps"].as_array().unwrap().len(),
                expected
            );
            assert!(stderr.contains(message), "{stderr:?}");
        }
    }
}
