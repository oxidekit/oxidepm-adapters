//! What the integration tests need: the real `oxidepmd` and `oxidepm` of the core repository (a
//! branch checked out next to this one, built once per test run), a private home for each test, the
//! adapter packaged and imported by its pinned hash, and a small HTTP server that plays the
//! messaging services.

#![allow(dead_code)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

pub fn backup_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap().to_os_string();
    name.push(".bak");
    path.with_file_name(name)
}

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
        std::fs::read(env!("CARGO_BIN_EXE_oxidepm-notifier")).expect("read the adapter binary");
    let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
        Vec::new(),
        flate2::Compression::fast(),
    ));
    for (name, data, mode) in [
        ("manifest.json", manifest.as_bytes(), 0o644),
        ("oxidepm-notifier", exec.as_slice(), 0o755),
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
}

impl Env {
    /// A daemon in a new home with the notifier imported and enabled. `daemon_env` is added to the
    /// daemon's environment (the adapter gets only what its manifest passes on).
    pub fn new(daemon_env: &[(&str, &str)]) -> Env {
        let mut env = Env::bare(daemon_env);
        env.install();
        env
    }

    /// A daemon in a new home, nothing installed.
    pub fn bare(daemon_env: &[(&str, &str)]) -> Env {
        let bins = core_bins();
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let stderr = std::fs::File::create(home.join("daemon-test.stderr")).unwrap();
        let child = Command::new(&bins.daemon)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .envs(daemon_env.iter().copied())
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
        };
        let socket = env.home.join("daemon.sock");
        assert!(
            wait_for(60, || UnixStream::connect(&socket).is_ok()),
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
        let file = self.dir.path().join("notifier.tar.gz");
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
        let enabled = self.cli(&[
            "adapter",
            "enable",
            "notifier",
            "--accept",
            &pkg.fingerprint,
        ]);
        assert_eq!(
            enabled.code,
            0,
            "enable failed:\n{}\n{}",
            enabled.all(),
            self.daemon_stderr()
        );
        assert!(
            self.wait_adapter_state("ready"),
            "the adapter is not ready: {}\n{}",
            self.adapter_info(),
            self.daemon_stderr()
        );
    }

    pub fn cli(&self, args: &[&str]) -> Run {
        let out = Command::new(&core_bins().cli)
            .args(args)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", &self.home)
            .env("OXIDEPM_HOME", &self.home)
            .current_dir(self.project())
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
        let run = self.cli(&["--json", "adapter", "info", "notifier"]);
        serde_json::from_str(&run.stdout).unwrap_or_else(|_| json!({ "unparsable": run.all() }))
    }

    pub fn wait_adapter_state(&self, state: &str) -> bool {
        wait_for(30, || self.adapter_info()["state"] == state)
    }

    pub fn state_dir(&self) -> PathBuf {
        self.home.join("adapters").join("state").join("notifier")
    }

    pub fn config_path(&self) -> PathBuf {
        self.state_dir().join("config.toml")
    }

    pub fn write_config(&self, text: &str) {
        std::fs::create_dir_all(self.state_dir()).unwrap();
        std::fs::write(self.config_path(), text).unwrap();
    }

    /// The adapter's stderr logs written by the daemon.
    pub fn adapter_stderr(&self) -> String {
        let dir = self.home.join("adapter-logs").join("notifier");
        let mut text = String::new();
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if name.starts_with("stderr") {
                    text.push_str(&std::fs::read_to_string(e.path()).unwrap_or_default());
                }
            }
        }
        text
    }

    /// One request on the main socket as a person.
    pub fn rpc(&self, request: Value) -> Value {
        self.try_rpc(request).expect("a daemon reply")
    }

    fn try_rpc(&self, request: Value) -> std::io::Result<Value> {
        let mut stream = UnixStream::connect(self.home.join("daemon.sock"))?;
        stream.set_read_timeout(Some(Duration::from_secs(60)))?;
        let envelope = json!({ "protocol": 2, "request_id": "t-1", "request": request });
        writeln!(stream, "{envelope}")?;
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line)?;
        let reply: Value = serde_json::from_str(&line)?;
        Ok(reply["response"].clone())
    }

    /// Start an app that runs `sh -c script`; `auto_restart` with a short delay when asked.
    pub fn start_app(&self, name: &str, script: &str, auto_restart: bool) -> Value {
        let mut spec = oxidepm_core::AppSpec::new(
            name.into(),
            oxidepm_core::Runtime::CMD,
            "sh".into(),
            self.project(),
        );
        spec.args = vec!["-c".into(), script.into()];
        spec.restart_policy.auto_restart = auto_restart;
        spec.restart_policy.max_restarts = 1000;
        spec.restart_policy.restart_delay_ms = 20;
        spec.restart_policy.max_restart_delay_ms = 50;
        self.rpc(json!({ "type": "start", "spec": spec }))
    }

    pub fn stop_all(&self) {
        let _ = self.rpc(json!({ "type": "stop", "selector": { "by": "all" } }));
    }

    pub fn delete_all(&self) {
        let _ = self.rpc(json!({ "type": "delete", "selector": { "by": "all" } }));
    }

    /// The pids of the running adapter processes (by their executable).
    pub fn adapter_pids(&self) -> Vec<i32> {
        let exe = self
            .home
            .join("adapters")
            .join("installed")
            .join("notifier");
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

    pub fn app_pids(&self) -> Vec<u32> {
        let status = self.rpc(json!({ "type": "status" }));
        status["apps"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| x["state"]["pid"].as_u64())
                    .map(|p| p as u32)
                    .collect()
            })
            .unwrap_or_default()
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let Some(mut child) = self.daemon.take() else {
            return;
        };
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        if !std::thread::panicking() {
            let _ = self.try_rpc(json!({ "type": "delete", "selector": { "by": "all" } }));
        }
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

#[test]
fn cleanup_reaps_a_daemon_that_exited_before_binding_its_socket() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir(&home).unwrap();
    let mut daemon = Command::new("/bin/sh")
        .args(["-c", "exit 1"])
        .spawn()
        .unwrap();
    assert_eq!(daemon.wait().unwrap().code(), Some(1));
    let env = Env {
        dir,
        home,
        daemon: Some(daemon),
    };

    drop(env);
}

// ---- the messaging services -------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Req {
    pub method: String,
    pub path: String,
    /// Header names in lower case.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Req {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
}

pub struct Reply {
    pub status: u16,
    pub body: String,
    pub delay: Duration,
}

impl Reply {
    pub fn ok() -> Reply {
        Reply {
            status: 200,
            body: "{\"ok\":true}".into(),
            delay: Duration::ZERO,
        }
    }

    pub fn status(status: u16, body: &str) -> Reply {
        Reply {
            status,
            body: body.into(),
            delay: Duration::ZERO,
        }
    }

    pub fn after(mut self, delay: Duration) -> Reply {
        self.delay = delay;
        self
    }
}

type Handler = Box<dyn Fn(&Req) -> Reply + Send + Sync>;

pub struct Mock {
    pub port: u16,
    requests: Arc<Mutex<Vec<Req>>>,
    handler: Arc<Mutex<Handler>>,
    stop: Arc<AtomicBool>,
}

impl Mock {
    pub fn start() -> Mock {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let handler: Arc<Mutex<Handler>> = Arc::new(Mutex::new(Box::new(|_| Reply::ok())));
        let stop = Arc::new(AtomicBool::new(false));
        let (r, h, s) = (
            Arc::clone(&requests),
            Arc::clone(&handler),
            Arc::clone(&stop),
        );
        std::thread::spawn(move || {
            while !s.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let (r, h) = (Arc::clone(&r), Arc::clone(&h));
                        std::thread::spawn(move || serve(stream, r, h));
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(5)),
                }
            }
        });
        Mock {
            port,
            requests,
            handler,
            stop,
        }
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{}", self.port, path)
    }

    pub fn base(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    pub fn respond_with(&self, f: impl Fn(&Req) -> Reply + Send + Sync + 'static) {
        *self.handler.lock().unwrap() = Box::new(f);
    }

    pub fn requests(&self) -> Vec<Req> {
        self.requests.lock().unwrap().clone()
    }

    pub fn requests_to(&self, prefix: &str) -> Vec<Req> {
        self.requests()
            .into_iter()
            .filter(|r| r.path.starts_with(prefix))
            .collect()
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn serve(mut stream: TcpStream, requests: Arc<Mutex<Vec<Req>>>, handler: Arc<Mutex<Handler>>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut first = String::new();
    if reader.read_line(&mut first).unwrap_or(0) == 0 {
        return;
    }
    let mut parts = first.split_whitespace();
    let (method, path) = (
        parts.next().unwrap_or("").to_string(),
        parts.next().unwrap_or("").to_string(),
    );
    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_lowercase(), v.trim().to_string()));
        }
    }
    let len: usize = headers
        .iter()
        .find(|(k, _)| k == "content-length")
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; len];
    if reader.read_exact(&mut body).is_err() {
        return;
    }
    let req = Req {
        method,
        path,
        headers,
        body,
    };
    let reply = (handler.lock().unwrap())(&req);
    requests.lock().unwrap().push(req);
    std::thread::sleep(reply.delay);
    let text = format!(
        "HTTP/1.1 {} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        reply.status,
        reply.body.len(),
        reply.body
    );
    let _ = stream.write_all(text.as_bytes());
}
