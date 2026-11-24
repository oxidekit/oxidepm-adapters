//! What to run for each runtime.

use std::path::Path;

use oxidepm_adapter_proto::messages::{
    PlanParams, PlanResult, PrepareParams, PrepareResult, SpecView,
};
use oxidepm_adapter_sdk::{Refusal, RuntimeProvider};

pub struct Node;

/// The tool that runs a package script, for the three runtimes that do.
fn package_tool(runtime: &str) -> Option<&'static str> {
    match runtime {
        "npm" => Some("npm"),
        "pnpm" => Some("pnpm"),
        "yarn" => Some("yarn"),
        _ => None,
    }
}

fn check(spec: &SpecView) -> Result<(), Refusal> {
    if spec.runtime != "node" && package_tool(&spec.runtime).is_none() {
        return Err(Refusal::new(format!(
            "runtime-node does not provide the runtime '{}'",
            spec.runtime
        )));
    }
    if spec.command.trim().is_empty() {
        return Err(Refusal::new(if spec.runtime == "node" {
            "no script given"
        } else {
            "no package script given"
        }));
    }
    Ok(())
}

impl RuntimeProvider for Node {
    /// Nothing is built and nothing is looked at: there are no steps.
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
        let (program, mut args) = match package_tool(&spec.runtime) {
            // `npm run <script> -- <arguments>`: without the `--`, npm takes arguments that look
            // like options (`--port 80`) for its own
            Some("npm") => ("npm", {
                let mut args = vec!["run".to_string(), spec.command.clone()];
                if !spec.args.is_empty() {
                    args.push("--".to_string());
                }
                args
            }),
            Some(tool) => (tool, vec!["run".to_string(), spec.command.clone()]),
            None => {
                let script = if Path::new(&spec.command).is_absolute() {
                    spec.command.clone()
                } else {
                    Path::new(&spec.cwd)
                        .join(&spec.command)
                        .to_string_lossy()
                        .to_string()
                };
                ("node", vec![script])
            }
        };
        args.extend(spec.args.iter().cloned());
        Ok(PlanResult {
            program: program.to_string(),
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

    fn plan(runtime: &str, command: &str, args: &[&str]) -> Result<PlanResult, Refusal> {
        Node.plan(&PlanParams {
            spec: spec(runtime, command, args),
            steps_done: vec![],
        })
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_script_runs_under_node_from_the_apps_directory() {
        let plan = plan("node", "server.js", &["--port", "3000"]).unwrap();
        assert_eq!(plan.program, "node");
        assert_eq!(
            plan.args,
            strings(&["/srv/app/server.js", "--port", "3000"])
        );
        assert_eq!(plan.cwd, "/srv/app");
        assert!(plan.extra_env.is_empty() && plan.binary_path.is_none());
        // an absolute script is used as it is
        assert_eq!(
            plan_args("node", "/opt/x/main.mjs", &[]),
            strings(&["/opt/x/main.mjs"])
        );
    }

    fn plan_args(runtime: &str, command: &str, args: &[&str]) -> Vec<String> {
        plan(runtime, command, args).unwrap().args
    }

    #[test]
    fn npm_puts_a_double_dash_before_the_arguments_of_the_script() {
        let plan = plan("npm", "start", &["--port", "80"]).unwrap();
        assert_eq!(plan.program, "npm");
        assert_eq!(plan.args, strings(&["run", "start", "--", "--port", "80"]));
        // no arguments, no `--`
        assert_eq!(plan_args("npm", "start", &[]), strings(&["run", "start"]));
    }

    #[test]
    fn pnpm_and_yarn_pass_the_arguments_as_they_are() {
        // both hand the arguments after the script name to the script, and pnpm would pass a
        // `--` on to it
        let pnpm = plan("pnpm", "dev", &["--host"]).unwrap();
        assert_eq!(
            (pnpm.program.as_str(), pnpm.args),
            ("pnpm", strings(&["run", "dev", "--host"]))
        );
        let yarn = plan("yarn", "dev", &["--host"]).unwrap();
        assert_eq!(
            (yarn.program.as_str(), yarn.args),
            ("yarn", strings(&["run", "dev", "--host"]))
        );
    }

    #[test]
    fn there_are_no_steps_and_no_environment() {
        for runtime in ["node", "npm", "pnpm", "yarn"] {
            let prepared = Node
                .prepare(&PrepareParams {
                    spec: spec(runtime, "x", &[]),
                    path_hint: "/usr/bin".into(),
                    phase: Phase::Start,
                })
                .unwrap();
            assert!(prepared.steps.is_empty(), "{runtime}");
            assert!(
                plan(runtime, "x", &[]).unwrap().extra_env.is_empty(),
                "{runtime}"
            );
        }
    }

    #[test]
    fn another_runtime_or_an_empty_command_is_refused_with_a_reason() {
        let other = Node
            .prepare(&PrepareParams {
                spec: spec("python", "app.py", &[]),
                path_hint: String::new(),
                phase: Phase::Start,
            })
            .unwrap_err();
        assert!(other.0.contains("python"), "{other}");
        assert!(plan("node", "  ", &[]).unwrap_err().0.contains("no script"));
        assert!(plan("npm", "", &[])
            .unwrap_err()
            .0
            .contains("no package script"));
    }
}
