use oxidepm_adapter_proto::messages::{InvokeParams, InvokeResult, OutputStream};
use serde_json::json;

use crate::service::{check_plan, finish, parse_args, Emit};
use crate::shell::cwd_command;
use crate::ssh::{SshRunner, Step};

pub const USAGE: &str = "Usage: oxidepm [--dry-run] diff <HOST> [--cwd DIR] [--count N]";

pub fn commands(cwd: &str, count: u16) -> [(&'static str, &'static str, String); 3] {
    [
        (
            "git_log",
            "Recent commits",
            format!("{cwd}git log --oneline --no-decorate -n {count}"),
        ),
        (
            "git_status",
            "Working tree",
            format!("{cwd}git status --short"),
        ),
        (
            "git_diff",
            "Diff stat",
            format!("{cwd}git diff --stat HEAD"),
        ),
    ]
}

pub async fn run(
    runner: &impl SshRunner,
    call: &InvokeParams,
    emit: &mut Emit<'_>,
) -> Result<InvokeResult, String> {
    let mut count = 5;
    let args = parse_args(call, |flag, value| {
        if flag != "--count" {
            return Err(format!("Unknown argument {flag}"));
        }
        count = value
            .parse::<u16>()
            .ok()
            .filter(|count| (1..=1000).contains(count))
            .ok_or("--count must be a number between 1 and 1000")?;
        Ok(())
    })?;
    let fields = json!({ "host": args.host, "dry_run": args.dry_run });
    let (labels, mut steps): (Vec<_>, Vec<_>) = commands(&cwd_command(args.cwd), count)
        .into_iter()
        .map(|(name, label, command)| (label, Step::new(name, args.host, command)))
        .unzip();
    check_plan(&fields, &steps)?;

    let mut failed = false;
    for (step, label) in steps.iter_mut().zip(labels) {
        step.run(label, args.dry_run, runner, emit).await;
        if step.failed() {
            emit(OutputStream::Stderr, &format!("{label} failed"));
            failed = true;
        } else if !args.dry_run && step.step == "git_status" && step.stdout.trim().is_empty() {
            emit(OutputStream::Stdout, "Working tree clean");
        }
    }
    Ok(finish(i32::from(failed), fields, steps))
}

#[cfg(test)]
mod tests {
    use std::io;

    use crate::service::{invoke, tests::call};
    use crate::ssh::tests::{output, FakeSsh};

    #[tokio::test]
    async fn diff_validates_count_and_previews_on_global_dry_run() {
        let ssh = FakeSsh::new([]);
        for count in ["0", "1001", "no"] {
            let result = invoke(
                &ssh,
                call("diff", &["origin", "--count", count]),
                &mut |_, _| {},
            )
            .await;
            assert_eq!(result.exit_code, 2);
            assert!(result.result.unwrap()["error"]
                .as_str()
                .unwrap()
                .contains("1 and 1000"));
        }
        let mut dry_run = call("diff", &["origin", "--count", "1000"]);
        dry_run.dry_run = true;
        let result = invoke(&ssh, dry_run, &mut |_, _| {}).await;
        assert_eq!(result.exit_code, 0);
        let data = result.result.unwrap();
        assert_eq!(data["dry_run"], true);
        assert_eq!(
            data["steps"][0]["command"],
            "git log --oneline --no-decorate -n 1000"
        );
        assert!(ssh.calls().is_empty());
    }

    #[tokio::test]
    async fn large_diff_output_keeps_argv_and_status_in_the_reply() {
        let remote_output = format!("a{}", "é".repeat(48 * 1024));
        let ssh = FakeSsh::new((0..3).map(|_| output(0, &remote_output)));
        let mut relayed = String::new();
        let result = invoke(&ssh, call("diff", &["origin"]), &mut |_, text| {
            if text.starts_with(['a', 'é']) {
                relayed.push_str(text);
            }
        })
        .await;
        assert_eq!(result.exit_code, 0);
        assert_eq!(relayed, remote_output.repeat(3));
        assert!(
            serde_json::to_vec(&result).unwrap().len()
                < oxidepm_adapter_proto::Limits::PROTOCOL_1.max_line
        );
        let data = result.result.unwrap();
        assert_eq!(data["steps"][2]["argv"][6], "git diff --stat HEAD");
        assert_eq!(data["steps"][2]["exit_code"], 0);
        assert!(data["steps"][2]["stdout"]
            .as_str()
            .unwrap()
            .contains("truncated in result"));
    }

    #[tokio::test]
    async fn diff_records_a_failed_ssh_step() {
        let ssh = FakeSsh::new([
            output(0, "latest\n"),
            Err(io::Error::new(io::ErrorKind::NotFound, "ssh unavailable")),
            output(0, "file | 1 +\n"),
        ]);
        let result = invoke(&ssh, call("diff", &["origin"]), &mut |_, _| {}).await;
        assert_eq!(result.exit_code, 1);
        let data = result.result.unwrap();
        assert_eq!(data["steps"][1]["ok"], false);
        assert!(data["steps"][1]["stderr"]
            .as_str()
            .unwrap()
            .contains("ssh unavailable"));
        assert_eq!(data["steps"][2]["stdout"], "file | 1 +\n");
    }
}
