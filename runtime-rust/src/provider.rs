//! How a Rust app is built and which binary is run.
//!
//! `rust` is one source file, compiled with `rustc` into `.oxidepm/bin` of the app's directory.
//! `cargo` is a Cargo project, built with `cargo build --release`; `cargo metadata` (a step whose
//! output the daemon hands back) says which binaries the project has and where its target
//! directory is, so workspaces, a configured target directory and several binaries work.

use std::path::Path;

use oxidepm_adapter_proto::messages::{
    Capture, PlanParams, PlanResult, PrepareParams, PrepareResult, SpecView, Step,
};
use oxidepm_adapter_sdk::{Refusal, RuntimeProvider};
use serde_json::Value;

pub struct Rust;

/// The id of the step whose output is the metadata of the project.
const METADATA: &str = "metadata";

/// How long one build may take; the daemon holds the step to its own cap as well.
const BUILD_SECS: u64 = 1800;

fn check_runtime(spec: &SpecView) -> Result<(), Refusal> {
    match spec.runtime.as_str() {
        "rust" | "cargo" => Ok(()),
        other => Err(Refusal::new(format!(
            "runtime-rust does not provide the runtime '{other}'"
        ))),
    }
}

/// The name of the binary the person asked for, or `None` for "the project's own".
fn named_binary(spec: &SpecView) -> Result<Option<&str>, Refusal> {
    let name = spec.command.trim();
    if matches!(name, "" | "." | "./") {
        return Ok(None);
    }
    if name.contains(['/', '\\']) || name.contains(char::is_whitespace) {
        return Err(Refusal::new(format!("{name:?} is not the name of a binary (name one of the project's binaries, or . for the project's own)")));
    }
    Ok(Some(name))
}

/// The source file of a `rust` app, as an absolute path, and the name the compiler gives the
/// binary (the file name without its extension).
fn source_of(spec: &SpecView) -> Result<(String, String), Refusal> {
    let command = spec.command.trim();
    if command.is_empty() {
        return Err(Refusal::new("no source file given"));
    }
    let path = Path::new(command);
    if path.extension().and_then(|e| e.to_str()) != Some("rs") {
        return Err(Refusal::new(format!(
            "{command:?} is not a Rust source file (expected a .rs file)"
        )));
    }
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        Path::new(&spec.cwd).join(path)
    };
    let stem = absolute
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| Refusal::new(format!("{command:?} has no file name")))?;
    Ok((absolute.to_string_lossy().to_string(), stem.to_string()))
}

fn rust_binary(spec: &SpecView, stem: &str) -> String {
    Path::new(&spec.cwd)
        .join(".oxidepm")
        .join("bin")
        .join(stem)
        .to_string_lossy()
        .to_string()
}

impl RuntimeProvider for Rust {
    fn prepare(&self, params: &PrepareParams) -> Result<PrepareResult, Refusal> {
        let spec = &params.spec;
        check_runtime(spec)?;
        let steps = if spec.runtime == "rust" {
            let (source, stem) = source_of(spec)?;
            let out_dir = Path::new(&spec.cwd)
                .join(".oxidepm")
                .join("bin")
                .to_string_lossy()
                .to_string();
            vec![Step {
                id: "compile".to_string(),
                argv: vec![
                    "rustc".to_string(),
                    source,
                    "--out-dir".to_string(),
                    out_dir,
                    "-O".to_string(),
                ],
                cwd: None,
                env_add: Default::default(),
                timeout_secs: BUILD_SECS,
                capture: Capture::None,
                capture_max: None,
                optional: false,
                description: format!("Compile {stem}.rs"),
            }]
        } else {
            // The step that asks cargo for the target directory runs without the app's variables
            // and the build with them, so a CARGO_TARGET_DIR in the app's own environment would
            // send the binary somewhere the plan does not look.
            if spec.env.contains_key("CARGO_TARGET_DIR") {
                return Err(Refusal::new("CARGO_TARGET_DIR is set in the app's environment; set the target directory in .cargo/config.toml ([build] target-dir) instead, or in the daemon's environment"));
            }
            let mut build = vec![
                "cargo".to_string(),
                "build".to_string(),
                "--release".to_string(),
            ];
            if let Some(name) = named_binary(spec)? {
                build.extend(["--bin".to_string(), name.to_string()]);
            }
            vec![
                Step {
                    id: METADATA.to_string(),
                    argv: ["cargo", "metadata", "--format-version", "1", "--no-deps"]
                        .map(String::from)
                        .to_vec(),
                    cwd: None,
                    env_add: Default::default(),
                    timeout_secs: 120,
                    capture: Capture::Stdout,
                    capture_max: None,
                    optional: false,
                    description: "Ask cargo for the binaries and the target directory".to_string(),
                },
                Step {
                    id: "build".to_string(),
                    argv: build,
                    cwd: None,
                    env_add: Default::default(),
                    timeout_secs: BUILD_SECS,
                    capture: Capture::None,
                    capture_max: None,
                    optional: false,
                    description: "cargo build --release".to_string(),
                },
            ]
        };
        Ok(PrepareResult {
            steps,
            notes: Vec::new(),
        })
    }

    fn plan(&self, params: &PlanParams) -> Result<PlanResult, Refusal> {
        let spec = &params.spec;
        check_runtime(spec)?;
        let binary = if spec.runtime == "rust" {
            let (_, stem) = source_of(spec)?;
            rust_binary(spec, &stem)
        } else {
            let metadata = params
                .steps_done
                .iter()
                .find(|s| s.id == METADATA)
                .and_then(|s| s.stdout.as_deref())
                .ok_or_else(|| Refusal::new("cargo metadata gave no answer"))?;
            cargo_binary(spec, metadata)?
        };
        Ok(PlanResult {
            program: binary.clone(),
            args: spec.args.clone(),
            cwd: spec.cwd.clone(),
            extra_env: Default::default(),
            binary_path: Some(binary),
        })
    }
}

/// One binary target of the project.
#[derive(Debug, PartialEq)]
struct Bin {
    package: String,
    name: String,
    /// Is the package the one in the app's directory?
    root: bool,
}

/// The path of the release binary to run, from the output of `cargo metadata --no-deps`.
fn cargo_binary(spec: &SpecView, metadata: &str) -> Result<String, Refusal> {
    let doc: Value = serde_json::from_str(metadata)
        .map_err(|e| Refusal::new(format!("cannot read the output of cargo metadata: {e}")))?;
    let target_dir = doc["target_directory"]
        .as_str()
        .filter(|d| Path::new(d).is_absolute())
        .ok_or_else(|| Refusal::new("cargo metadata did not say where the target directory is"))?;
    let own_manifest = Path::new(&spec.cwd).join("Cargo.toml");
    let mut bins = Vec::new();
    for package in doc["packages"].as_array().into_iter().flatten() {
        let root = package["manifest_path"]
            .as_str()
            .is_some_and(|m| Path::new(m) == own_manifest);
        for target in package["targets"].as_array().into_iter().flatten() {
            if target["kind"]
                .as_array()
                .is_some_and(|k| k.iter().any(|k| k == "bin"))
            {
                if let Some(name) = target["name"].as_str() {
                    bins.push(Bin {
                        package: package["name"].as_str().unwrap_or_default().to_string(),
                        name: name.to_string(),
                        root,
                    });
                }
            }
        }
    }
    let chosen = choose(&bins, named_binary(spec)?)?;
    Ok(Path::new(target_dir)
        .join("release")
        .join(&chosen.name)
        .to_string_lossy()
        .to_string())
}

fn list(bins: &[Bin]) -> String {
    let mut names: Vec<&str> = bins.iter().map(|b| b.name.as_str()).collect();
    names.sort_unstable();
    names.dedup();
    names.join(", ")
}

/// Which binary: the one that is named; else the only one of the package in the app's directory,
/// or the one with the package's name among several; else (a workspace with no package of its
/// own) the only one there is.
fn choose<'a>(bins: &'a [Bin], wanted: Option<&str>) -> Result<&'a Bin, Refusal> {
    if bins.is_empty() {
        return Err(Refusal::new("the project has no binary to run"));
    }
    if let Some(name) = wanted {
        let found: Vec<&Bin> = bins.iter().filter(|b| b.name == name).collect();
        return match found.as_slice() {
            [one] => Ok(one),
            [] => Err(Refusal::new(format!(
                "the project has no binary called {name}; it has: {}",
                list(bins)
            ))),
            _ => Err(Refusal::new(format!(
                "{name} is the name of binaries in several packages ({})",
                found
                    .iter()
                    .map(|b| b.package.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))),
        };
    }
    let own: Vec<&Bin> = bins.iter().filter(|b| b.root).collect();
    let pool: Vec<&Bin> = if own.is_empty() {
        bins.iter().collect()
    } else {
        own
    };
    match pool.as_slice() {
        [one] => Ok(one),
        many => many
            .iter()
            .copied()
            .find(|b| b.root && b.name == b.package)
            .ok_or_else(|| {
                Refusal::new(format!(
                    "the project has several binaries ({}); name the one to run",
                    list(bins)
                ))
            }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxidepm_adapter_proto::messages::{Phase, StepDone};
    use serde_json::json;

    fn spec(runtime: &str, command: &str, args: &[&str]) -> SpecView {
        SpecView {
            name: "app".into(),
            runtime: runtime.into(),
            command: command.into(),
            args: args.iter().map(|a| a.to_string()).collect(),
            cwd: "/srv/app".into(),
            tags: vec![],
            ports: vec![],
            instances: 1,
            env: Default::default(),
            options: Default::default(),
        }
    }

    fn prepare(spec: SpecView) -> Result<Vec<Step>, Refusal> {
        Rust.prepare(&PrepareParams {
            spec,
            path_hint: "/usr/bin".into(),
            phase: Phase::Start,
        })
        .map(|p| p.steps)
    }

    fn plan(spec: SpecView, steps_done: Vec<StepDone>) -> Result<PlanResult, Refusal> {
        Rust.plan(&PlanParams { spec, steps_done })
    }

    /// `cargo metadata --no-deps` for packages `(name, manifest dir, [bin names], has lib)`.
    fn metadata(target_dir: &str, packages: &[(&str, &str, &[&str], bool)]) -> StepDone {
        let packages: Vec<Value> = packages
            .iter()
            .map(|(name, dir, bins, lib)| {
                let mut targets: Vec<Value> = bins.iter().map(|b| json!({"kind": ["bin"], "crate_types": ["bin"], "name": b})).collect();
                if *lib {
                    targets.push(json!({"kind": ["lib"], "crate_types": ["lib"], "name": name.replace('-', "_")}));
                }
                json!({"name": name, "manifest_path": format!("{dir}/Cargo.toml"), "targets": targets})
            })
            .collect();
        let doc = json!({"packages": packages, "target_directory": target_dir, "workspace_root": "/srv/app"});
        StepDone {
            id: METADATA.into(),
            exit_code: 0,
            stdout: Some(doc.to_string()),
            stderr: None,
        }
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_source_file_is_compiled_into_the_apps_own_directory_and_run_from_there() {
        let steps = prepare(spec("rust", "tool.rs", &[])).unwrap();
        let [compile] = steps.as_slice() else {
            panic!("one step: {steps:?}")
        };
        assert_eq!(
            compile.argv,
            strings(&[
                "rustc",
                "/srv/app/tool.rs",
                "--out-dir",
                "/srv/app/.oxidepm/bin",
                "-O"
            ])
        );
        assert_eq!(compile.capture, Capture::None);
        assert!(!compile.optional);
        let planned = plan(spec("rust", "tool.rs", &["--port", "3000"]), vec![]).unwrap();
        assert_eq!(planned.program, "/srv/app/.oxidepm/bin/tool");
        assert_eq!(
            planned.binary_path.as_deref(),
            Some("/srv/app/.oxidepm/bin/tool")
        );
        assert_eq!(planned.args, strings(&["--port", "3000"]));
        assert_eq!(planned.cwd, "/srv/app");
        // an absolute source keeps its name and still builds in the app's directory
        let steps = prepare(spec("rust", "/opt/src/other-tool.rs", &[])).unwrap();
        assert_eq!(steps[0].argv[1], "/opt/src/other-tool.rs");
        assert_eq!(
            plan(spec("rust", "/opt/src/other-tool.rs", &[]), vec![])
                .unwrap()
                .program,
            "/srv/app/.oxidepm/bin/other-tool"
        );
    }

    #[test]
    fn a_file_that_is_not_rust_or_no_file_is_refused_with_a_reason() {
        assert!(prepare(spec("rust", "app.txt", &[]))
            .unwrap_err()
            .0
            .contains("expected a .rs file"));
        assert!(prepare(spec("rust", "", &[]))
            .unwrap_err()
            .0
            .contains("no source file"));
        assert!(prepare(spec("go", "x.go", &[]))
            .unwrap_err()
            .0
            .contains("'go'"));
    }

    #[test]
    fn a_project_is_asked_for_its_binaries_and_then_built_in_release_mode() {
        let steps = prepare(spec("cargo", ".", &[])).unwrap();
        let [meta, build] = steps.as_slice() else {
            panic!("two steps: {steps:?}")
        };
        assert_eq!(meta.id, "metadata");
        assert_eq!(
            meta.argv,
            strings(&["cargo", "metadata", "--format-version", "1", "--no-deps"])
        );
        assert_eq!(meta.capture, Capture::Stdout);
        assert_eq!(build.argv, strings(&["cargo", "build", "--release"]));
        assert_eq!(build.capture, Capture::None);
        // a named binary is built by name
        let steps = prepare(spec("cargo", "server", &[])).unwrap();
        assert_eq!(
            steps[1].argv,
            strings(&["cargo", "build", "--release", "--bin", "server"])
        );
        for odd in ["a b", "../x", "dir/x"] {
            assert!(
                prepare(spec("cargo", odd, &[]))
                    .unwrap_err()
                    .0
                    .contains("not the name of a binary"),
                "{odd}"
            );
        }
    }

    #[test]
    fn a_target_directory_set_in_the_apps_environment_is_refused_because_the_plan_could_not_find_it(
    ) {
        let mut s = spec("cargo", ".", &[]);
        s.env.insert("CARGO_TARGET_DIR".into(), "<redacted>".into());
        assert!(prepare(s).unwrap_err().0.contains(".cargo/config.toml"));
        // the single-file runtime has no such problem
        let mut s = spec("rust", "t.rs", &[]);
        s.env.insert("CARGO_TARGET_DIR".into(), "<redacted>".into());
        assert!(prepare(s).is_ok());
    }

    #[test]
    fn the_plan_runs_the_binary_in_the_target_directory_cargo_reported() {
        // a custom target directory, away from the project
        let meta = metadata("/var/build/out", &[("app", "/srv/app", &["app"], true)]);
        let planned = plan(spec("cargo", ".", &["-v"]), vec![meta]).unwrap();
        assert_eq!(planned.program, "/var/build/out/release/app");
        assert_eq!(
            planned.binary_path.as_deref(),
            Some("/var/build/out/release/app")
        );
        assert_eq!(planned.args, strings(&["-v"]));
    }

    #[test]
    fn a_workspace_member_is_found_by_the_name_of_its_binary_not_by_a_guess_at_the_package() {
        // the package is called `app-server`, its binary `server`, its library `x`
        let meta = || {
            metadata(
                "/srv/target",
                &[
                    ("app-server", "/srv/app/app-server", &["server"], true),
                    ("tools", "/srv/app/tools", &["migrate", "seed"], false),
                ],
            )
        };
        assert_eq!(
            plan(spec("cargo", "server", &[]), vec![meta()])
                .unwrap()
                .program,
            "/srv/target/release/server"
        );
        assert_eq!(
            plan(spec("cargo", "seed", &[]), vec![meta()])
                .unwrap()
                .program,
            "/srv/target/release/seed"
        );
        let err = plan(spec("cargo", "nothing", &[]), vec![meta()]).unwrap_err();
        assert!(
            err.0.contains("no binary called nothing") && err.0.contains("migrate, seed, server"),
            "{err}"
        );
        // no name in a workspace with three binaries: the person has to say which
        let err = plan(spec("cargo", ".", &[]), vec![meta()]).unwrap_err();
        assert!(
            err.0.contains("several binaries") && err.0.contains("migrate, seed, server"),
            "{err}"
        );
    }

    #[test]
    fn without_a_name_the_project_in_the_apps_directory_decides() {
        // one binary of the package here: that one
        let one = metadata(
            "/t",
            &[
                ("app", "/srv/app", &["run-it"], false),
                ("other", "/srv/app/other", &["other"], false),
            ],
        );
        assert_eq!(
            plan(spec("cargo", "", &[]), vec![one]).unwrap().program,
            "/t/release/run-it"
        );
        // several here: the one with the package's name
        let several = metadata("/t", &[("app", "/srv/app", &["helper", "app"], false)]);
        assert_eq!(
            plan(spec("cargo", ".", &[]), vec![several])
                .unwrap()
                .program,
            "/t/release/app"
        );
        // several here and none with the package's name
        let none = metadata("/t", &[("app", "/srv/app", &["a", "b"], false)]);
        assert!(plan(spec("cargo", ".", &[]), vec![none])
            .unwrap_err()
            .0
            .contains("name the one to run"));
        // a workspace with no package of its own and a single binary
        let virtual_root = metadata(
            "/t",
            &[
                ("a", "/srv/app/a", &["only"], true),
                ("b", "/srv/app/b", &[], true),
            ],
        );
        assert_eq!(
            plan(spec("cargo", ".", &[]), vec![virtual_root])
                .unwrap()
                .program,
            "/t/release/only"
        );
        // a project with only a library
        let lib = metadata("/t", &[("app", "/srv/app", &[], true)]);
        assert!(plan(spec("cargo", ".", &[]), vec![lib])
            .unwrap_err()
            .0
            .contains("no binary"));
    }

    #[test]
    fn a_virtual_workspace_needs_a_name_even_when_binaries_match_their_packages() {
        let meta = metadata(
            "/t",
            &[
                ("one", "/srv/app/one", &["one"], false),
                ("two", "/srv/app/two", &["two"], false),
            ],
        );
        let err = plan(spec("cargo", ".", &[]), vec![meta]).unwrap_err();
        assert!(err.0.contains("several binaries (one, two)"), "{err}");
    }

    #[test]
    fn the_same_binary_name_in_two_packages_is_ambiguous() {
        let meta = metadata(
            "/t",
            &[
                ("one", "/srv/app/one", &["srv"], false),
                ("two", "/srv/app/two", &["srv"], false),
            ],
        );
        let err = plan(spec("cargo", "srv", &[]), vec![meta]).unwrap_err();
        assert!(
            err.0.contains("several packages") && err.0.contains("one, two"),
            "{err}"
        );
    }

    #[test]
    fn missing_or_unreadable_metadata_is_refused_with_a_reason() {
        assert!(plan(spec("cargo", ".", &[]), vec![])
            .unwrap_err()
            .0
            .contains("gave no answer"));
        let garbage = StepDone {
            id: METADATA.into(),
            exit_code: 0,
            stdout: Some("not json".into()),
            stderr: None,
        };
        assert!(plan(spec("cargo", ".", &[]), vec![garbage])
            .unwrap_err()
            .0
            .contains("cannot read the output"));
        let no_dir = StepDone {
            id: METADATA.into(),
            exit_code: 0,
            stdout: Some(json!({"packages": []}).to_string()),
            stderr: None,
        };
        assert!(plan(spec("cargo", ".", &[]), vec![no_dir])
            .unwrap_err()
            .0
            .contains("target directory"));
        let relative = StepDone {
            id: METADATA.into(),
            exit_code: 0,
            stdout: Some(json!({"packages": [], "target_directory": "target"}).to_string()),
            stderr: None,
        };
        assert!(plan(spec("cargo", ".", &[]), vec![relative])
            .unwrap_err()
            .0
            .contains("target directory"));
    }
}
