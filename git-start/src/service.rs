use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use oxidepm_adapter_proto::messages::{EventParams, InvokeParams, InvokeResult};
use oxidepm_adapter_sdk::{Output, Service};
use serde::Serialize;
use serde_json::{json, Value};

use crate::deps;
use crate::git::{self, Commands, Runner};
use crate::spec::Launch;
use crate::url::RepoUrl;

const USAGE: &str = "Usage: oxidepm [--dry-run] git-start <URL> [--branch BRANCH] [--name NAME] [--runtime RUNTIME] [--script SCRIPT | --bin BIN] [--port PORT] [--env KEY=VALUE] [--offline] [--install] [--allow-scripts] [--no-start]";

pub struct GitStart {
    pub state: PathBuf,
}

struct Args<'a> {
    branch: Option<&'a str>,
    offline: bool,
    install: bool,
    allow_scripts: bool,
    no_start: bool,
    launch: Launch<'a>,
}

impl<'a> Args<'a> {
    fn parse(argv: &'a [String]) -> Result<(RepoUrl<'a>, Self), String> {
        let mut repo = None;
        let mut args = Self {
            branch: None,
            offline: false,
            install: false,
            allow_scripts: false,
            no_start: false,
            launch: Launch {
                name: None,
                runtime: None,
                script: None,
                bin: None,
                port: None,
                env: Default::default(),
            },
        };
        let mut argv = argv.iter().map(String::as_str);
        let mut literal = false;
        while let Some(arg) = argv.next() {
            if literal || !arg.starts_with('-') {
                if repo.is_some() {
                    return Err("Expected one repository URL".into());
                }
                repo = Some(RepoUrl::parse(arg)?);
                continue;
            }
            let (flag, attached) = arg
                .split_once('=')
                .map_or((arg, None), |(key, val)| (key, Some(val)));
            let mut value = || {
                attached
                    .or_else(|| argv.next())
                    .filter(|val| !val.is_empty())
                    .ok_or_else(|| format!("{flag} requires a value"))
            };
            match flag {
                "--" if attached.is_none() => literal = true,
                "--offline" if attached.is_none() => args.offline = true,
                "--install" if attached.is_none() => args.install = true,
                "--allow-scripts" if attached.is_none() => args.allow_scripts = true,
                "--no-start" if attached.is_none() => args.no_start = true,
                "--branch" => {
                    let branch = value()?;
                    if !git::valid_branch(branch) {
                        return Err("Invalid Git branch name".into());
                    }
                    args.branch = Some(branch);
                }
                "--name" => args.launch.name = Some(value()?),
                "--runtime" | "--mode" => {
                    args.launch.runtime = Some(
                        value()?
                            .parse()
                            .map_err(|e: oxidepm_core::Error| e.to_string())?,
                    );
                }
                "--script" => args.launch.script = Some(value()?),
                "--bin" => args.launch.bin = Some(value()?),
                "--port" => {
                    args.launch.port = Some(
                        value()?
                            .parse::<u16>()
                            .ok()
                            .filter(|n| *n > 0)
                            .ok_or("Invalid port")?,
                    );
                }
                "--env" => {
                    let (key, val) = value()?
                        .split_once('=')
                        .filter(|(key, _)| !key.is_empty())
                        .ok_or("Expected --env KEY=VALUE")?;
                    args.launch.env.insert(key.to_string(), val.to_string());
                }
                _ => return Err(format!("Unknown argument {arg}")),
            }
        }
        let repo = repo.ok_or("A repository URL is required")?;
        args.launch.validate(repo.name())?;
        Ok((repo, args))
    }
}

#[async_trait]
impl Service for GitStart {
    async fn event(self: Arc<Self>, _: EventParams) {}
    async fn shutdown(self: Arc<Self>) {}

    async fn invoke(self: Arc<Self>, call: InvokeParams, out: Output) -> InvokeResult {
        tokio::task::spawn_blocking(move || run(&self.state, call, &out))
            .await
            .unwrap_or_else(|_| failure(1, "Git start did not finish".into()))
    }
}

fn success(result: Value) -> InvokeResult {
    InvokeResult {
        exit_code: 0,
        result: Some(result),
    }
}

fn failure(exit_code: i32, error: String) -> InvokeResult {
    InvokeResult {
        exit_code,
        result: Some(json!({ "error": error })),
    }
}

fn run(state: &Path, call: InvokeParams, out: &Output) -> InvokeResult {
    if matches!(call.argv.as_slice(), [flag] if flag == "--help" || flag == "-h") {
        out.stdout(USAGE);
        return success(json!({ "usage": USAGE }));
    }
    let (repo, args) = match Args::parse(&call.argv) {
        Ok(args) => args,
        Err(error) => {
            out.stderr(format!("{error}\n{USAGE}"));
            return failure(2, error);
        }
    };
    let dir = repo.clone_dir(state);
    if call.dry_run {
        let commands = git::planned_commands(&repo, &dir, args.branch, args.offline);
        out.stdout(format!(
            "Repository: {}\nCheckout: {}",
            repo.original,
            dir.display()
        ));
        for command in &commands {
            out.stdout(command);
        }
        if args.install {
            out.stdout(format!(
                "If package.json is present: npm {}",
                deps::install_args(args.allow_scripts, args.offline).join(" ")
            ));
        } else {
            out.stdout("Dependency install: skipped");
        }
        return success(json!({
            "clone_dir": dir,
            "commands": commands,
            "install": args.install,
            "allow_scripts": args.allow_scripts,
            "auto_start": !args.no_start
        }));
    }

    match prepare(&repo, args, &dir, out) {
        Ok(result) => success(json!(result)),
        Err(error) => {
            out.stderr(&error);
            failure(1, error)
        }
    }
}

#[derive(Serialize)]
struct Prepared<'a> {
    clone_dir: &'a Path,
    commit: String,
    subject: String,
    suggested_spec: oxidepm_core::AppSpec,
    auto_start: bool,
}

fn prepare<'a>(
    repo: &RepoUrl<'_>,
    args: Args<'_>,
    dir: &'a Path,
    out: &Output,
) -> Result<Prepared<'a>, String> {
    let _lock = git::lock(dir)?;
    let runner = Commands;
    git::prepare(&runner, repo, dir, args.branch, args.offline, |msg| {
        out.stdout(msg)
    })?;
    if args.offline {
        out.stderr("Offline: using the existing checkout without fetching");
    }
    if args.install {
        if dir.join("package.json").is_file() {
            out.stdout("Installing Node dependencies");
            deps::install(&runner, dir, args.allow_scripts, args.offline)?;
        } else {
            out.stdout("Dependency install: no package.json found");
        }
    }
    let commit = runner.run(dir, "git", &["rev-parse", "--short", "HEAD"])?;
    let subject = runner.run(dir, "git", &["log", "-1", "--format=%s"])?;
    out.stdout(format!("Commit: {commit} {subject}"));
    Ok(Prepared {
        clone_dir: dir,
        commit,
        subject,
        suggested_spec: args.launch.spec(dir, repo.name())?,
        auto_start: !args.no_start,
    })
}
