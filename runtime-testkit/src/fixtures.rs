//! Helpers for locating and driving the shell test fixtures under
//! `runs/tests/fixtures/` and `unreal/tests/fixtures/`.

use std::io::{BufRead, BufReader};
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::Duration;

/// Exit code emitted by UnrealBuildTool when its single-instance mutex is already held.
pub const CONFLICTING_INSTANCE_EXIT_CODE: i32 = 10;

/// Leading substring of UnrealBuildTool's single-instance conflict message.
pub const CONFLICTING_INSTANCE_PREFIX: &str =
    r"A conflicting instance of Global\UnrealBuildTool_Mutex_";

/// Marker emitted by fixtures in `hang` mode right before blocking.
pub const HANG_READY_MARKER: &str = "FAKE_HANG_READY";

/// Workspace root of the adapters repository.
pub fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("runtime-testkit lives directly under the workspace root")
        .to_path_buf()
}

/// Directory containing the fake Unreal tool scripts and scrubbed log samples.
pub fn unreal_fixtures_dir() -> PathBuf {
    workspace_root().join("unreal/tests/fixtures")
}

/// Directory containing the scrubbed Unreal log samples.
pub fn unreal_samples_dir() -> PathBuf {
    unreal_fixtures_dir().join("samples")
}

/// Path to one fake Unreal fixture script (for example `"fake-ubt"`).
pub fn unreal_fixture(name: &str) -> PathBuf {
    unreal_fixtures_dir().join(name)
}

/// Directory containing the fake host (`ssh` / `rsync`) scripts.
pub fn runs_fixtures_dir() -> PathBuf {
    workspace_root().join("runs/tests/fixtures")
}

/// Path to one fake runs fixture script (for example `"fake-ssh"`).
pub fn runs_fixture(name: &str) -> PathBuf {
    runs_fixtures_dir().join(name)
}

/// A temporary local directory acting as a fake SSH/rsync target host, plus a
/// `bin/` directory exposing `ssh` and `rsync` stubs that can be prepended to `PATH`.
pub struct FakeHost {
    _dir: tempfile::TempDir,
    pub root: PathBuf,
    pub bin_dir: PathBuf,
    pub ssh_log: PathBuf,
    pub rsync_log: PathBuf,
}

impl FakeHost {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().expect("create fake host tempdir");
        let root = dir.path().join("host-root");
        let bin_dir = dir.path().join("bin");
        let ssh_log = dir.path().join("ssh.log");
        let rsync_log = dir.path().join("rsync.log");

        std::fs::create_dir_all(&root).expect("create fake host root");
        std::fs::create_dir_all(&bin_dir).expect("create fake host bin dir");
        symlink(runs_fixture("fake-ssh"), bin_dir.join("ssh")).expect("link fake ssh");
        symlink(runs_fixture("fake-rsync"), bin_dir.join("rsync")).expect("link fake rsync");

        Self {
            _dir: dir,
            root,
            bin_dir,
            ssh_log,
            rsync_log,
        }
    }

    /// `PATH` value with this fake host's `bin_dir` prepended.
    pub fn path_env(&self) -> String {
        let current = std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".to_string());
        format!("{}:{current}", self.bin_dir.display())
    }

    /// Apply the fake host's `PATH`, `FAKE_HOST_ROOT`, and call-log variables to `cmd`.
    pub fn configure_command<'a>(&self, cmd: &'a mut Command) -> &'a mut Command {
        cmd.env("PATH", self.path_env())
            .env("FAKE_HOST_ROOT", &self.root)
            .env("FAKE_SSH_LOG", &self.ssh_log)
            .env("FAKE_RSYNC_LOG", &self.rsync_log)
    }

    /// Recorded `ssh` invocations (one tab-separated line per call).
    pub fn ssh_calls(&self) -> Vec<String> {
        read_lines(&self.ssh_log)
    }

    /// Recorded `rsync` invocations (one tab-separated line per call).
    pub fn rsync_calls(&self) -> Vec<String> {
        read_lines(&self.rsync_log)
    }
}

impl Default for FakeHost {
    fn default() -> Self {
        Self::new()
    }
}

fn read_lines(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(ToOwned::to_owned)
        .collect()
}

/// Spawn a fixture in `hang` mode, wait until it prints `FAKE_HANG_READY`, verify
/// it is still running, kill only that child process, and reap it.
pub fn run_hang_and_kill(mut cmd: Command, timeout: Duration) -> ExitStatus {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn hanging fixture");
    wait_for_hang_ready_and_kill(&mut child, timeout)
}

/// Wait for `HANG_READY_MARKER` on `child`'s piped stdout, assert the child is
/// still alive, then terminate and reap only `child`.
pub fn wait_for_hang_ready_and_kill(child: &mut Child, timeout: Duration) -> ExitStatus {
    let stdout = child.stdout.take().expect("piped stdout on hanging child");
    let (tx, rx) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut buf = BufReader::new(stdout);
        let mut line = String::new();
        while let Ok(n) = buf.read_line(&mut line) {
            if n == 0 {
                break;
            }
            if line.trim_end() == HANG_READY_MARKER {
                let _ = tx.send(());
                break;
            }
            line.clear();
        }
    });

    let ready = rx.recv_timeout(timeout);
    if ready.is_err() {
        let _ = child.kill();
        let _ = child.wait();
        let _ = reader.join();
        panic!("hanging fixture did not emit {HANG_READY_MARKER} within {timeout:?}");
    }

    assert!(
        matches!(child.try_wait(), Ok(None)),
        "hanging fixture exited before being killed"
    );

    let _ = nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(child.id() as i32),
        nix::sys::signal::Signal::SIGTERM,
    );
    let status = child.wait().expect("reap killed hanging fixture");
    let _ = reader.join();
    status
}
