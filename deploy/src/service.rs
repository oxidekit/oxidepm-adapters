use std::sync::Arc;

use async_trait::async_trait;
use oxidepm_adapter_proto::messages::{EventParams, InvokeParams, InvokeResult, OutputStream};
use oxidepm_adapter_proto::Limits;
use oxidepm_adapter_sdk::{Output, Service};
use serde::Serialize;
use serde_json::{json, Value};

use crate::shell::valid_host;
use crate::ssh::{SshRunner, Step, SystemSsh};
use crate::{deploy, diff};

pub struct Deploy;

pub type Emit<'a> = dyn FnMut(OutputStream, &str) + Send + 'a;

#[async_trait]
impl Service for Deploy {
    async fn event(self: Arc<Self>, _: EventParams) {}
    async fn shutdown(self: Arc<Self>) {}

    async fn invoke(self: Arc<Self>, call: InvokeParams, out: Output) -> InvokeResult {
        invoke(&SystemSsh, call, &mut |stream, text| match stream {
            OutputStream::Stdout => out.stdout(text),
            OutputStream::Stderr | OutputStream::Status => out.stderr(text),
        })
        .await
    }
}

pub async fn invoke(
    runner: &impl SshRunner,
    call: InvokeParams,
    emit: &mut Emit<'_>,
) -> InvokeResult {
    let usage = match call.verb.as_str() {
        "deploy" => deploy::USAGE,
        "diff" => diff::USAGE,
        _ => return answer(2, json!({ "error": "Unknown verb" })),
    };
    if matches!(call.argv.as_slice(), [flag] if flag == "--help" || flag == "-h") {
        emit(OutputStream::Stdout, usage);
        return answer(0, json!({ "usage": usage }));
    }

    let result = match call.verb.as_str() {
        "deploy" => deploy::run(runner, &call, emit).await,
        _ => diff::run(runner, &call, emit).await,
    };
    result.unwrap_or_else(|error| {
        emit(OutputStream::Stderr, &format!("{error}\n{usage}"));
        answer(2, json!({ "error": error }))
    })
}

pub fn answer(exit_code: i32, result: Value) -> InvokeResult {
    InvokeResult {
        exit_code,
        result: Some(result),
    }
}

const RESULT_BYTES: usize = Limits::PROTOCOL_1.max_line - 1024;

fn encoded_size(value: &impl Serialize) -> usize {
    serde_json::to_vec(value)
        .expect("invocation data is serializable")
        .len()
}

pub fn check_plan<'a>(
    fields: &Value,
    steps: impl IntoIterator<Item = &'a Step>,
) -> Result<(), String> {
    // Budget for the exact command and argv in each step before running anything.
    let mut bytes = encoded_size(fields);
    for step in steps {
        let argv_bytes = encoded_size(&step.argv);
        if argv_bytes > Limits::PROTOCOL_1.invoke_output_frame {
            return Err("SSH arguments are too long to report".into());
        }
        bytes += argv_bytes + encoded_size(&step.command) + 256;
    }
    if bytes > RESULT_BYTES {
        return Err("SSH commands are too long to report".into());
    }
    Ok(())
}

pub fn finish(exit_code: i32, mut fields: Value, mut steps: Vec<Step>) -> InvokeResult {
    // Progress was already emitted. When the reply itself would exceed the frame, retain a
    // bounded excerpt of each stream when there is room. The plan check reserves space
    // for the exit status and exact commands.
    let mut limit = 16 * 1024;
    fields["steps"] = json!(steps);
    while limit > 0 && encoded_size(&fields) > RESULT_BYTES {
        for step in &mut steps {
            truncate(&mut step.stdout, limit);
            truncate(&mut step.stderr, limit);
        }
        fields["steps"] = json!(steps);
        limit /= 2;
    }
    if limit == 0 && encoded_size(&fields) > RESULT_BYTES {
        for step in &mut steps {
            step.stdout.clear();
            step.stderr.clear();
        }
        fields["steps"] = json!(steps);
    }
    answer(exit_code, fields)
}

fn truncate(text: &mut String, mut limit: usize) {
    const MARKER: &str = "\n[SSH output truncated in result]\n";
    if text.len() > limit + MARKER.len() {
        while !text.is_char_boundary(limit) {
            limit -= 1;
        }
        text.truncate(limit);
        text.push_str(MARKER);
    }
}

pub struct CommonArgs<'a> {
    pub host: &'a str,
    pub cwd: Option<&'a str>,
    pub dry_run: bool,
}

pub fn parse_args<'a>(
    call: &'a InvokeParams,
    mut set_option: impl FnMut(&str, &'a str) -> Result<(), String>,
) -> Result<CommonArgs<'a>, String> {
    let mut host = None;
    let mut cwd = None;
    let mut dry_run = call.dry_run;
    let mut literal = false;
    let mut args = call.argv.iter().map(String::as_str);
    while let Some(arg) = args.next() {
        match arg {
            "--" if !literal => literal = true,
            "--dry-run" if !literal => dry_run = true,
            _ if literal || !arg.starts_with('-') => {
                if !valid_host(arg) {
                    return Err("Invalid SSH host".into());
                }
                if host.replace(arg).is_some() {
                    return Err("Expected one SSH host".into());
                }
            }
            _ => {
                let (flag, attached) = arg
                    .split_once('=')
                    .map_or((arg, None), |(flag, value)| (flag, Some(value)));
                if !flag.starts_with("--") {
                    return Err(format!("Unknown argument {arg}"));
                }
                let value = attached
                    .or_else(|| args.next())
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| format!("{flag} requires a value"))?;
                if flag == "--cwd" {
                    cwd = Some(value);
                } else {
                    set_option(flag, value)?;
                }
            }
        }
    }

    Ok(CommonArgs {
        host: host.ok_or("An SSH host is required")?,
        cwd,
        dry_run,
    })
}

#[cfg(test)]
pub mod tests {
    use oxidepm_adapter_proto::messages::ActorView;

    use super::*;

    #[test]
    fn escape_heavy_output_is_dropped_when_the_result_is_full() {
        let fields = json!({ "host": "origin", "dry_run": false });
        let cwd = crate::shell::cwd_command(Some(&"x".repeat(43_300)));
        let steps: Vec<_> = crate::diff::commands(&cwd, 5)
            .into_iter()
            .map(|(name, _, command)| {
                let mut step = Step::new(name, "origin", command);
                step.ok = Some(false);
                step.exit_code = Some(1);
                step.stdout = "\u{1}".repeat(34);
                step.stderr = step.stdout.clone();
                step
            })
            .collect();
        check_plan(&fields, &steps).unwrap();
        let mut with_output = fields.clone();
        with_output["steps"] = json!(steps);
        assert!(encoded_size(&with_output) > RESULT_BYTES);

        let argv = steps[0].argv.clone();
        let result = finish(1, fields, steps);
        assert_eq!(result.exit_code, 1);
        let data = result.result.unwrap();
        assert!(encoded_size(&data) <= RESULT_BYTES);
        assert_eq!(data["steps"][0]["argv"], json!(argv));
        for step in data["steps"].as_array().unwrap() {
            assert_eq!(step["stdout"], "");
            assert_eq!(step["stderr"], "");
            assert_eq!(step["exit_code"], 1);
        }
    }

    pub fn call(verb: &str, args: &[&str]) -> InvokeParams {
        InvokeParams {
            verb: verb.into(),
            argv: args.iter().map(|arg| (*arg).into()).collect(),
            cwd: "/project".into(),
            dry_run: false,
            actor: ActorView {
                class: "human".into(),
                label: "test".into(),
            },
            tty: false,
        }
    }
}
