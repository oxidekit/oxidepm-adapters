use std::collections::HashMap;
use std::path::Path;

use oxidepm_core::{validate::MAX_NAME_LEN, validate_app_name, AppSpec, Runtime};

pub struct Launch<'a> {
    pub name: Option<&'a str>,
    pub runtime: Option<Runtime>,
    pub script: Option<&'a str>,
    pub bin: Option<&'a str>,
    pub port: Option<u16>,
    pub env: HashMap<String, String>,
}

fn app_name(explicit: Option<&str>, repo: &str) -> Result<String, String> {
    if let Some(name) = explicit {
        return if validate_app_name(name) {
            Ok(name.to_string())
        } else {
            Err(format!("Invalid app name: {name}"))
        };
    }

    let sanitized: String = repo
        .chars()
        .take(MAX_NAME_LEN)
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let name = sanitized.trim_matches('-');
    if validate_app_name(name) {
        return Ok(name.to_string());
    }
    let prefixed = format!(
        "app-{}",
        name.chars()
            .take(MAX_NAME_LEN - "app-".len())
            .collect::<String>()
    );
    Ok(prefixed.trim_end_matches('-').to_string())
}

impl Launch<'_> {
    pub fn validate(&self, repo: &str) -> Result<(), String> {
        app_name(self.name, repo)?;
        if self.script.is_some() && self.bin.is_some() {
            return Err("Choose either --script or --bin".into());
        }
        Ok(())
    }

    pub fn spec(self, dir: &Path, repo: &str) -> Result<AppSpec, String> {
        let name = app_name(self.name, repo)?;
        let runtime = self
            .runtime
            .or_else(|| self.script.map(|_| Runtime::from_static("npm")))
            .or_else(|| Runtime::detect(dir))
            .unwrap_or(Runtime::CMD);
        let default = match runtime.as_str() {
            "npm" | "pnpm" | "yarn" => "start",
            "python" => "main.py",
            _ => ".",
        };
        let command = self.script.or(self.bin).unwrap_or(default);
        let mut spec = AppSpec::try_new(name, runtime, command.into(), dir.into())
            .map_err(|err| err.to_string())?;
        spec.env = self.env;
        spec.port = self.port;
        if let Some(port) = self.port {
            spec.env
                .entry("PORT".into())
                .or_insert_with(|| port.to_string());
        }
        Ok(spec)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_name_is_sanitized_and_explicit_names_are_validated() {
        for (repo, name) in [
            ("an.app", "an-app"),
            ("all", "app-all"),
            ("123", "app-123"),
            ("--", "app"),
        ] {
            assert_eq!(app_name(None, repo).unwrap(), name);
        }
        assert!(app_name(Some("all"), "project").is_err());
    }
}
