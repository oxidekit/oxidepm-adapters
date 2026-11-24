use std::{collections::HashSet, fs, path::Path};

use oxidepm_core::{validate::MAX_NAME_LEN, validate_app_name};

use crate::render::GeneratedApp;

pub fn apps(dir: &Path) -> Result<Vec<GeneratedApp>, String> {
    let fallback = dir
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("app");
    let mut apps = Vec::new();
    if dir.join("Cargo.toml").exists() {
        apps.push(cargo(dir, fallback)?);
    }
    if dir.join("package.json").exists() {
        apps.push(node(dir, fallback)?);
    }
    if [
        "requirements.txt",
        "pyproject.toml",
        "setup.py",
        "manage.py",
    ]
    .iter()
    .any(|file| dir.join(file).is_file())
    {
        apps.push(python(dir, fallback));
    }
    if dir.join("go.mod").exists() {
        apps.push(go(dir)?);
    }
    if apps.is_empty() {
        apps.push(GeneratedApp::new(fallback, "cmd"));
    }

    let mut taken = HashSet::new();
    for app in &mut apps {
        app.name = unique_name(&app.name, &mut taken);
        if !validate_app_name(&app.name) {
            return Err(format!("Invalid app name: {}", app.name));
        }
    }
    Ok(apps)
}

fn read(dir: &Path, file: &str) -> Result<String, String> {
    let path = dir.join(file);
    fs::read_to_string(&path).map_err(|e| format!("Cannot read {}: {e}", path.display()))
}

fn cargo(dir: &Path, fallback: &str) -> Result<GeneratedApp, String> {
    let manifest = read(dir, "Cargo.toml")?;
    let package: toml::Value =
        toml::from_str(&manifest).map_err(|e| format!("Invalid Cargo.toml: {e}"))?;
    let name = package
        .get("package")
        .and_then(|package| package.get("name"))
        .and_then(toml::Value::as_str)
        .unwrap_or(fallback);
    let mut app = GeneratedApp::new(name, "cargo");
    app.env.insert("RUST_LOG", "info");
    Ok(app)
}

fn node(dir: &Path, fallback: &str) -> Result<GeneratedApp, String> {
    let package: serde_json::Value = serde_json::from_str(&read(dir, "package.json")?)
        .map_err(|e| format!("Invalid package.json: {e}"))?;
    if !package.is_object() {
        return Err("Invalid package.json: expected a JSON object".into());
    }
    let name = package["name"].as_str().unwrap_or(fallback);
    let mode = if dir.join("pnpm-lock.yaml").is_file() {
        "pnpm"
    } else if dir.join("yarn.lock").is_file() {
        "yarn"
    } else {
        "npm"
    };
    let mut app = GeneratedApp::new(name, mode);
    app.script = ["start", "dev"]
        .into_iter()
        .find(|script| package["scripts"][*script].is_string())
        .or(Some("start"));
    Ok(app)
}

fn python(dir: &Path, name: &str) -> GeneratedApp {
    let mut app = GeneratedApp::new(name, "python");
    if dir.join("manage.py").is_file() {
        app.script = Some("manage.py");
        app.args.push("runserver");
    } else {
        app.script = Some(if dir.join("main.py").is_file() {
            "main.py"
        } else {
            "app.py"
        });
    }
    app
}

fn go(dir: &Path) -> Result<GeneratedApp, String> {
    let module = read(dir, "go.mod")?;
    let name = module
        .lines()
        .find_map(|line| {
            let mut words = line.split_whitespace();
            if words.next()? != "module" {
                return None;
            }
            words.next()?.trim_matches('"').rsplit('/').next()
        })
        .unwrap_or("app");
    let mut app = GeneratedApp::new(name, "go");
    app.script = Some(".");
    Ok(app)
}

fn unique_name(raw: &str, taken: &mut HashSet<String>) -> String {
    let sanitized: String = raw
        .trim_start_matches('@')
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let mut base = sanitized.trim_matches('-');
    base = base[..base.len().min(MAX_NAME_LEN)].trim_end_matches('-');
    if !validate_app_name(base) {
        base = "app";
    }
    let mut name = base.to_string();
    let mut index = 2;
    while !taken.insert(name.clone()) {
        let suffix = format!("-{index}");
        let prefix = &base[..base.len().min(MAX_NAME_LEN - suffix.len())];
        name = format!("{prefix}{suffix}");
        index += 1;
    }
    name
}
