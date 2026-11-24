use std::io;
use std::process::Stdio;

use async_trait::async_trait;
use oxidepm_adapter_proto::messages::OutputStream;
use serde::Serialize;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

use crate::service::Emit;

#[async_trait]
pub trait SshRunner: Sync {
    async fn run(&self, argv: &[String]) -> io::Result<SshOutput>;
}

pub struct SshOutput {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

pub struct SystemSsh;

#[async_trait]
impl SshRunner for SystemSsh {
    async fn run(&self, argv: &[String]) -> io::Result<SshOutput> {
        let mut child = Command::new("ssh")
            .args(argv)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let (status, stdout, stderr) =
            tokio::try_join!(child.wait(), read_output(stdout), read_output(stderr))?;
        Ok(SshOutput {
            exit_code: status.code(),
            stdout,
            stderr,
        })
    }
}

// Keep reading after the capture fills so the remote command can finish normally.
async fn read_output(mut reader: impl AsyncRead + Unpin) -> io::Result<String> {
    const CAPTURE_BYTES: usize = 128 * 1024;
    let mut output = Vec::new();
    let mut truncated = false;
    let mut buffer = [0; 8192];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let keep = read.min(CAPTURE_BYTES - output.len());
        output.extend_from_slice(&buffer[..keep]);
        truncated |= keep < read;
    }
    let mut output = String::from_utf8_lossy(&output).into_owned();
    if truncated {
        output.push_str("\n[SSH output truncated]\n");
    }
    Ok(output)
}

#[derive(Serialize)]
pub struct Step {
    pub step: &'static str,
    pub argv: Vec<String>,
    pub command: String,
    pub ok: Option<bool>,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Step {
    pub fn new(step: &'static str, host: &str, command: String) -> Self {
        let argv = [
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=10",
            "--",
            host,
            &command,
        ]
        .map(String::from)
        .into();
        Self {
            step,
            argv,
            command,
            ok: None,
            exit_code: None,
            stdout: String::new(),
            stderr: String::new(),
        }
    }

    pub async fn run(
        &mut self,
        label: &str,
        dry_run: bool,
        runner: &impl SshRunner,
        emit: &mut Emit<'_>,
    ) {
        if dry_run {
            emit(
                OutputStream::Stdout,
                &serde_json::to_string(&self.argv).expect("SSH arguments are strings"),
            );
            return;
        }

        emit(OutputStream::Stdout, label);
        match runner.run(&self.argv).await {
            Ok(output) => {
                self.ok = Some(output.exit_code == Some(0));
                self.exit_code = output.exit_code;
                self.stdout = output.stdout;
                self.stderr = output.stderr;
            }
            Err(error) => {
                self.ok = Some(false);
                self.stderr = format!("Could not run ssh: {error}");
            }
        }
        emit_output(emit, OutputStream::Stdout, &self.stdout);
        emit_output(emit, OutputStream::Stderr, &self.stderr);
    }

    pub fn failed(&self) -> bool {
        self.ok == Some(false)
    }
}

fn emit_output(emit: &mut Emit<'_>, stream: OutputStream, mut text: &str) {
    const FRAME_BYTES: usize = 32 * 1024;
    while !text.is_empty() {
        let mut end = text.len().min(FRAME_BYTES);
        if end < text.len() {
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            if let Some(line_end) = text[..end].rfind('\n') {
                end = line_end + 1;
            }
        }
        emit(stream, &text[..end]);
        text = &text[end..];
    }
}

#[cfg(test)]
pub mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use super::*;

    pub struct FakeSsh {
        replies: Mutex<VecDeque<io::Result<SshOutput>>>,
        calls: Mutex<Vec<Vec<String>>>,
    }

    impl FakeSsh {
        pub fn new(replies: impl IntoIterator<Item = io::Result<SshOutput>>) -> Self {
            Self {
                replies: Mutex::new(replies.into_iter().collect()),
                calls: Mutex::new(Vec::new()),
            }
        }

        pub fn calls(&self) -> Vec<Vec<String>> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl SshRunner for FakeSsh {
        async fn run(&self, argv: &[String]) -> io::Result<SshOutput> {
            self.calls.lock().unwrap().push(argv.to_vec());
            self.replies
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected ssh call")
        }
    }

    pub fn output(exit_code: i32, stdout: &str) -> io::Result<SshOutput> {
        Ok(SshOutput {
            exit_code: Some(exit_code),
            stdout: stdout.into(),
            stderr: String::new(),
        })
    }

    #[tokio::test]
    async fn ssh_argv_always_includes_double_dash_before_host() {
        let ssh = FakeSsh::new([output(0, "")]);
        let mut step = Step::new("git_status", "myhost", "git status --short".into());
        step.run("Working tree", false, &ssh, &mut |_, _| {}).await;
        assert_eq!(
            ssh.calls()[0],
            [
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=10",
                "--",
                "myhost",
                "git status --short"
            ],
        );
    }
}
