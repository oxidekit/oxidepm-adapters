//! Which interpreter to run, and with what.
//!
//! The adapter cannot look at the app's directory, so it has the daemon run the system's Python
//! once, in that directory, to say where the project's virtualenv interpreter is. The interpreter
//! it names for that step is the one it falls back to in the plan, so the two agree.

use std::path::Path;

use oxidepm_adapter_proto::messages::{
    Capture, PlanParams, PlanResult, PrepareParams, PrepareResult, SpecView, Step,
};
use oxidepm_adapter_sdk::{Refusal, RuntimeProvider};

pub struct Python;

/// The ids of the step that looks for the virtualenv; the rest of the id is the system
/// interpreter that runs it.
const PROBE_PREFIX: &str = "interpreter-";

/// Print the interpreter of the project's virtualenv, the first of `venv` and `.venv` that has one.
const PROBE: &str = "import os\nfor d in ('venv', '.venv'):\n    p = os.path.join(os.getcwd(), d, 'bin', 'python')\n    if os.path.exists(p):\n        print(p)\n        break\n";

fn check(spec: &SpecView) -> Result<Vec<String>, Refusal> {
    if spec.runtime != "python" {
        return Err(Refusal::new(format!(
            "runtime-python does not provide the runtime '{}'",
            spec.runtime
        )));
    }
    let words = shell_words::split(&spec.command)
        .map_err(|e| Refusal::new(format!("cannot read the command {:?}: {e}", spec.command)))?;
    if words.is_empty() {
        return Err(Refusal::new("no script or module given"));
    }
    Ok(words)
}

/// The system interpreter to use: `python3` when it is on the path, else `python` when that is,
/// else `python3` (the daemon then says it is not found).
fn system_interpreter(path_hint: &str) -> &'static str {
    let found = |name: &str| {
        path_hint
            .split(':')
            .filter(|d| !d.is_empty())
            .any(|d| Path::new(d).join(name).is_file())
    };
    if found("python3") || !found("python") {
        "python3"
    } else {
        "python"
    }
}

impl RuntimeProvider for Python {
    fn prepare(&self, params: &PrepareParams) -> Result<PrepareResult, Refusal> {
        check(&params.spec)?;
        let interpreter = system_interpreter(&params.path_hint);
        Ok(PrepareResult {
            steps: vec![Step {
                id: format!("{PROBE_PREFIX}{interpreter}"),
                argv: vec![interpreter.to_string(), "-c".to_string(), PROBE.to_string()],
                cwd: None,
                env_add: Default::default(),
                timeout_secs: 20,
                capture: Capture::Stdout,
                capture_max: Some(4096),
                optional: true,
                description: "Look for the project's virtualenv".to_string(),
            }],
            notes: Vec::new(),
        })
    }

    fn plan(&self, params: &PlanParams) -> Result<PlanResult, Refusal> {
        let spec = &params.spec;
        let mut args = check(spec)?;
        let probe = params
            .steps_done
            .iter()
            .find(|s| s.id.starts_with(PROBE_PREFIX));
        let system = probe.map_or("python3", |s| {
            if s.id.ends_with("python") {
                "python"
            } else {
                "python3"
            }
        });
        // the virtualenv's interpreter if the project has one: an absolute path to a file called python
        let venv = probe
            .filter(|s| s.exit_code == 0)
            .and_then(|s| s.stdout.as_deref())
            .and_then(|out| out.lines().next())
            .map(str::trim)
            .filter(|line| {
                Path::new(line).is_absolute()
                    && Path::new(line).file_name().is_some_and(|n| n == "python")
            });
        args.extend(spec.args.iter().cloned());
        Ok(PlanResult {
            program: venv.unwrap_or(system).to_string(),
            args,
            cwd: spec.cwd.clone(),
            // unbuffered output, so the log sees lines as they are printed
            extra_env: [("PYTHONUNBUFFERED".to_string(), "1".to_string())].into(),
            binary_path: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxidepm_adapter_proto::messages::{Phase, StepDone};

    fn spec(command: &str, args: &[&str]) -> SpecView {
        SpecView {
            name: "app".into(),
            runtime: "python".into(),
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

    fn done(id: &str, code: i32, stdout: Option<&str>) -> StepDone {
        StepDone {
            id: id.into(),
            exit_code: code,
            stdout: stdout.map(str::to_string),
            stderr: None,
        }
    }

    fn plan(
        command: &str,
        args: &[&str],
        steps_done: Vec<StepDone>,
    ) -> Result<PlanResult, Refusal> {
        Python.plan(&PlanParams {
            spec: spec(command, args),
            steps_done,
        })
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn dirs_with(files: &[&str]) -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        for f in files {
            std::fs::write(dir.path().join(f), "").unwrap();
        }
        let hint = format!("/nonexistent-dir:{}", dir.path().display());
        (dir, hint)
    }

    #[test]
    fn a_script_runs_with_the_system_interpreter_and_unbuffered_output() {
        let plan = plan(
            "app.py",
            &["--port", "3000"],
            vec![done("interpreter-python3", 0, Some(""))],
        )
        .unwrap();
        assert_eq!(plan.program, "python3");
        assert_eq!(plan.args, strings(&["app.py", "--port", "3000"]));
        assert_eq!(plan.cwd, "/srv/app");
        assert_eq!(
            plan.extra_env.get("PYTHONUNBUFFERED").map(String::as_str),
            Some("1")
        );
        assert_eq!(plan.extra_env.len(), 1);
    }

    #[test]
    fn a_command_of_several_words_is_split_like_a_shell_would() {
        // a module, a quoted argument and an argument with a space
        let planned = plan(
            "-m uvicorn app:app --host '0.0.0.0'",
            &["--port", "a b"],
            vec![],
        )
        .unwrap();
        assert_eq!(
            planned.args,
            strings(&["-m", "uvicorn", "app:app", "--host", "0.0.0.0", "--port", "a b"])
        );
        // a script with a space in its name has to be quoted
        let planned = plan("'my script.py'", &[], vec![]).unwrap();
        assert_eq!(planned.args, strings(&["my script.py"]));
        assert!(plan("'unclosed", &[], vec![])
            .unwrap_err()
            .0
            .contains("cannot read the command"));
        assert!(plan("   ", &[], vec![])
            .unwrap_err()
            .0
            .contains("no script"));
    }

    #[test]
    fn the_virtualenv_wins_when_the_probe_found_one() {
        let found = done(
            "interpreter-python3",
            0,
            Some("/srv/app/.venv/bin/python\n"),
        );
        assert_eq!(
            plan("app.py", &[], vec![found]).unwrap().program,
            "/srv/app/.venv/bin/python"
        );
        // nothing printed, a failed probe, or an answer that is not an interpreter: the system one
        for step in [
            done("interpreter-python3", 0, Some("")),
            done("interpreter-python3", 1, None),
            done("interpreter-python3", 0, Some("/bin/sh\n")),
            done("interpreter-python3", 0, Some("venv/bin/python\n")),
            done("interpreter-python3", 1, Some("/srv/app/venv/bin/python\n")),
        ] {
            assert_eq!(
                plan("app.py", &[], vec![step.clone()]).unwrap().program,
                "python3",
                "{step:?}"
            );
        }
    }

    #[test]
    fn prepare_and_plan_agree_on_the_system_interpreter() {
        // `python` only: the probe runs under it and the plan falls back to it
        let (_only_python, hint) = dirs_with(&["python"]);
        let prepared = Python
            .prepare(&PrepareParams {
                spec: spec("app.py", &[]),
                path_hint: hint,
                phase: Phase::Start,
            })
            .unwrap();
        let [probe] = prepared.steps.as_slice() else {
            panic!("one step: {prepared:?}")
        };
        assert_eq!(probe.id, "interpreter-python");
        assert_eq!(probe.argv[0], "python");
        assert!(probe.optional && probe.capture == Capture::Stdout);
        assert_eq!(
            plan("app.py", &[], vec![done(&probe.id, 0, Some(""))])
                .unwrap()
                .program,
            "python"
        );

        // both: python3 first; neither: python3, which the daemon then reports as missing
        for files in [&["python3", "python"][..], &[][..]] {
            let (_dir, hint) = dirs_with(files);
            let prepared = Python
                .prepare(&PrepareParams {
                    spec: spec("app.py", &[]),
                    path_hint: hint,
                    phase: Phase::Start,
                })
                .unwrap();
            assert_eq!(prepared.steps[0].id, "interpreter-python3", "{files:?}");
            assert_eq!(prepared.steps[0].argv[0], "python3");
        }
    }

    #[test]
    fn the_probe_looks_in_the_app_directory_and_prints_the_interpreter_it_finds() {
        // the snippet is Python; run it if this machine has python3
        let Ok(python) = std::process::Command::new("python3")
            .arg("--version")
            .output()
        else {
            return;
        };
        if !python.status.success() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let run = || {
            let out = std::process::Command::new("python3")
                .args(["-c", PROBE])
                .current_dir(dir.path())
                .output()
                .unwrap();
            assert!(out.status.success());
            String::from_utf8(out.stdout).unwrap()
        };
        assert_eq!(run(), "");
        std::fs::create_dir_all(dir.path().join(".venv/bin")).unwrap();
        std::fs::write(dir.path().join(".venv/bin/python"), "").unwrap();
        let cwd = std::fs::canonicalize(dir.path()).unwrap();
        assert_eq!(run().trim(), cwd.join(".venv/bin/python").to_string_lossy());
        // venv is looked at first
        std::fs::create_dir_all(dir.path().join("venv/bin")).unwrap();
        std::fs::write(dir.path().join("venv/bin/python"), "").unwrap();
        assert_eq!(run().trim(), cwd.join("venv/bin/python").to_string_lossy());
    }

    #[test]
    fn another_runtime_is_refused_with_a_reason() {
        let mut other = spec("app.py", &[]);
        other.runtime = "go".into();
        let err = Python
            .prepare(&PrepareParams {
                spec: other,
                path_hint: String::new(),
                phase: Phase::Start,
            })
            .unwrap_err();
        assert!(err.0.contains("'go'"), "{err}");
    }
}
