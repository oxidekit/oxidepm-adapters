//! What the integration tests need: the real `oxidepmd` and `oxidepm` of the core repository (a
//! branch checked out next to this one, built once per test run), a private home for each test,
//! the adapter packaged and imported by its pinned hash, and a pseudo-terminal with a small
//! screen on the other end for the CLI to run `oxidepm monit` on.

#![allow(dead_code)]

use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

pub const ADAPTER: &str = "ui-tui";

pub fn hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub fn wait_for(secs: u64, mut f: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if f() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

pub fn pid_alive(pid: i32) -> bool {
    // a zombie still answers signal 0 but has nothing left to do
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat
            .rsplit(')')
            .next()
            .map(|rest| !rest.trim_start().starts_with('Z'))
            .unwrap_or(false),
        Err(_) => false,
    }
}

// ---- the core repository's binaries -----------------------------------------------------------

struct CoreBins {
    daemon: PathBuf,
    cli: PathBuf,
}

/// Where the core repository is: `OXIDEPM_CORE_DIR`, else the sibling directory `oxidepm`.
fn core_dir() -> PathBuf {
    std::env::var_os("OXIDEPM_CORE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../oxidepm"))
}

/// The daemon, the CLI and the sandbox launcher, built (a no-op when current) into
/// `OXIDEPM_CORE_TARGET_DIR`, else the core repository's own target directory.
fn core_bins() -> &'static CoreBins {
    static BINS: OnceLock<CoreBins> = OnceLock::new();
    BINS.get_or_init(|| {
        let core = core_dir();
        let target = std::env::var_os("OXIDEPM_CORE_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| core.join("target"));
        let output = Command::new(env!("CARGO"))
            .args([
                "build",
                "-p",
                "oxidepmd",
                "-p",
                "oxidepm",
                "-p",
                "oxidepm-sandbox",
                "--offline",
                "--quiet",
            ])
            .current_dir(&core)
            .env("CARGO_TARGET_DIR", &target)
            .output()
            .expect("run cargo to build the core");
        assert!(
            output.status.success(),
            "building the core failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let debug = target.join("debug");
        CoreBins {
            daemon: debug.join("oxidepmd"),
            cli: debug.join("oxidepm"),
        }
    })
}

// ---- the packaged adapter ---------------------------------------------------------------------

pub struct Package {
    pub archive: Vec<u8>,
    pub archive_sha: String,
    pub fingerprint: String,
}

pub fn manifest_text() -> String {
    std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("manifest.json"))
        .expect("read manifest.json")
}

pub fn package() -> Package {
    let manifest = manifest_text();
    let exec =
        std::fs::read(env!("CARGO_BIN_EXE_oxidepm-ui-tui")).expect("read the adapter binary");
    let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
        Vec::new(),
        flate2::Compression::fast(),
    ));
    for (name, data, mode) in [
        ("manifest.json", manifest.as_bytes(), 0o644),
        ("oxidepm-ui-tui", exec.as_slice(), 0o755),
    ] {
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(mode);
        header.set_entry_type(tar::EntryType::Regular);
        builder.append_data(&mut header, name, data).unwrap();
    }
    let archive = builder.into_inner().unwrap().finish().unwrap();
    let fingerprint = oxidepm_adapter_proto::Manifest::parse(manifest.as_bytes())
        .expect("the manifest is valid")
        .fingerprint(&hex(&exec));
    Package {
        archive_sha: hex(&archive),
        archive,
        fingerprint,
    }
}

// ---- a daemon and a home ----------------------------------------------------------------------

pub struct Run {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Run {
    pub fn all(&self) -> String {
        format!("{}{}", self.stdout, self.stderr)
    }
}

pub struct Env {
    pub dir: tempfile::TempDir,
    pub home: PathBuf,
    daemon: Option<Child>,
    fingerprint: String,
}

impl Env {
    /// A daemon in a new home with the adapter imported and enabled.
    pub fn new() -> Env {
        let mut env = Env::bare();
        env.install();
        env
    }

    /// A daemon in a new home, nothing installed.
    pub fn bare() -> Env {
        let bins = core_bins();
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let stderr = std::fs::File::create(home.join("daemon-test.stderr")).unwrap();
        let child = Command::new(&bins.daemon)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("OXIDEPM_HOME", &home)
            .env("HOME", &home)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr)
            .spawn()
            .expect("spawn oxidepmd");
        std::fs::create_dir_all(dir.path().join("project")).unwrap();
        let env = Env {
            dir,
            home,
            daemon: Some(child),
            fingerprint: String::new(),
        };
        let socket = env.home.join("daemon.sock");
        assert!(
            wait_for(20, || UnixStream::connect(&socket).is_ok()),
            "the daemon did not come up: {}",
            env.daemon_stderr()
        );
        env
    }

    /// SIGTERM, and wait for the exit: what a reboot does.
    pub fn shutdown_daemon(&mut self) {
        if let Some(mut child) = self.daemon.take() {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(child.id() as i32),
                nix::sys::signal::Signal::SIGTERM,
            );
            let _ = child.wait();
        }
    }

    /// A working directory for the CLI and the apps: a verb is not run in a directory that holds
    /// the daemon's home.
    pub fn project(&self) -> PathBuf {
        std::fs::canonicalize(self.dir.path().join("project")).unwrap()
    }

    pub fn daemon_stderr(&self) -> String {
        std::fs::read_to_string(self.home.join("daemon-test.stderr")).unwrap_or_default()
    }

    /// Import by the pinned hash (the name is reserved, and no key signs this build) and enable.
    pub fn install(&mut self) {
        let pkg = package();
        self.fingerprint = pkg.fingerprint.clone();
        let file = self.dir.path().join("ui-tui.tar.gz");
        std::fs::write(&file, &pkg.archive).unwrap();
        let imported = self.cli(&[
            "adapter",
            "import",
            file.to_str().unwrap(),
            "--sha256",
            &pkg.archive_sha,
            "--allow-reserved-name",
        ]);
        assert_eq!(imported.code, 0, "import failed:\n{}", imported.all());
        let enabled = self.cli(&["adapter", "enable", ADAPTER, "--accept", &self.fingerprint]);
        assert_eq!(
            enabled.code,
            0,
            "enable failed:\n{}\n{}",
            enabled.all(),
            self.daemon_stderr()
        );
        assert!(
            self.wait_adapter_state("stopped") || self.wait_adapter_state("ready"),
            "the adapter is not enabled: {}\n{}",
            self.adapter_info(),
            self.daemon_stderr()
        );
    }

    pub fn cli_command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(&core_bins().cli);
        cmd.args(args)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", &self.home)
            .env("OXIDEPM_HOME", &self.home)
            .current_dir(self.project());
        cmd
    }

    pub fn cli(&self, args: &[&str]) -> Run {
        let out = self
            .cli_command(args)
            .stdin(Stdio::null())
            .output()
            .expect("run the CLI");
        Run {
            code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).to_string(),
            stderr: String::from_utf8_lossy(&out.stderr).to_string(),
        }
    }

    pub fn adapter_info(&self) -> Value {
        let run = self.cli(&["--json", "adapter", "info", ADAPTER]);
        serde_json::from_str(&run.stdout).unwrap_or_else(|_| json!({ "unparsable": run.all() }))
    }

    pub fn wait_adapter_state(&self, state: &str) -> bool {
        wait_for(30, || self.adapter_info()["state"] == state)
    }

    pub fn state_dir(&self) -> PathBuf {
        self.home.join("adapters").join("state").join(ADAPTER)
    }

    /// The last lines of the adapter's stderr (its log).
    pub fn adapter_stderr(&self) -> String {
        let mut text = String::new();
        if let Ok(entries) = std::fs::read_dir(self.state_dir()) {
            for e in entries.flatten() {
                if e.file_name().to_string_lossy().starts_with("stderr") {
                    text.push_str(&std::fs::read_to_string(e.path()).unwrap_or_default());
                }
            }
        }
        text
    }

    /// One request on the main socket as a person.
    pub fn rpc(&self, request: Value) -> Value {
        let mut stream = UnixStream::connect(self.home.join("daemon.sock")).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(60)))
            .unwrap();
        let envelope = json!({ "protocol": 2, "request_id": "t-1", "request": request });
        writeln!(stream, "{envelope}").unwrap();
        let mut line = String::new();
        BufReader::new(stream)
            .read_line(&mut line)
            .expect("read the answer");
        let reply: Value = serde_json::from_str(&line).expect("a JSON answer");
        reply["response"].clone()
    }

    /// Start an app that runs `sh -c script`.
    pub fn start_app(&self, name: &str, script: &str) -> Value {
        let mut spec = oxidepm_core::AppSpec::new(
            name.into(),
            oxidepm_core::Runtime::CMD,
            "sh".into(),
            self.project(),
        );
        spec.args = vec!["-c".into(), script.into()];
        spec.restart_policy.auto_restart = false;
        self.rpc(json!({ "type": "start", "spec": spec }))
    }

    /// The state of an app: its status string and pid.
    pub fn app_state(&self, name: &str) -> Option<(String, Option<u32>)> {
        let status = self.rpc(json!({ "type": "status" }));
        status["apps"]
            .as_array()?
            .iter()
            .find(|x| x["spec"]["name"] == name)
            .map(|x| {
                (
                    x["state"]["status"].as_str().unwrap_or("").to_string(),
                    x["state"]["pid"].as_u64().map(|p| p as u32),
                )
            })
    }

    /// Wait until the app is running with a pid, and return it.
    pub fn wait_running(&self, name: &str) -> u32 {
        let mut pid = 0;
        assert!(
            wait_for(20, || {
                pid = self.app_state(name).and_then(|(_, pid)| pid).unwrap_or(0);
                pid != 0
            }),
            "{name} did not come up"
        );
        pid
    }

    pub fn delete_all(&self) {
        let _ = self.rpc(json!({ "type": "delete", "selector": { "by": "all" } }));
    }

    /// The pids of the running adapter processes (by their executable).
    pub fn adapter_pids(&self) -> Vec<i32> {
        let exe = self.home.join("adapters").join("installed").join(ADAPTER);
        let mut pids = Vec::new();
        if let Ok(entries) = std::fs::read_dir("/proc") {
            for e in entries.flatten() {
                let Some(pid) = e.file_name().to_str().and_then(|n| n.parse::<i32>().ok()) else {
                    continue;
                };
                if let Ok(target) = std::fs::read_link(e.path().join("exe")) {
                    if target.starts_with(&exe) {
                        pids.push(pid);
                    }
                }
            }
        }
        pids
    }

    /// Everything the audit log holds, as lines.
    pub fn audit_text(&self) -> String {
        let mut text = String::new();
        let dir = self.home.join("audit");
        if let Ok(entries) = std::fs::read_dir(&dir) {
            let mut files: Vec<_> = entries.flatten().map(|e| e.path()).collect();
            files.sort();
            for f in files {
                text.push_str(&std::fs::read_to_string(f).unwrap_or_default());
            }
        }
        text
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        if self.daemon.is_some() {
            self.delete_all();
        }
        if let Some(mut child) = self.daemon.take() {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(child.id() as i32),
                nix::sys::signal::Signal::SIGTERM,
            );
            let deadline = Instant::now() + Duration::from_secs(15);
            while Instant::now() < deadline {
                if matches!(child.try_wait(), Ok(Some(_))) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

// ---- a terminal -------------------------------------------------------------------------------

/// A pseudo-terminal with a screen on the master side: what the dashboard writes is kept and
/// drawn onto a grid of cells, the way a terminal would.
pub struct Screen {
    master: std::fs::File,
    slave: Option<OwnedFd>,
    bytes: Arc<Mutex<Vec<u8>>>,
    stop: Arc<AtomicBool>,
    reader: Option<std::thread::JoinHandle<()>>,
    pub width: u16,
    pub height: u16,
}

fn set_size(fd: RawFd, width: u16, height: u16) {
    let size = libc::winsize {
        ws_row: height,
        ws_col: width,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: TIOCSWINSZ reads one winsize.
    let rc = unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &size) };
    assert_eq!(rc, 0, "TIOCSWINSZ: {}", std::io::Error::last_os_error());
}

impl Screen {
    pub fn new(width: u16, height: u16) -> Screen {
        let (mut master, mut slave): (RawFd, RawFd) = (-1, -1);
        // SAFETY: openpty fills the two descriptors; the null pointers ask for no name, terminal
        // settings or window size.
        let rc = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(rc, 0, "openpty: {}", std::io::Error::last_os_error());
        // SAFETY: both descriptors were just made for us.
        let (master, slave) = unsafe {
            (
                std::fs::File::from_raw_fd(master),
                OwnedFd::from_raw_fd(slave),
            )
        };
        set_size(master.as_raw_fd(), width, height);
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let flags = unsafe { libc::fcntl(master.as_raw_fd(), libc::F_GETFL) };
        unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) };
        let reader = {
            let mut master = master.try_clone().unwrap();
            let (bytes, stop) = (bytes.clone(), stop.clone());
            std::thread::spawn(move || {
                let mut buf = [0u8; 8192];
                while !stop.load(Ordering::Relaxed) {
                    match master.read(&mut buf) {
                        Ok(0) => std::thread::sleep(Duration::from_millis(10)),
                        Ok(n) => bytes.lock().unwrap().extend_from_slice(&buf[..n]),
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(10))
                        }
                        // every slave descriptor is closed
                        Err(_) => std::thread::sleep(Duration::from_millis(10)),
                    }
                }
            })
        };
        Screen {
            master,
            slave: Some(slave),
            bytes,
            stop,
            reader: Some(reader),
            width,
            height,
        }
    }

    /// A descriptor for a child's standard input or output.
    pub fn stdio(&self) -> Stdio {
        Stdio::from(
            self.slave
                .as_ref()
                .expect("the slave is still ours")
                .try_clone()
                .unwrap(),
        )
    }

    /// Let go of our own copy of the slave side (the child has its own).
    pub fn release_slave(&mut self) {
        self.slave = None;
    }

    pub fn master_fd(&self) -> RawFd {
        self.master.as_raw_fd()
    }

    /// Type the bytes as the person would.
    pub fn type_keys(&mut self, keys: &str) {
        self.master.write_all(keys.as_bytes()).unwrap();
    }

    /// The terminal window changes size.
    pub fn resize(&mut self, width: u16, height: u16) {
        set_size(self.master.as_raw_fd(), width, height);
        self.width = width;
        self.height = height;
    }

    /// Everything written so far.
    pub fn raw(&self) -> Vec<u8> {
        self.bytes.lock().unwrap().clone()
    }

    pub fn raw_text(&self) -> String {
        String::from_utf8_lossy(&self.raw()).into_owned()
    }

    /// The rows of the screen as it looks now.
    pub fn rows(&self) -> Vec<String> {
        render(&self.raw(), self.width as usize, self.height as usize)
    }

    pub fn text(&self) -> String {
        self.rows().join("\n")
    }

    /// Wait until the screen shows `needle`; the screen is returned (and printed by a failed assert).
    pub fn wait_for_text(&self, needle: &str, secs: u64) -> bool {
        wait_for(secs, || self.text().contains(needle))
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

/// Draw a stream of terminal output onto a grid. It knows what the dashboard sends: printable
/// text, cursor positioning, erasing, and the sequences it can ignore (colours, modes).
pub fn render(bytes: &[u8], width: usize, height: usize) -> Vec<String> {
    let mut grid = vec![vec![' '; width]; height];
    let (mut row, mut col) = (0usize, 0usize);
    let text = String::from_utf8_lossy(bytes);
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.peek() {
                Some('[') => {
                    chars.next();
                    let mut params = String::new();
                    let mut last = ' ';
                    for n in chars.by_ref() {
                        if ('\x40'..='\x7e').contains(&n) {
                            last = n;
                            break;
                        }
                        params.push(n);
                    }
                    let private = params.starts_with('?');
                    let numbers: Vec<usize> = params
                        .trim_start_matches('?')
                        .split(';')
                        .map(|p| p.parse().unwrap_or(0))
                        .collect();
                    let first = numbers.first().copied().unwrap_or(0);
                    match last {
                        'H' | 'f' if !private => {
                            row = numbers.first().copied().unwrap_or(1).max(1) - 1;
                            col = numbers.get(1).copied().unwrap_or(1).max(1) - 1;
                        }
                        'J' => match first {
                            0 => {
                                for (r, cells) in grid.iter_mut().enumerate().skip(row) {
                                    for cell in
                                        cells.iter_mut().skip(if r == row { col } else { 0 })
                                    {
                                        *cell = ' ';
                                    }
                                }
                            }
                            _ => grid = vec![vec![' '; width]; height],
                        },
                        'K' => {
                            if row < height {
                                let from = if first == 0 { col } else { 0 };
                                let to = if first == 1 { col + 1 } else { width };
                                for cell in grid[row].iter_mut().take(to.min(width)).skip(from) {
                                    *cell = ' ';
                                }
                            }
                        }
                        'A' => row = row.saturating_sub(first.max(1)),
                        'B' => row += first.max(1),
                        'C' => col += first.max(1),
                        'D' => col = col.saturating_sub(first.max(1)),
                        'h' | 'l' if private && first == 1049 => {
                            grid = vec![vec![' '; width]; height]
                        }
                        _ => {}
                    }
                }
                Some(']') => {
                    // an operating system command ends at BEL or ST
                    for n in chars.by_ref() {
                        if n == '\x07' || n == '\\' {
                            break;
                        }
                    }
                }
                _ => {
                    chars.next();
                }
            },
            '\r' => col = 0,
            '\n' => row += 1,
            c if c.is_control() => {}
            c => {
                if row < height && col < width {
                    grid[row][col] = c;
                }
                col += 1;
            }
        }
    }
    grid.into_iter()
        .map(|r| r.into_iter().collect::<String>().trim_end().to_string())
        .collect()
}

/// The terminal settings of a descriptor.
pub fn termios_of(fd: RawFd) -> libc::termios {
    // SAFETY: tcgetattr fills a termios the size of the struct we give it.
    unsafe {
        let mut settings: libc::termios = std::mem::zeroed();
        assert_eq!(
            libc::tcgetattr(fd, &mut settings),
            0,
            "tcgetattr: {}",
            std::io::Error::last_os_error()
        );
        settings
    }
}

/// Wait for a child to exit; killed (and failed) after `secs`.
pub fn wait_child(child: &mut Child, secs: u64) -> Option<i32> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status.code().or(Some(-1));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}
