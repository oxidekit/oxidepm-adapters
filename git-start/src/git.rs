use std::fs::{self, File, OpenOptions};
use std::io::ErrorKind;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Command, Stdio};

use nix::fcntl::{Flock, FlockArg};

use crate::url::RepoUrl;

pub trait Runner {
    fn run(&self, dir: &Path, program: &str, args: &[&str]) -> Result<String, String>;
}

pub struct Commands;

impl Runner for Commands {
    fn run(&self, dir: &Path, program: &str, args: &[&str]) -> Result<String, String> {
        const MAX_STDERR_CHARS: usize = 2048;

        let mut command = Command::new(program);
        command
            .current_dir(dir)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if program == "git" {
            command.env("GIT_TERMINAL_PROMPT", "0");
        } else if program == "npm" {
            command.env("npm_config_cache", dir.join(".git/oxidepm-npm-cache"));
        }
        let child = command
            .spawn()
            .map_err(|err| format!("Could not run {program}: {err}"))?;
        let pid = child.id();
        let output = child
            .wait_with_output()
            .map_err(|err| format!("Could not wait for {program}: {err}"))?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout)
                .trim_end()
                .to_string())
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stderr = stderr.trim();
            let start = stderr
                .char_indices()
                .rev()
                .nth(MAX_STDERR_CHARS - 1)
                .map_or(0, |(offset, _)| offset);
            let detail = if stderr.is_empty() {
                "no stderr output"
            } else {
                &stderr[start..]
            };
            let status = output.status;
            let status = if status.signal().is_some() {
                format!("child PID {pid}, {status}")
            } else {
                status.to_string()
            };
            Err(format!(
                "{program} {} failed ({status}): {detail}",
                args.first().copied().unwrap_or_default()
            ))
        }
    }
}

pub fn lock(dir: &Path) -> Result<Flock<File>, String> {
    let parent = dir.parent().expect("checkout has a parent");
    fs::create_dir_all(parent)
        .map_err(|err| format!("Could not create {}: {err}", parent.display()))?;
    let path = dir.with_extension("lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|err| format!("Could not open {}: {err}", path.display()))?;
    Flock::lock(file, FlockArg::LockExclusiveNonblock)
        .map_err(|(_, err)| format!("Could not lock checkout {}: {err}", dir.display()))
}

pub fn prepare(
    runner: &impl Runner,
    repo: &RepoUrl<'_>,
    dir: &Path,
    branch: Option<&str>,
    offline: bool,
    progress: impl Fn(&str),
) -> Result<(), String> {
    let parent = dir.parent().expect("checkout has a parent");
    let checkout = dir.join(".git");
    if let Ok(meta) = dir.symlink_metadata() {
        if !meta.is_dir() || !checkout.symlink_metadata().is_ok_and(|meta| meta.is_dir()) {
            return Err(format!("{} exists without a git checkout", dir.display()));
        }
        let origin = runner.run(dir, "git", &["remote", "get-url", "origin"])?;
        if RepoUrl::parse(&origin).map(|url| url.normalized).as_deref()
            != Ok(repo.normalized.as_str())
        {
            return Err(format!(
                "The origin of {} does not match the requested repository",
                dir.display()
            ));
        }
        if offline {
            if let Some(branch) = branch {
                runner.run(dir, "git", &["switch", "--no-guess", "--", branch])?;
            }
            return Ok(());
        }
        progress("Updating the existing checkout");
        if let Some(branch) = branch {
            let remote_ref = format!("refs/remotes/origin/{branch}");
            let refspec = format!("+refs/heads/{branch}:{remote_ref}");
            runner.run(dir, "git", &["fetch", "--", "origin", &refspec])?;
            // A shallow clone only tracks its original branch. Add a local branch when needed.
            if runner
                .run(dir, "git", &["switch", "--no-guess", "--", branch])
                .is_err()
            {
                runner.run(
                    dir,
                    "git",
                    &["remote", "set-branches", "--add", "origin", branch],
                )?;
                runner.run(
                    dir,
                    "git",
                    &["switch", "--track", "-c", branch, "--", &remote_ref],
                )?;
            }
            let head_ref = format!("refs/heads/{branch}");
            runner.run(
                dir,
                "git",
                &["pull", "--ff-only", "--", "origin", &head_ref],
            )?;
        } else {
            runner.run(dir, "git", &["pull", "--ff-only"])?;
        }
        return Ok(());
    }

    if offline {
        return Err("Offline: no existing checkout to use".into());
    }
    let partial = dir.with_extension("partial");
    let target = partial.to_str().ok_or("Checkout path is not UTF-8")?;
    // Publish the checkout only after git succeeds. A cancelled clone can be retried.
    remove_partial(&partial)?;
    progress("Cloning the repository");
    if let Err(err) = runner.run(parent, "git", &clone_args(repo, branch, target)) {
        let _ = remove_partial(&partial);
        return Err(err);
    }
    fs::rename(&partial, dir)
        .map_err(|err| format!("Could not move the checkout to {}: {err}", dir.display()))
}

fn remove_partial(path: &Path) -> Result<(), String> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
        Err(err) => Err(format!("Could not clear {}: {err}", path.display())),
    }
}

pub fn planned_commands(
    repo: &RepoUrl<'_>,
    dir: &Path,
    branch: Option<&str>,
    offline: bool,
) -> Vec<String> {
    let mut commands = Vec::new();
    if dir.join(".git").is_dir() {
        commands.push("git remote get-url origin".to_string());
        if let Some(branch) = branch {
            let remote_ref = format!("refs/remotes/origin/{branch}");
            if !offline {
                commands.push(format!(
                    "git fetch -- origin +refs/heads/{branch}:{remote_ref}"
                ));
            }
            commands.push(format!("git switch --no-guess -- {branch}"));
            if !offline {
                commands.push(format!(
                    "If needed: git remote set-branches --add origin {branch}"
                ));
                commands.push(format!(
                    "If needed: git switch --track -c {branch} -- {remote_ref}"
                ));
                commands.push(format!("git pull --ff-only -- origin refs/heads/{branch}"));
            }
        } else if !offline {
            commands.push("git pull --ff-only".to_string());
        }
    } else if offline {
        return vec!["Offline: no existing checkout to use".to_string()];
    } else {
        let partial = dir.with_extension("partial");
        commands.push(format!(
            "git {}",
            clone_args(repo, branch, &partial.to_string_lossy()).join(" ")
        ));
    }
    commands.push("git rev-parse --short HEAD".to_string());
    commands.push("git log -1 --format=%s".to_string());
    commands
}

pub fn clone_args<'a>(
    repo: &RepoUrl<'a>,
    branch: Option<&'a str>,
    target: &'a str,
) -> Vec<&'a str> {
    let mut args = vec!["clone", "--depth", "1"];
    if let Some(branch) = branch {
        args.extend(["--branch", branch]);
    }
    args.extend(["--", repo.original, target]);
    args
}

pub fn valid_branch(branch: &str) -> bool {
    !branch.is_empty()
        && !matches!(branch, "HEAD" | "@")
        && !branch.starts_with('-')
        && !branch.ends_with('.')
        && !branch.contains("..")
        && !branch.contains("@{")
        && !branch
            .chars()
            .any(|c| c.is_control() || c.is_whitespace() || "~^:?*[\\".contains(c))
        && branch
            .split('/')
            .all(|part| !part.is_empty() && !part.starts_with('.') && !part.ends_with(".lock"))
}

#[cfg(test)]
pub mod tests {
    use std::cell::RefCell;

    use super::*;

    #[test]
    fn command_failure_keeps_the_end_of_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let prefix = "é".repeat(2048);
        let error = Commands
            .run(
                dir.path(),
                "/bin/sh",
                &[
                    "-c",
                    "printf '%s\\nfinal diagnostic\\n' \"$1\" >&2; exit 1",
                    "sh",
                    &prefix,
                ],
            )
            .unwrap_err();
        let (_, detail) = error.split_once("): ").unwrap();
        assert_eq!(detail.chars().count(), 2048);
        assert!(detail.ends_with("\nfinal diagnostic"), "{error}");
    }

    #[test]
    fn silent_signal_reports_which_child_exited() {
        let dir = tempfile::tempdir().unwrap();
        let error = Commands
            .run(
                dir.path(),
                "/bin/sh",
                &[
                    "-c",
                    "ulimit -c 0; printf '%s' \"$$\" > child.pid; kill -s ABRT \"$$\"",
                ],
            )
            .unwrap_err();
        let pid = fs::read_to_string(dir.path().join("child.pid")).unwrap();
        assert!(
            error.contains(&format!("child PID {pid}, signal: 6")),
            "{error}"
        );
        assert!(error.ends_with("no stderr output"), "{error}");
    }

    #[derive(Default)]
    pub struct GitRunner {
        pub origin: String,
        pub fail_pull: bool,
        pub calls: RefCell<Vec<Vec<String>>>,
    }

    impl Runner for GitRunner {
        fn run(&self, _: &Path, program: &str, args: &[&str]) -> Result<String, String> {
            self.calls.borrow_mut().push(
                std::iter::once(program)
                    .chain(args.iter().copied())
                    .map(String::from)
                    .collect(),
            );
            match args {
                ["remote", "get-url", "origin"] => Ok(self.origin.clone()),
                ["pull", ..] if self.fail_pull => Err("could not pull".into()),
                _ => Ok(String::new()),
            }
        }
    }

    const URL: &str = "https://example.com/org/app.git";

    fn existing() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join(".git")).unwrap();
        dir
    }

    #[test]
    fn existing_clone_with_mismatched_origin_is_refused() {
        let runner = GitRunner {
            origin: "https://another.example/org/app.git".into(),
            ..Default::default()
        };
        let repo = RepoUrl::parse(URL).unwrap();
        let dir = existing();
        assert!(prepare(&runner, &repo, dir.path(), None, false, |_| {})
            .unwrap_err()
            .contains("origin"));
        assert_eq!(
            *runner.calls.borrow(),
            [vec!["git", "remote", "get-url", "origin"]]
        );
    }

    #[test]
    fn pull_failure_is_an_error_unless_offline() {
        let runner = GitRunner {
            origin: URL.into(),
            fail_pull: true,
            ..Default::default()
        };
        let repo = RepoUrl::parse(URL).unwrap();
        let dir = existing();
        assert_eq!(
            prepare(&runner, &repo, dir.path(), None, false, |_| {}),
            Err("could not pull".into())
        );
        runner.calls.borrow_mut().clear();
        prepare(&runner, &repo, dir.path(), None, true, |_| {}).unwrap();
        assert_eq!(
            *runner.calls.borrow(),
            [vec!["git", "remote", "get-url", "origin"]]
        );
    }

    #[test]
    fn branch_is_checked_out_on_existing_clone() {
        let runner = GitRunner {
            origin: URL.into(),
            ..Default::default()
        };
        let repo = RepoUrl::parse(URL).unwrap();
        let dir = existing();
        prepare(&runner, &repo, dir.path(), Some("feat"), false, |_| {}).unwrap();
        assert_eq!(
            *runner.calls.borrow(),
            [
                vec!["git", "remote", "get-url", "origin"],
                vec![
                    "git",
                    "fetch",
                    "--",
                    "origin",
                    "+refs/heads/feat:refs/remotes/origin/feat"
                ],
                vec!["git", "switch", "--no-guess", "--", "feat"],
                vec![
                    "git",
                    "pull",
                    "--ff-only",
                    "--",
                    "origin",
                    "refs/heads/feat"
                ],
            ]
        );
    }
}
