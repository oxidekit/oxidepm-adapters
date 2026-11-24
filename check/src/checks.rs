use std::collections::HashMap;
use std::fs;
use std::io::ErrorKind;
use std::path::Path;
use std::process::Command;

use serde::Serialize;

use crate::{args::Args, port};
use port::PortStatus;

#[derive(Debug, Clone, Serialize)]
pub struct CheckResult {
    pub status: CheckStatus,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix_hint: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckStatus {
    Ok,
    Warn,
    Error,
    Info,
    Fixed,
}

impl CheckStatus {
    pub fn prefix(self) -> &'static str {
        match self {
            Self::Ok => "[OK]",
            Self::Warn => "[WARN]",
            Self::Error => "[ERROR]",
            Self::Info => "[INFO]",
            Self::Fixed => "[FIX]",
        }
    }
}

impl CheckResult {
    pub fn new(status: CheckStatus, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            fix_hint: None,
        }
    }

    fn hint(mut self, hint: impl Into<String>) -> Self {
        self.fix_hint = Some(hint.into());
        self
    }
}

use CheckStatus::{Error, Fixed, Info, Warn};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ProjectType {
    NodeJs,
    Cargo,
    Generic,
}

pub fn run(dir: &Path, args: &Args<'_>) -> (ProjectType, Vec<CheckResult>) {
    let project_type = detect_project_type(dir);
    let mut results = Vec::new();
    match project_type {
        ProjectType::NodeJs => check_nodejs_project(dir, args.fix, &mut results),
        ProjectType::Cargo => check_cargo_project(dir, &mut results),
        ProjectType::Generic => {}
    }
    check_env_files(dir, args.fix, &mut results);
    check_config_files(dir, &mut results);
    if !args.set_env.is_empty() {
        handle_set_env(dir, &args.set_env, &mut results);
    }
    if let Some(port) = port::check_port_conflict(dir) {
        let result = match port.status {
            PortStatus::InUse => {
                let result = CheckResult::new(
                    Error,
                    format!("Port {} is already in use", port.desired_port),
                );
                match port.available_port {
                    Some(available) => result.hint(format!(
                        "Free the port or configure the app to use port {available}"
                    )),
                    None => result,
                }
            }
            PortStatus::Available => {
                CheckResult::new(Info, format!("Configured port: {}", port.desired_port))
            }
            PortStatus::Unknown => CheckResult::new(
                Warn,
                format!("Could not check port {} availability", port.desired_port),
            ),
        };
        results.push(result);
    }
    (project_type, results)
}

fn detect_project_type(dir: &Path) -> ProjectType {
    if dir.join("package.json").exists() {
        ProjectType::NodeJs
    } else if dir.join("Cargo.toml").exists() {
        ProjectType::Cargo
    } else {
        ProjectType::Generic
    }
}

fn check_nodejs_project(dir: &Path, fix: bool, results: &mut Vec<CheckResult>) {
    let package = fs::read_to_string(dir.join("package.json"))
        .map_err(|e| e.to_string())
        .and_then(|content| {
            serde_json::from_str::<serde_json::Value>(&content).map_err(|e| e.to_string())
        });
    match package {
        Ok(value) if value.is_object() => {
            results.push(CheckResult::new(CheckStatus::Ok, "package.json found"))
        }
        Ok(_) => {
            results.push(CheckResult::new(
                Error,
                "Invalid package.json: expected a JSON object",
            ));
            return;
        }
        Err(e) => {
            results.push(CheckResult::new(
                Error,
                format!("Invalid package.json: {e}"),
            ));
            return;
        }
    }

    let lockfiles = [
        ("pnpm-lock.yaml", "pnpm"),
        ("yarn.lock", "yarn"),
        ("package-lock.json", "npm"),
    ];
    let manager = lockfiles
        .iter()
        .find(|(lockfile, _)| dir.join(lockfile).is_file())
        .map_or("npm", |(_, manager)| *manager);

    let node_modules = dir.join("node_modules");
    if node_modules.is_dir() {
        let count = package_count(&node_modules);
        results.push(CheckResult::new(
            CheckStatus::Ok,
            format!("node_modules/ found ({count} packages)"),
        ));
    } else if fix {
        match Command::new(manager)
            .arg("install")
            .current_dir(dir)
            .output()
        {
            Ok(output) if output.status.success() => {
                let count = package_count(&node_modules);
                results.push(CheckResult::new(
                    Fixed,
                    format!("node_modules/ installed ({count} packages)"),
                ));
            }
            Ok(output) => {
                let why = String::from_utf8_lossy(&output.stderr);
                results.push(CheckResult::new(
                    Error,
                    format!(
                        "{manager} install failed ({}): {}",
                        output.status,
                        why.trim()
                    ),
                ));
            }
            Err(e) => results.push(CheckResult::new(
                Error,
                format!("{manager} install failed: {e}"),
            )),
        }
    } else {
        results.push(
            CheckResult::new(
                Warn,
                format!("node_modules/ missing - run `{manager} install`"),
            )
            .hint("Use --fix to auto-install dependencies"),
        );
    }

    if let Some((lockfile, _)) = lockfiles
        .iter()
        .find(|(lockfile, _)| dir.join(lockfile).is_file())
    {
        results.push(CheckResult::new(
            CheckStatus::Ok,
            format!("{lockfile} found"),
        ));
    } else {
        results.push(
            CheckResult::new(
                Warn,
                "No lockfile found (package-lock.json, yarn.lock, or pnpm-lock.yaml)",
            )
            .hint("Run `npm install` to generate package-lock.json"),
        );
    }
}

fn package_count(dir: &Path) -> usize {
    fs::read_dir(dir).map_or(0, |entries| entries.flatten().count())
}

fn check_cargo_project(dir: &Path, results: &mut Vec<CheckResult>) {
    if !dir.join("Cargo.toml").is_file() {
        results.push(CheckResult::new(Error, "Cargo.toml is not a file"));
        return;
    }
    results.push(CheckResult::new(CheckStatus::Ok, "Cargo.toml found"));

    if dir.join("Cargo.lock").exists() {
        results.push(CheckResult::new(CheckStatus::Ok, "Cargo.lock found"));
    } else {
        results.push(
            CheckResult::new(Warn, "Cargo.lock missing - dependencies not locked")
                .hint("Run `cargo build` to generate Cargo.lock"),
        );
    }

    let target = dir.join("target");
    let build = if target.join("release").exists() {
        CheckResult::new(
            CheckStatus::Ok,
            "target/release/ found (release build available)",
        )
    } else if target.join("debug").exists() {
        CheckResult::new(
            CheckStatus::Ok,
            "target/debug/ found (debug build available)",
        )
    } else if target.is_dir() {
        CheckResult::new(Info, "target/ exists but no builds found")
            .hint("Run `cargo build` or `cargo build --release`")
    } else {
        CheckResult::new(Info, "target/ not found - project not built yet")
            .hint("Run `cargo build` to build the project")
    };
    results.push(build);
}

fn check_env_files(dir: &Path, fix: bool, results: &mut Vec<CheckResult>) {
    let env_file = dir.join(".env");
    if env_file.exists() {
        results.push(CheckResult::new(CheckStatus::Ok, ".env found"));
    } else if let Some(template) = [".env.example", ".env.template"]
        .into_iter()
        .find(|name| dir.join(name).exists())
    {
        if fix {
            match fs::copy(dir.join(template), &env_file) {
                Ok(_) => results.push(CheckResult::new(
                    Fixed,
                    format!(".env created from {template}"),
                )),
                Err(e) => results.push(
                    CheckResult::new(Error, format!("Failed to create .env: {e}"))
                        .hint(format!("Manually copy {template} to .env")),
                ),
            }
        } else {
            results.push(
                CheckResult::new(Warn, format!(".env missing (template found: {template})"))
                    .hint("Use --fix to create .env from template"),
            );
        }
    } else {
        results.push(CheckResult::new(
            Info,
            ".env not found (no template available)",
        ));
    }
    if dir.join(".env.local").exists() {
        results.push(CheckResult::new(Info, ".env.local found"));
    }
}

fn check_config_files(dir: &Path, results: &mut Vec<CheckResult>) {
    let found: Vec<_> = [
        "config.json",
        "config.toml",
        "config.yaml",
        "config.yml",
        ".config.json",
        ".config.toml",
        "settings.json",
        "settings.toml",
    ]
    .into_iter()
    .filter(|file| dir.join(file).exists())
    .collect();
    if !found.is_empty() {
        results.push(CheckResult::new(
            Info,
            format!("Found config files: {}", found.join(", ")),
        ));
    }
}

fn handle_set_env(dir: &Path, envs: &[(&str, &str)], results: &mut Vec<CheckResult>) {
    let env_file = dir.join(".env");
    let existing = match fs::read_to_string(&env_file) {
        Ok(content) => content,
        Err(e) if e.kind() == ErrorKind::NotFound => String::new(),
        Err(e) => {
            results.push(CheckResult::new(Error, format!("Failed to read .env: {e}")));
            return;
        }
    };
    let mut entries: HashMap<&str, usize> = existing
        .lines()
        .enumerate()
        .filter_map(|(index, line)| {
            let line = line.trim();
            if line.starts_with('#') {
                return None;
            }
            let (key, _) = line.split_once('=')?;
            Some((key.trim(), index))
        })
        .collect();
    let mut lines: Vec<_> = existing.lines().map(String::from).collect();
    let mut added = 0;
    let mut updated = 0;
    for &(key, value) in envs {
        let entry = format!("{key}={value}");
        if let Some(&index) = entries.get(key) {
            lines[index] = entry;
            updated += 1;
        } else {
            entries.insert(key, lines.len());
            lines.push(entry);
            added += 1;
        }
    }

    if let Err(e) = fs::write(&env_file, format!("{}\n", lines.join("\n"))) {
        results.push(CheckResult::new(
            Error,
            format!("Failed to write .env: {e}"),
        ));
        return;
    }
    if added > 0 {
        results.push(CheckResult::new(
            Fixed,
            format!("Added {added} environment variable(s) to .env"),
        ));
    }
    if updated > 0 {
        results.push(CheckResult::new(
            Fixed,
            format!("Updated {updated} environment variable(s) in .env"),
        ));
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;
    use tempfile::TempDir;

    #[test]
    fn test_detect_project_type_nodejs() {
        let dir = TempDir::new().unwrap();
        File::create(dir.path().join("package.json")).unwrap();

        assert_eq!(detect_project_type(dir.path()), ProjectType::NodeJs);
    }

    #[test]
    fn test_detect_project_type_cargo() {
        let dir = TempDir::new().unwrap();
        File::create(dir.path().join("Cargo.toml")).unwrap();

        assert_eq!(detect_project_type(dir.path()), ProjectType::Cargo);
    }

    #[test]
    fn test_detect_project_type_generic() {
        let dir = TempDir::new().unwrap();

        assert_eq!(detect_project_type(dir.path()), ProjectType::Generic);
    }

    #[test]
    fn test_check_env_files_with_template() {
        let dir = TempDir::new().unwrap();
        let mut template = File::create(dir.path().join(".env.example")).unwrap();
        writeln!(template, "DATABASE_URL=postgres://localhost/mydb").unwrap();

        let mut results = Vec::new();
        check_env_files(dir.path(), false, &mut results);

        // Should have a warning about missing .env
        assert!(results.iter().any(|r| r.status == CheckStatus::Warn));
        assert!(results.iter().any(|r| r.message.contains(".env missing")));
    }

    #[test]
    fn test_check_env_files_with_fix() {
        let dir = TempDir::new().unwrap();
        let mut template = File::create(dir.path().join(".env.example")).unwrap();
        writeln!(template, "DATABASE_URL=postgres://localhost/mydb").unwrap();

        let mut results = Vec::new();
        check_env_files(dir.path(), true, &mut results);

        // Should have created .env
        assert!(dir.path().join(".env").exists());
        assert!(results.iter().any(|r| r.status == CheckStatus::Fixed));
    }

    #[test]
    fn test_handle_set_env_new_file() {
        let dir = TempDir::new().unwrap();
        let mut results = Vec::new();

        handle_set_env(
            dir.path(),
            &[
                ("DATABASE_URL", "postgres://localhost/mydb"),
                ("PORT", "3000"),
            ],
            &mut results,
        );

        assert!(dir.path().join(".env").exists());
        let content = fs::read_to_string(dir.path().join(".env")).unwrap();
        assert!(content.contains("DATABASE_URL=postgres://localhost/mydb"));
        assert!(content.contains("PORT=3000"));
    }

    #[test]
    fn test_handle_set_env_update_existing() {
        let dir = TempDir::new().unwrap();
        let mut env_file = File::create(dir.path().join(".env")).unwrap();
        writeln!(env_file, "DATABASE_URL=old_value").unwrap();
        writeln!(env_file, "OTHER_VAR=keep_me").unwrap();
        drop(env_file);

        let mut results = Vec::new();
        handle_set_env(dir.path(), &[("DATABASE_URL", "new_value")], &mut results);

        let content = fs::read_to_string(dir.path().join(".env")).unwrap();
        assert!(content.contains("DATABASE_URL=new_value"));
        assert!(content.contains("OTHER_VAR=keep_me"));
        assert!(!content.contains("old_value"));
    }

    #[test]
    fn test_check_nodejs_project_missing_node_modules() {
        let dir = TempDir::new().unwrap();
        let mut package = File::create(dir.path().join("package.json")).unwrap();
        writeln!(package, r#"{{"name": "test"}}"#).unwrap();

        let mut results = Vec::new();
        check_nodejs_project(dir.path(), false, &mut results);

        assert!(results
            .iter()
            .any(|r| r.message.contains("package.json found")));
        assert!(results
            .iter()
            .any(|r| r.message.contains("node_modules/ missing")));
    }

    #[test]
    fn test_check_cargo_project() {
        let dir = TempDir::new().unwrap();
        let mut cargo_toml = File::create(dir.path().join("Cargo.toml")).unwrap();
        writeln!(cargo_toml, r#"[package]"#).unwrap();
        writeln!(cargo_toml, r#"name = "test""#).unwrap();

        let mut results = Vec::new();
        check_cargo_project(dir.path(), &mut results);

        assert!(results
            .iter()
            .any(|r| r.message.contains("Cargo.toml found")));
        assert!(results
            .iter()
            .any(|r| r.message.contains("Cargo.lock missing")));
    }

    #[test]
    fn test_check_config_files() {
        let dir = TempDir::new().unwrap();
        File::create(dir.path().join("config.json")).unwrap();
        File::create(dir.path().join("config.toml")).unwrap();

        let mut results = Vec::new();
        check_config_files(dir.path(), &mut results);

        assert!(results.iter().any(|r| r.message.contains("config.json")));
        assert!(results.iter().any(|r| r.message.contains("config.toml")));
    }

    #[test]
    fn test_check_status_prefix() {
        assert!(CheckStatus::Ok.prefix().contains("OK"));
        assert!(CheckStatus::Warn.prefix().contains("WARN"));
        assert!(CheckStatus::Error.prefix().contains("ERROR"));
        assert!(CheckStatus::Info.prefix().contains("INFO"));
        assert!(CheckStatus::Fixed.prefix().contains("FIX"));
    }
}
