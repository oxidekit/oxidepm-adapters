use std::{collections::BTreeMap, path::Path};

use oxidepm_core::ConfigFile;
use serde::Serialize;

use crate::detect;

#[derive(Serialize)]
struct GeneratedConfig {
    apps: Vec<GeneratedApp>,
}

#[derive(Serialize)]
pub struct GeneratedApp {
    pub name: String,
    pub mode: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub script: Option<&'static str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<&'static str>,
    cwd: &'static str,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<&'static str, &'static str>,
}

impl GeneratedApp {
    pub fn new(name: &str, mode: &'static str) -> Self {
        Self {
            name: name.to_string(),
            mode,
            script: None,
            args: Vec::new(),
            cwd: ".",
            env: BTreeMap::new(),
        }
    }
}

pub fn render(dir: &Path) -> Result<String, String> {
    let apps = detect::apps(dir)?;
    let rendered = toml::to_string_pretty(&GeneratedConfig { apps })
        .map_err(|e| format!("Cannot render config: {e}"))?;
    ConfigFile::from_toml(&rendered)
        .and_then(|config| config.into_specs(dir))
        .map_err(|e| format!("Generated config is invalid: {e}"))?;
    Ok(rendered)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn minified_package_json_and_scoped_name_produce_valid_toml() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("package.json"),
            r#"{"name":"@acme/api","scripts":{"start":"node \"index.js\"","dev":"vite"}}"#,
        )
        .unwrap();
        let generated = render(dir.path()).unwrap();
        let spec = &ConfigFile::from_toml(&generated)
            .unwrap()
            .into_specs(dir.path())
            .unwrap()[0];
        assert_eq!(spec.name, "acme-api");
        assert_eq!(spec.command, "start");
        assert!(!generated.contains("watch"));
        assert!(!generated.contains("NODE_ENV"));
    }

    #[test]
    fn cargo_toml_with_inline_comment_and_quotes_is_parsed_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("Cargo.toml"),
            "[workspace]\nmembers = []\n[package]\nname = \"my-svc\" # service\n",
        )
        .unwrap();
        let generated = render(dir.path()).unwrap();
        let spec = &ConfigFile::from_toml(&generated)
            .unwrap()
            .into_specs(dir.path())
            .unwrap()[0];
        assert_eq!(spec.name, "my-svc");
        assert_eq!(spec.mode.as_str(), "cargo");
    }

    #[test]
    fn django_project_puts_runserver_in_args() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("requirements.txt"), "Django").unwrap();
        fs::write(dir.path().join("manage.py"), "").unwrap();
        let generated = render(dir.path()).unwrap();
        let spec = &ConfigFile::from_toml(&generated)
            .unwrap()
            .into_specs(dir.path())
            .unwrap()[0];
        assert_eq!(spec.command, "manage.py");
        assert_eq!(spec.args, ["runserver"]);
    }

    #[test]
    fn cargo_and_node_in_same_dir_deduplicates_names() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("Cargo.toml"), "[package]\nname = 'app'").unwrap();
        fs::write(dir.path().join("package.json"), r#"{"name":"app"}"#).unwrap();
        let generated = render(dir.path()).unwrap();
        let specs = ConfigFile::from_toml(&generated)
            .unwrap()
            .into_specs(dir.path())
            .unwrap();
        assert_eq!(
            specs.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["app", "app-2"]
        );
    }
}
