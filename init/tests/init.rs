use std::{fs, sync::OnceLock};

use oxidepm_core::ConfigFile;
use oxidepm_runtime_testkit::{Adapter, Env};

const CONFIG: &str = "oxidepm.config.toml";

fn setup() -> Env {
    static ADAPTER: OnceLock<Adapter> = OnceLock::new();
    Env::new(ADAPTER.get_or_init(|| {
        Adapter::new(
            env!("CARGO_MANIFEST_DIR"),
            env!("CARGO_BIN_EXE_oxidepm-init"),
        )
    }))
}

#[test]
fn init_writes_loadable_config_for_every_project_type() {
    let env = setup();
    for (name, mode, files) in [
        (
            "rust-svc",
            "cargo",
            &[("Cargo.toml", "[package]\nname = \"rust-svc\" # service\n")][..],
        ),
        (
            "node-svc",
            "npm",
            &[(
                "package.json",
                r#"{"name":"@acme/node-svc","scripts":{"start":"x"}}"#,
            )],
        ),
        (
            "py-svc",
            "python",
            &[("requirements.txt", ""), ("main.py", "")],
        ),
        (
            "py-django",
            "python",
            &[("requirements.txt", ""), ("manage.py", "")],
        ),
        ("go-svc", "go", &[("go.mod", "module example.com/go-svc\n")]),
        ("plain", "cmd", &[]),
    ] {
        let dir = env.project().join(name);
        fs::create_dir(&dir).unwrap();
        for (file, contents) in files {
            fs::write(dir.join(file), contents).unwrap();
        }
        let run = env.cli(&["init", name]);
        assert_eq!(run.code, 0, "{name}: {}", run.all());
        let specs = ConfigFile::load(&dir.join(CONFIG))
            .unwrap()
            .into_specs(&dir)
            .unwrap();
        assert_eq!(specs.len(), 1);
        assert_eq!(
            specs[0].name,
            if name == "node-svc" {
                "acme-node-svc"
            } else {
                name
            }
        );
        assert_eq!(specs[0].mode.as_str(), mode);
        if name == "py-django" {
            assert_eq!(specs[0].command, "manage.py");
            assert_eq!(specs[0].args, ["runserver"]);
        }
    }
}

#[test]
fn init_dry_run_prints_config_and_writes_nothing() {
    let env = setup();
    fs::write(
        env.project().join("package.json"),
        r#"{"name":"preview","scripts":{"dev":"vite"}}"#,
    )
    .unwrap();
    let run = env.cli(&["--dry-run", "init"]);
    assert_eq!(run.code, 0, "{}", run.all());
    let specs = ConfigFile::from_toml(&run.stdout)
        .unwrap()
        .into_specs(&env.project())
        .unwrap();
    assert_eq!(specs[0].name, "preview");
    assert_eq!(specs[0].command, "dev");
    assert!(!env.project().join(CONFIG).exists());
}

#[test]
fn init_refuses_to_overwrite_existing_config() {
    let env = setup();
    let config = env.project().join(CONFIG);
    fs::write(&config, "existing content\n").unwrap();
    let run = env.cli(&["init"]);
    assert_eq!(run.code, 1, "{}", run.all());
    assert!(run.all().contains("Config already exists"), "{}", run.all());
    assert_eq!(fs::read_to_string(&config).unwrap(), "existing content\n");
}
