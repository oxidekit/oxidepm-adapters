use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use oxidepm_adapter_proto::messages::{EventParams, InvokeParams, InvokeResult};
use oxidepm_adapter_sdk::{Output, Service};
use serde::Serialize;

use crate::args::{Args, USAGE};
use crate::checks::{self, CheckResult, CheckStatus, ProjectType};

pub struct Check;

#[derive(Serialize)]
struct Summary {
    project: String,
    project_type: Option<ProjectType>,
    checks: Vec<CheckResult>,
    warnings: usize,
    errors: usize,
    fixed: usize,
}

#[async_trait]
impl Service for Check {
    async fn event(self: Arc<Self>, _: EventParams) {}
    async fn shutdown(self: Arc<Self>) {}

    async fn invoke(self: Arc<Self>, call: InvokeParams, out: Output) -> InvokeResult {
        let result = tokio::task::spawn_blocking(move || run(call, &out)).await;
        result.unwrap_or_else(|_| InvokeResult {
            exit_code: 1,
            result: Some(serde_json::json!({"error": "The project check did not finish"})),
        })
    }
}

fn run(call: InvokeParams, out: &Output) -> InvokeResult {
    if matches!(call.argv.as_slice(), [flag] if flag == "--help" || flag == "-h") {
        out.stdout(USAGE);
        return InvokeResult {
            exit_code: 0,
            result: Some(serde_json::json!({"usage": USAGE})),
        };
    }

    let args = match Args::parse(&call.argv) {
        Ok(args) => args,
        Err(error) => {
            out.stderr(format!("{error}\n{USAGE}"));
            return InvokeResult {
                exit_code: 2,
                result: Some(serde_json::json!({"error": error})),
            };
        }
    };
    let cwd = Path::new(&call.cwd);
    let target = cwd.join(args.target);
    let (project, project_type, checks) = match resolve_project(cwd, &target, &args) {
        Ok(dir) => {
            out.stdout(format!("Checking project: {}", dir.display()));
            let (project_type, checks) = checks::run(&dir, &args);
            (dir, Some(project_type), checks)
        }
        Err(error) => (
            target,
            None,
            vec![CheckResult::new(CheckStatus::Error, error)],
        ),
    };
    let count = |status| checks.iter().filter(|check| check.status == status).count();
    let (warnings, errors, fixed) = (
        count(CheckStatus::Warn),
        count(CheckStatus::Error),
        count(CheckStatus::Fixed),
    );

    for check in &checks {
        let mut line = format!("{} {}", check.status.prefix(), check.message);
        if let Some(hint) = &check.fix_hint {
            line.push_str(&format!("\n       {hint}"));
        }
        if check.status == CheckStatus::Error {
            out.stderr(line);
        } else {
            out.stdout(line);
        }
    }
    if warnings > 0 || errors > 0 {
        out.stdout(format!("Issues: {errors} error(s), {warnings} warning(s)"));
    } else {
        out.stdout("All checks passed!");
    }

    let summary = Summary {
        project: project.display().to_string(),
        project_type,
        checks,
        warnings,
        errors,
        fixed,
    };
    InvokeResult {
        exit_code: i32::from(errors > 0 || (args.strict && warnings > 0)),
        result: Some(serde_json::json!(summary)),
    }
}

fn resolve_project(cwd: &Path, target: &Path, args: &Args<'_>) -> Result<PathBuf, String> {
    let real = target.canonicalize().map_err(|e| {
        if e.kind() == ErrorKind::NotFound {
            format!("target does not exist: {}", target.display())
        } else {
            format!("Cannot resolve target {}: {e}", target.display())
        }
    })?;
    let project = if real.is_dir() {
        real
    } else if real.is_file() {
        real.parent().expect("a file has a parent").to_path_buf()
    } else {
        return Err(format!(
            "target is not a file or directory: {}",
            real.display()
        ));
    };
    if args.fix || !args.set_env.is_empty() {
        let cwd = cwd
            .canonicalize()
            .map_err(|e| format!("Cannot resolve working directory {}: {e}", cwd.display()))?;
        if !project.starts_with(&cwd) {
            return Err(format!(
                "--fix and --set-env can only write within the current working directory; run check from {}",
                project.display()
            ));
        }
    }
    Ok(project)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::symlink};

    #[test]
    fn check_resolves_a_symlinked_working_directory_before_allowing_writes() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let cwd = root.path().join("link");
        fs::create_dir(&project).unwrap();
        symlink(&project, &cwd).unwrap();

        for argv in [
            vec!["--fix".into()],
            vec!["--set-env".into(), "KEY=value".into()],
        ] {
            let args = Args::parse(&argv).unwrap();
            assert_eq!(
                resolve_project(&cwd, &cwd, &args).unwrap(),
                project.canonicalize().unwrap()
            );
            assert!(resolve_project(&cwd, root.path(), &args).is_err());
        }
    }
}
