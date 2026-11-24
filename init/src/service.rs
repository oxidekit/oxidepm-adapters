use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use oxidepm_adapter_proto::messages::{EventParams, InvokeParams, InvokeResult};
use oxidepm_adapter_sdk::{Output, Service};
use serde_json::json;

use crate::render;

pub struct Init;

const USAGE: &str = "Usage: oxidepm [--dry-run] init [DIR]";

#[async_trait]
impl Service for Init {
    async fn event(self: Arc<Self>, _: EventParams) {}
    async fn shutdown(self: Arc<Self>) {}

    async fn invoke(self: Arc<Self>, call: InvokeParams, out: Output) -> InvokeResult {
        let literal = call.argv.first().is_some_and(|flag| flag == "--");
        let args = &call.argv[usize::from(literal)..];
        let dir = match args {
            [] => ".",
            [flag] if !literal && (flag == "--help" || flag == "-h") => {
                out.stdout(USAGE);
                return InvokeResult {
                    exit_code: 0,
                    result: Some(json!({"usage": USAGE})),
                };
            }
            [dir] if literal || !dir.starts_with('-') => dir,
            _ => {
                out.stderr(USAGE);
                return InvokeResult {
                    exit_code: 2,
                    result: Some(json!({"usage": USAGE})),
                };
            }
        };
        let cwd = Path::new(&call.cwd);
        match generate(cwd, dir, call.dry_run) {
            Ok((path, contents)) => {
                if call.dry_run {
                    out.stdout(&contents);
                } else {
                    out.stdout(format!("Created {}", path.display()));
                }
                InvokeResult {
                    exit_code: 0,
                    result: Some(
                        json!({"path": path.to_string_lossy(), "config": contents, "written": !call.dry_run}),
                    ),
                }
            }
            Err(error) => {
                out.stderr(&error);
                InvokeResult {
                    exit_code: 1,
                    result: Some(json!({"error": error})),
                }
            }
        }
    }
}

fn generate(cwd: &Path, dir: &str, dry_run: bool) -> Result<(PathBuf, String), String> {
    let requested = cwd.join(dir);
    let project = requested
        .canonicalize()
        .map_err(|e| format!("Cannot open directory {}: {e}", requested.display()))?;
    if !project.is_dir() {
        return Err(format!("Not a directory: {}", project.display()));
    }
    let cwd = cwd
        .canonicalize()
        .map_err(|e| format!("Cannot resolve working directory {}: {e}", cwd.display()))?;
    if !project.starts_with(&cwd) {
        return Err("Run init from the project directory or one of its parents".into());
    }

    let path = project.join("oxidepm.config.toml");
    let exists = || {
        format!(
            "Config already exists: {}. Edit it directly.",
            path.display()
        )
    };
    match path.symlink_metadata() {
        Ok(_) => return Err(exists()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("Cannot inspect {}: {e}", path.display())),
    }
    let contents = render::render(&project)?;
    if !dry_run {
        match write_new(&path, &contents) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => return Err(exists()),
            Err(e) => return Err(format!("Cannot write {}: {e}", path.display())),
        }
    }
    Ok((path, contents))
}

fn write_new(path: &Path, contents: &str) -> io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    if let Err(error) = file.write_all(contents.as_bytes()) {
        let _ = fs::remove_file(path);
        return Err(error);
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn init_resolves_a_symlinked_working_directory_before_allowing_writes() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let cwd = root.path().join("link");
        fs::create_dir(&project).unwrap();
        symlink(&project, &cwd).unwrap();
        fs::write(
            project.join("package.json"),
            r#"{"name":"example","scripts":{"start":"node index.js"}}"#,
        )
        .unwrap();

        let (path, contents) = generate(&cwd, ".", false).unwrap();
        assert_eq!(
            path,
            project.canonicalize().unwrap().join("oxidepm.config.toml")
        );
        assert_eq!(fs::read_to_string(path).unwrap(), contents);
        assert_eq!(
            generate(&cwd, "..", false).unwrap_err(),
            "Run init from the project directory or one of its parents"
        );
    }
}
