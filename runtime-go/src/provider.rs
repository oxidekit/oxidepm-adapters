//! What to run for a Go app: `go run <target> <arguments>`.

use oxidepm_adapter_proto::messages::{
    PlanParams, PlanResult, PrepareParams, PrepareResult, SpecView,
};
use oxidepm_adapter_sdk::{Refusal, RuntimeProvider};

pub struct Go;

fn check(spec: &SpecView) -> Result<(), Refusal> {
    if spec.runtime != "go" {
        return Err(Refusal::new(format!(
            "runtime-go does not provide the runtime '{}'",
            spec.runtime
        )));
    }
    if spec.command.trim().is_empty() {
        return Err(Refusal::new(
            "no package or file given (for the package in the app's directory the command is .)",
        ));
    }
    Ok(())
}

impl RuntimeProvider for Go {
    /// Nothing is built ahead of the start (`go run` compiles) and nothing is looked at: there
    /// are no steps.
    fn prepare(&self, params: &PrepareParams) -> Result<PrepareResult, Refusal> {
        check(&params.spec)?;
        Ok(PrepareResult {
            steps: Vec::new(),
            notes: Vec::new(),
        })
    }

    fn plan(&self, params: &PlanParams) -> Result<PlanResult, Refusal> {
        let spec = &params.spec;
        check(spec)?;
        let mut args = vec!["run".to_string(), spec.command.clone()];
        args.extend(spec.args.iter().cloned());
        Ok(PlanResult {
            program: "go".to_string(),
            args,
            cwd: spec.cwd.clone(),
            extra_env: Default::default(),
            binary_path: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxidepm_adapter_proto::messages::Phase;

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

    #[test]
    fn go_run_gets_the_target_and_then_the_arguments() {
        for (command, args) in [
            (".", vec!["--port", "8080"]),
            ("main.go", vec![]),
            ("./cmd/server", vec!["a b"]),
        ] {
            let plan = Go
                .plan(&PlanParams {
                    spec: spec("go", command, &args),
                    steps_done: vec![],
                })
                .unwrap();
            assert_eq!(plan.program, "go");
            let mut want = vec!["run".to_string(), command.to_string()];
            want.extend(args.iter().map(|a| a.to_string()));
            assert_eq!(plan.args, want);
            assert_eq!(plan.cwd, "/srv/app");
            assert!(plan.extra_env.is_empty() && plan.binary_path.is_none());
        }
    }

    #[test]
    fn there_are_no_steps() {
        let prepared = Go
            .prepare(&PrepareParams {
                spec: spec("go", ".", &[]),
                path_hint: "/usr/bin".into(),
                phase: Phase::Start,
            })
            .unwrap();
        assert!(prepared.steps.is_empty());
    }

    #[test]
    fn another_runtime_or_an_empty_command_is_refused_with_a_reason() {
        let other = Go
            .prepare(&PrepareParams {
                spec: spec("rust", "x.rs", &[]),
                path_hint: String::new(),
                phase: Phase::Start,
            })
            .unwrap_err();
        assert!(other.0.contains("'rust'"), "{other}");
        let empty = Go
            .plan(&PlanParams {
                spec: spec("go", " ", &[]),
                steps_done: vec![],
            })
            .unwrap_err();
        assert!(empty.0.contains("no package or file"), "{empty}");
    }
}
