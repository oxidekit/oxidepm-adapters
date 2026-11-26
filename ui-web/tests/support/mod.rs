//! What the integration tests need: the real `oxidepmd` and `oxidepm` of the core repository (a
//! branch checked out next to this one, built once per test run), a private home for each test, the
//! adapter packaged and imported by its pinned hash, a small HTTP client and a WebSocket client.
//! (The first half follows `exporter-prometheus/tests/support`.)

#![allow(dead_code)]

use std::collections::HashSet;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{LazyLock, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

pub const ADAPTER: &str = "ui-web";
/// The key the tests put in the configuration (a test needs to know it).
pub const KEY: &str = "test-key-0123456789-abcdefghij";

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

/// The manifest with `network.listen` set to the given port: the sandbox lets the adapter bind
/// exactly the ports its manifest lists, and a test must not use the shipped port. With `admin`
/// the manifest asks for the admin level (the shipped one does not; a level can only be lowered).
pub fn manifest_for(port: u16, admin: bool) -> String {
    let mut manifest: Value = serde_json::from_str(&manifest_text()).expect("the manifest is JSON");
    manifest["permissions"]["network"]["listen"] = json!([format!("127.0.0.1:{port}")]);
    if admin {
        manifest["permissions"]["host"]["level"] = json!("admin");
    }
    serde_json::to_string_pretty(&manifest).unwrap()
}

pub fn package(port: u16, admin: bool) -> Package {
    let manifest = manifest_for(port, admin);
    let exec =
        std::fs::read(env!("CARGO_BIN_EXE_oxidepm-ui-web")).expect("read the adapter binary");
    let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
        Vec::new(),
        flate2::Compression::fast(),
    ));
    for (name, data, mode) in [
        ("manifest.json", manifest.as_bytes(), 0o644),
        ("oxidepm-ui-web", exec.as_slice(), 0o755),
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
    /// A daemon in a new home with the adapter imported and enabled, its manifest listing `port`.
    /// The configuration is `listen = 127.0.0.1:port` and the test key, plus `extra` lines; it is
    /// written before the adapter starts because it is read once.
    pub fn new(port: u16, extra: &str) -> Env {
        Env::with(
            port,
            Some(&format!(
                "listen = \"127.0.0.1:{port}\"\nkey = \"{KEY}\"\n{extra}"
            )),
            false,
        )
    }

    /// Like `new` with the admin level in the manifest and in the grant.
    pub fn new_admin(port: u16, extra: &str) -> Env {
        Env::with(
            port,
            Some(&format!(
                "listen = \"127.0.0.1:{port}\"\nkey = \"{KEY}\"\n{extra}"
            )),
            true,
        )
    }

    /// A configuration of the test's own (or none: the adapter makes it), and the level asked for.
    pub fn with(port: u16, config: Option<&str>, admin: bool) -> Env {
        let mut env = Env::bare(&[]);
        if let Some(config) = config {
            env.write_config(config);
        }
        env.install(port, admin);
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

    /// A working directory for the CLI and the apps: a verb is not run in a directory that holds
    /// the daemon's home.
    pub fn project(&self) -> PathBuf {
        std::fs::canonicalize(self.dir.path().join("project")).unwrap()
    }

    pub fn daemon_stderr(&self) -> String {
        std::fs::read_to_string(self.home.join("daemon-test.stderr")).unwrap_or_default()
    }

    /// Import by the pinned hash (the name is reserved, and no key signs this build) and enable.
    /// An admin grant needs a terminal; the test says it has one in the request itself.
    pub fn install(&mut self, port: u16, admin: bool) {
        let pkg = package(port, admin);
        self.fingerprint = pkg.fingerprint.clone();
        let file = self.dir.path().join("ui-web.tar.gz");
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
        self.enable(admin);
        assert!(
            self.wait_adapter_state("ready"),
            "the adapter is not ready: {}\n{}",
            self.adapter_info(),
            self.daemon_stderr()
        );
    }

    fn enable(&self, admin: bool) {
        if admin {
            let answer = self.rpc(json!({ "type": "adapter", "op": "enable", "name": ADAPTER, "accept": self.fingerprint, "interactive": true }));
            assert_ne!(answer["type"], "error", "enable failed: {answer}");
        } else {
            let enabled = self.cli(&["adapter", "enable", ADAPTER, "--accept", &self.fingerprint]);
            assert_eq!(
                enabled.code,
                0,
                "enable failed:\n{}\n{}",
                enabled.all(),
                self.daemon_stderr()
            );
        }
    }

    /// Disable and enable again: the configuration is read when the adapter starts.
    pub fn restart_adapter(&self, admin: bool) {
        let off = self.cli(&["adapter", "disable", ADAPTER]);
        assert_eq!(off.code, 0, "disable failed:\n{}", off.all());
        self.enable(admin);
        assert!(
            self.wait_adapter_state("ready"),
            "the adapter is not ready: {}",
            self.adapter_info()
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
        let run = self.cli(&["--json", "adapter", "info", ADAPTER]);
        serde_json::from_str(&run.stdout).unwrap_or_else(|_| json!({ "unparsable": run.all() }))
    }

    pub fn wait_adapter_state(&self, state: &str) -> bool {
        wait_for(30, || self.adapter_info()["state"] == state)
    }

    pub fn state_dir(&self) -> PathBuf {
        self.home.join("adapters").join("state").join(ADAPTER)
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
        let dir = self.home.join("adapter-logs").join(ADAPTER);
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

    /// Start an app that runs `sh -c script`; `auto_restart` with a short delay when asked.
    pub fn start_app(&self, name: &str, script: &str, auto_restart: bool) -> Value {
        self.start_app_with(name, script, auto_restart, &[], true)
    }

    /// Like `start_app`, with tags, and with or without `ui-web` under `adapters` (the opt-in).
    pub fn start_app_with(
        &self,
        name: &str,
        script: &str,
        auto_restart: bool,
        tags: &[&str],
        opted_in: bool,
    ) -> Value {
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
        spec.tags = tags.iter().map(|t| t.to_string()).collect();
        if opted_in {
            spec.adapters.insert(ADAPTER.to_string(), json!({}));
        }
        self.rpc(json!({ "type": "start", "spec": spec }))
    }

    /// Wait until the app is running with a pid, and return it.
    pub fn wait_running(&self, name: &str) -> u32 {
        let mut pid = 0;
        assert!(
            wait_for(20, || {
                let status = self.rpc(json!({ "type": "status" }));
                pid = status["apps"]
                    .as_array()
                    .and_then(|a| a.iter().find(|x| x["spec"]["name"] == name))
                    .and_then(|x| x["state"]["pid"].as_u64())
                    .unwrap_or(0) as u32;
                pid != 0
            }),
            "{name} did not come up"
        );
        pid
    }

    pub fn stop_all(&self) {
        let _ = self.rpc(json!({ "type": "stop", "selector": { "by": "all" } }));
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

// ---- HTTP and WebSocket -------------------------------------------------------------------------

/// The ports `free_port` has handed out in this test process.
static HANDED_OUT: LazyLock<Mutex<HashSet<u16>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

/// A port nobody listens on now and that no earlier call in this process handed out: the tests
/// run in parallel, and the kernel offers a port it has just seen again now and then. (Another
/// process could take the port before the test does; the window is small.)
pub fn free_port() -> u16 {
    loop {
        let port = {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
            listener.local_addr().unwrap().port()
        };
        if HANDED_OUT.lock().unwrap().insert(port) {
            return port;
        }
    }
}

#[test]
fn a_port_is_not_handed_out_twice() {
    // The kernel offers a port again soon after it was released, so among enough draws one comes
    // back all but surely unless the helper remembers what it handed out.
    let mut ports: Vec<u16> = std::thread::scope(|scope| {
        let draws: Vec<_> = (0..8)
            .map(|_| scope.spawn(|| (0..125).map(|_| free_port()).collect::<Vec<u16>>()))
            .collect();
        draws
            .into_iter()
            .flat_map(|draw| draw.join().unwrap())
            .collect()
    });
    ports.sort_unstable();
    let again = ports.windows(2).filter(|pair| pair[0] == pair[1]).count();
    assert_eq!(again, 0);
}

pub struct Http {
    pub status: u16,
    /// Header names in lower case.
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Http {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or_else(|_| json!({ "unparsable": self.body }))
    }
}

/// One request to `addr` with the given `Host` header (when there is one), extra headers and a body.
pub fn request_with(
    addr: &str,
    method: &str,
    path: &str,
    host: Option<&str>,
    extra: &[(&str, &str)],
    body: &str,
) -> std::io::Result<Http> {
    let mut stream = TcpStream::connect(addr)?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    let mut text = format!("{method} {path} HTTP/1.1\r\n");
    if let Some(host) = host {
        text.push_str(&format!("Host: {host}\r\n"));
    }
    for (k, v) in extra {
        text.push_str(&format!("{k}: {v}\r\n"));
    }
    if !body.is_empty() || method == "POST" {
        text.push_str(&format!(
            "Content-Length: {}\r\nContent-Type: application/json\r\n",
            body.len()
        ));
    }
    text.push_str("Connection: close\r\n\r\n");
    text.push_str(body);
    stream.write_all(text.as_bytes())?;
    let mut raw = Vec::new();
    let _ = stream.read_to_end(&mut raw);
    let raw = String::from_utf8_lossy(&raw).to_string();
    let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((raw.as_str(), ""));
    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_lowercase(), v.trim().to_string()))
        .collect();
    Ok(Http {
        status,
        headers,
        body: body.to_string(),
    })
}

/// A request to the adapter on loopback with the right `Host` header and the test key.
pub fn api(port: u16, method: &str, path: &str, body: &str) -> Http {
    api_with(port, method, path, &[("X-API-Key", KEY)], body)
}

/// Like `api` with the headers of the caller's choice (and the right `Host`).
pub fn api_with(port: u16, method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> Http {
    let addr = format!("127.0.0.1:{port}");
    request_with(&addr, method, path, Some(&addr), headers, body).expect("request")
}

/// Is something accepting connections on loopback at `port`?
pub fn listening(port: u16) -> bool {
    TcpStream::connect(("127.0.0.1", port)).is_ok()
}

/// The local addresses (as hex, `0100007F` is 127.0.0.1) on which this machine listens on `port`.
pub fn listen_addresses(port: u16) -> Vec<String> {
    let mut found = Vec::new();
    for file in ["/proc/net/tcp", "/proc/net/tcp6"] {
        for line in std::fs::read_to_string(file)
            .unwrap_or_default()
            .lines()
            .skip(1)
        {
            let fields: Vec<&str> = line.split_whitespace().collect();
            // fields: sl local_address rem_address st ...; st 0A is LISTEN
            if let (Some(local), Some("0A")) = (fields.get(1), fields.get(3).copied()) {
                if let Some((addr, p)) = local.rsplit_once(':') {
                    if u16::from_str_radix(p, 16) == Ok(port) {
                        found.push(format!("{file}:{addr}"));
                    }
                }
            }
        }
    }
    found
}

/// A WebSocket client on the stream path. `Err` carries the HTTP status of a refused upgrade.
pub struct Ws {
    socket: tungstenite::WebSocket<TcpStream>,
    /// The subprotocol the server selected.
    pub protocol: Option<String>,
}

pub enum WsError {
    Refused(u16),
    Other(String),
}

impl Ws {
    /// Connect to `/ws` with the given extra request headers (`Host` is the right one unless given).
    pub fn connect(port: u16, headers: &[(&str, &str)]) -> Result<Ws, WsError> {
        Ws::connect_path(port, "/ws", headers)
    }

    pub fn connect_path(port: u16, path: &str, headers: &[(&str, &str)]) -> Result<Ws, WsError> {
        use tungstenite::client::IntoClientRequest;
        let addr = format!("127.0.0.1:{port}");
        let mut request = format!("ws://{addr}{path}")
            .into_client_request()
            .map_err(|e| WsError::Other(e.to_string()))?;
        for (k, v) in headers {
            request.headers_mut().insert(
                tungstenite::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                tungstenite::http::HeaderValue::from_str(v).unwrap(),
            );
        }
        let stream = TcpStream::connect(&addr).map_err(|e| WsError::Other(e.to_string()))?;
        stream
            .set_read_timeout(Some(Duration::from_secs(20)))
            .unwrap();
        match tungstenite::client(request, stream) {
            Ok((socket, response)) => {
                let protocol = response
                    .headers()
                    .get("sec-websocket-protocol")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string);
                Ok(Ws { socket, protocol })
            }
            Err(tungstenite::HandshakeError::Failure(tungstenite::Error::Http(response))) => {
                Err(WsError::Refused(response.status().as_u16()))
            }
            Err(e) => Err(WsError::Other(e.to_string())),
        }
    }

    /// The next text message as JSON, within `secs`; `None` when nothing arrives.
    pub fn next_json(&mut self, secs: u64) -> Option<Value> {
        self.socket
            .get_mut()
            .set_read_timeout(Some(Duration::from_secs(secs)))
            .unwrap();
        loop {
            match self.socket.read() {
                Ok(tungstenite::Message::Text(text)) => return serde_json::from_str(&text).ok(),
                Ok(_) => continue,
                Err(_) => return None,
            }
        }
    }

    /// Read until a message satisfies `want`, for at most `secs` seconds overall.
    pub fn wait_for(&mut self, secs: u64, mut want: impl FnMut(&Value) -> bool) -> Option<Value> {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < deadline {
            let left = deadline
                .saturating_duration_since(Instant::now())
                .as_secs()
                .max(1);
            match self.next_json(left) {
                Some(message) if want(&message) => return Some(message),
                Some(_) => continue,
                None => return None,
            }
        }
        None
    }

    pub fn disconnected(&mut self) -> bool {
        use tungstenite::{error::ProtocolError, Error, Message};
        self.socket
            .get_mut()
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        match self.socket.read() {
            Ok(Message::Close(_))
            | Err(
                Error::ConnectionClosed
                | Error::Protocol(ProtocolError::ResetWithoutClosingHandshake),
            ) => true,
            Err(Error::Io(error)) => error.kind() == std::io::ErrorKind::ConnectionReset,
            _ => false,
        }
    }

    pub fn send_text(&mut self, text: &str) {
        let _ = self
            .socket
            .send(tungstenite::Message::Text(text.to_string()));
    }
}
