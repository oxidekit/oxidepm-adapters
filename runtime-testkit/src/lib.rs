//! What the integration tests of the runtime adapters share: the real `oxidepmd` and `oxidepm` of
//! the core repository (a branch checked out next to this one, built once per test run), a
//! private home for each test, and adapters imported from an installed release bundle or by
//! their pinned hashes.

use std::fs::{self, DirBuilder, Permissions};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

pub mod fixtures;

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

pub struct CoreBins {
    pub daemon: PathBuf,
    pub cli: PathBuf,
}

/// Where the core repository is: `OXIDEPM_CORE_DIR`, else the sibling directory `oxidepm`.
pub fn core_dir() -> PathBuf {
    std::env::var_os("OXIDEPM_CORE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../oxidepm"))
}

/// The daemon, the CLI and the sandbox launcher, built (a no-op when current) into
/// `OXIDEPM_CORE_TARGET_DIR`, else the core repository's own target directory.
pub fn core_bins() -> &'static CoreBins {
    static BINS: OnceLock<CoreBins> = OnceLock::new();
    BINS.get_or_init(|| {
        let core = core_dir();
        let target = std::env::var_os("OXIDEPM_CORE_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| core.join("target"));
        let output = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
            .args([
                "build",
                "-p",
                "oxidepmd",
                "-p",
                "oxidepm",
                "-p",
                "oxidepm-sandbox",
                "--offline",
                "--locked",
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

/// An adapter under test: its crate directory (the `manifest.json` is there) and its built binary.
pub struct Adapter {
    pub name: String,
    pub manifest: String,
    pub exec_name: String,
    pub exe: Vec<u8>,
    manifest_dir: PathBuf,
    exe_path: PathBuf,
}

impl Adapter {
    /// `manifest_dir` is `env!("CARGO_MANIFEST_DIR")` of the adapter's crate and `exe` is
    /// `env!("CARGO_BIN_EXE_<binary>")`.
    pub fn new(manifest_dir: &str, exe: &str) -> Adapter {
        let manifest = std::fs::read_to_string(Path::new(manifest_dir).join("manifest.json"))
            .expect("read manifest.json");
        let parsed: Value = serde_json::from_str(&manifest).expect("manifest.json is JSON");
        Adapter {
            name: parsed["name"].as_str().expect("a name").to_string(),
            exec_name: parsed["exec"].as_str().expect("an exec").to_string(),
            manifest,
            exe: std::fs::read(exe).expect("read the adapter binary"),
            manifest_dir: PathBuf::from(manifest_dir),
            exe_path: PathBuf::from(exe),
        }
    }

    pub fn package(&self) -> Package {
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::fast(),
        ));
        for (name, data, mode) in [
            ("manifest.json", self.manifest.as_bytes(), 0o644),
            (self.exec_name.as_str(), self.exe.as_slice(), 0o755),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(mode);
            header.set_entry_type(tar::EntryType::Regular);
            builder.append_data(&mut header, name, data).unwrap();
        }
        let archive = builder.into_inner().unwrap().finish().unwrap();
        let fingerprint = oxidepm_adapter_proto::Manifest::parse(self.manifest.as_bytes())
            .expect("the manifest is valid")
            .fingerprint(&hex(&self.exe));
        Package {
            archive_sha: hex(&archive),
            archive,
            fingerprint,
        }
    }
}

pub struct Package {
    pub archive: Vec<u8>,
    pub archive_sha: String,
    pub fingerprint: String,
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

    fn missing_enforcement(&self, adapter: &str) -> bool {
        if self.code == 0 {
            return false;
        }
        let error = self.stderr.trim();
        let refusal =
            format!("Error: E_ADAPTER_INVALID: {adapter} requires the sandbox to enforce ");
        let selftest = format!(
            "Error: E_ADAPTER_UNAVAILABLE: the sandbox self-test of '{adapter}' failed: oxidepm-sandbox: this system cannot enforce "
        );
        (error.starts_with(&refusal)
            && error.contains(", which this machine cannot (")
            && error.ends_with(". If you accept that, pass --allow-unenforced"))
            || (error.starts_with(&selftest)
                && error.ends_with("); the adapter requires it. Enable it with --allow-unenforced to run it anyway"))
    }
}

pub struct Env {
    pub dir: tempfile::TempDir,
    pub home: PathBuf,
    adapter_name: String,
    daemon: Option<Child>,
    bundle: Option<Bundle>,
}

struct Bundle {
    dir: PathBuf,
    bins: CoreBins,
    single_adapter: Option<String>,
    _scratch: tempfile::TempDir,
}

fn private_tempdir() -> tempfile::TempDir {
    let dir = tempfile::Builder::new()
        .permissions(Permissions::from_mode(0o700))
        .tempdir()
        .expect("create a private tempdir");
    // Creation can lose owner bits to the umask or an inherited ACL.
    fs::set_permissions(dir.path(), Permissions::from_mode(0o700))
        .expect("set tempdir permissions");
    dir
}

fn create_home(dir: &Path) -> PathBuf {
    let home = dir.join("home");
    DirBuilder::new()
        .mode(0o700)
        .create(&home)
        .expect("create a private home");
    fs::set_permissions(&home, Permissions::from_mode(0o700)).expect("set home permissions");
    home
}

fn core_command(binary: &Path) -> Command {
    let mut command = Command::new(binary);
    command
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default());
    if let Some(abi) = std::env::var_os("OXIDEPM_TEST_LANDLOCK_ABI") {
        command.env("OXIDEPM_TEST_LANDLOCK_ABI", abi);
    }
    command
}

impl Env {
    /// A daemon in a new home with `adapter` imported and enabled.
    pub fn new(adapter: &Adapter) -> Env {
        let mut env = Env::bare(&[]);
        env.install(adapter);
        env
    }

    /// A daemon in a new home, nothing installed. `daemon_env` is added to the daemon's
    /// environment, after its `PATH`, so it can replace that.
    pub fn bare(daemon_env: &[(&str, &str)]) -> Env {
        Env::start_daemon(daemon_env, None)
    }

    /// A daemon in a new home with a release bundle installed alongside the core binaries.
    pub fn with_bundle(adapters: &[&Adapter], daemon_env: &[(&str, &str)]) -> Env {
        let scratch = private_tempdir();
        let dir = scratch.path().join("share/oxidepm/adapters");
        let inputs: Vec<oxidepm_adapter_bundle::Input> = adapters
            .iter()
            .map(|a| oxidepm_adapter_bundle::Input {
                manifest_dir: a.manifest_dir.clone(),
                exe: a.exe_path.clone(),
            })
            .collect();
        oxidepm_adapter_bundle::build(
            &dir,
            &inputs,
            &oxidepm_adapter_bundle::Options {
                target: oxidepm_adapter_proto::host_target(),
                url_base: "https://adapters.example.org/bundle".to_string(),
                sequence: 1,
                generated: chrono::Utc::now(),
                valid_for: chrono::Duration::days(30),
            },
        )
        .expect("make the bundle");

        let bin = scratch.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let core = core_bins();
        for (source, name) in [
            (core.cli.clone(), "oxidepm"),
            (core.daemon.clone(), "oxidepmd"),
            (
                core.daemon.with_file_name("oxidepm-sandbox"),
                "oxidepm-sandbox",
            ),
        ] {
            let dest = bin.join(name);
            if std::fs::hard_link(&source, &dest).is_err() {
                std::fs::copy(&source, &dest).expect("install the core binary");
            }
        }
        Env::start_daemon(
            daemon_env,
            Some(Bundle {
                dir,
                bins: CoreBins {
                    daemon: bin.join("oxidepmd"),
                    cli: bin.join("oxidepm"),
                },
                single_adapter: match adapters {
                    [adapter] => Some(adapter.name.clone()),
                    _ => None,
                },
                _scratch: scratch,
            }),
        )
    }

    fn start_daemon(daemon_env: &[(&str, &str)], bundle: Option<Bundle>) -> Env {
        let bins = match &bundle {
            Some(bundle) => &bundle.bins,
            None => core_bins(),
        };
        let dir = private_tempdir();
        let home = create_home(dir.path());
        let stderr = std::fs::File::create(home.join("daemon-test.stderr")).unwrap();
        let mut command = core_command(&bins.daemon);
        command
            .envs(daemon_env.iter().copied())
            .env("OXIDEPM_HOME", &home)
            .env("HOME", &home)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr);
        let child = command.spawn().expect("spawn oxidepmd");
        std::fs::create_dir_all(dir.path().join("project")).unwrap();
        let env = Env {
            dir,
            home,
            adapter_name: String::new(),
            daemon: Some(child),
            bundle,
        };
        let socket = env.home.join("daemon.sock");
        assert!(
            wait_for(60, || UnixStream::connect(&socket).is_ok()),
            "the daemon did not come up: {}",
            env.daemon_stderr()
        );
        env
    }

    /// The directory of the release bundle, for an environment made with `with_bundle`.
    pub fn bundle_dir(&self) -> Option<&Path> {
        self.bundle.as_ref().map(|b| b.dir.as_path())
    }

    /// The pid of the daemon.
    pub fn daemon_pid(&self) -> u32 {
        self.daemon.as_ref().map(|c| c.id()).unwrap_or(0)
    }

    /// A working directory for the CLI and the apps.
    pub fn project(&self) -> PathBuf {
        std::fs::canonicalize(self.dir.path().join("project")).unwrap()
    }

    pub fn daemon_stderr(&self) -> String {
        std::fs::read_to_string(self.home.join("daemon-test.stderr")).unwrap_or_default()
    }

    /// Import by the pinned hash (the name is reserved, and no key signs this build) and enable.
    pub fn install(&mut self, adapter: &Adapter) {
        let pkg = adapter.package();
        let file = self.dir.path().join(format!("{}.tar.gz", adapter.name));
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
        self.adapter_name = adapter.name.clone();
        self.enable(&pkg.fingerprint);
    }

    /// Import a bundled adapter with broader permissions and accept its grant.
    pub fn install_bundled(&mut self, name: &str) {
        let import = self.cli(&["adapter", "import", "--bundled", name, "--enable"]);
        assert_eq!(import.code, 2, "{}", import.all());
        assert!(
            import.stderr.contains("needs your consent"),
            "{}",
            import.all()
        );
        assert!(
            import.stdout.contains("bundled with this release"),
            "{}",
            import.all()
        );
        self.adapter_name = name.to_string();
        let info = self.adapter_info();
        assert_eq!(info["trust"], "bundled", "{info}");
        assert_eq!(info["enabled"], false, "{info}");
        let fingerprint = info["fingerprint"].as_str().expect("a fingerprint");
        assert!(import.stderr.contains(fingerprint), "{}", import.all());
        self.enable(fingerprint);
    }

    fn enable(&self, fingerprint: &str) {
        let enabled = self.cli(&[
            "adapter",
            "enable",
            &self.adapter_name,
            "--accept",
            fingerprint,
        ]);
        assert_eq!(
            enabled.code,
            0,
            "enable failed:\n{}\n{}",
            enabled.all(),
            self.daemon_stderr()
        );
        // a tool runs once at `enable` to prove that it answers, and then stops until it is needed
        let up = |info: &Value| {
            info["enabled"] == true && matches!(info["state"].as_str(), Some("ready" | "stopped"))
        };
        assert!(
            wait_for(30, || up(&self.adapter_info())),
            "the adapter is not usable: {}\n{}",
            self.adapter_info(),
            self.daemon_stderr()
        );
    }

    /// Run a test command, retrying an enable if a required sandbox feature is unavailable.
    pub fn cli(&self, args: &[&str]) -> Run {
        let attempt = self.cli_raw(args);
        let (name, consent) = match args {
            ["adapter", "enable", name, options @ ..]
                if !options.contains(&"--allow-unenforced") =>
            {
                (*name, options)
            }
            ["adapter", "import", options @ ..]
                if ["--bundled", "--enable", "--yes"]
                    .iter()
                    .all(|flag| options.contains(flag)) =>
            {
                // The import already installed this adapter. With a larger bundle it could
                // have stopped partway through, so only retry for a single-adapter fixture.
                let Some(name) = self
                    .bundle
                    .as_ref()
                    .and_then(|b| b.single_adapter.as_deref())
                else {
                    return attempt;
                };
                (name, ["--yes"].as_slice())
            }
            _ => return attempt,
        };
        if !attempt.missing_enforcement(name) {
            return attempt;
        }

        let mut args = vec!["adapter", "enable", name, "--allow-unenforced"];
        args.extend_from_slice(consent);
        let mut enabled = self.cli_raw(&args);
        if enabled.code == 0 {
            eprintln!(
                "Running adapter {name} with unenforced sandbox requirements (--allow-unenforced)."
            );
        }
        enabled.stdout.insert_str(0, &attempt.stdout);
        enabled
    }

    /// Run the CLI once. Use this when asserting the CLI's response to an enable request.
    pub fn cli_raw(&self, args: &[&str]) -> Run {
        let bins = match &self.bundle {
            Some(bundle) => &bundle.bins,
            None => core_bins(),
        };
        let mut command = core_command(&bins.cli);
        command
            .args(args)
            .env("HOME", &self.home)
            .env("OXIDEPM_HOME", &self.home)
            .current_dir(self.project())
            .stdin(Stdio::null());
        let out = command.output().expect("run the CLI");
        Run {
            code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).to_string(),
            stderr: String::from_utf8_lossy(&out.stderr).to_string(),
        }
    }

    pub fn adapter_info(&self) -> Value {
        let run = self.cli(&["--json", "adapter", "info", &self.adapter_name]);
        serde_json::from_str(&run.stdout).unwrap_or_else(|_| json!({ "unparsable": run.all() }))
    }

    /// One request on the main socket as a person.
    pub fn rpc(&self, request: Value) -> Value {
        self.try_rpc(request).expect("a daemon reply")
    }

    fn try_rpc(&self, request: Value) -> std::io::Result<Value> {
        let mut stream = UnixStream::connect(self.home.join("daemon.sock"))?;
        stream.set_read_timeout(Some(Duration::from_secs(120)))?;
        let envelope = json!({ "protocol": 2, "request_id": "t-1", "request": request });
        writeln!(stream, "{envelope}")?;
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line)?;
        let reply: Value = serde_json::from_str(&line)?;
        Ok(reply["response"].clone())
    }

    /// Start `command` with the runtime `runtime` in `cwd`, not restarting it when it dies, and
    /// return the daemon's answer.
    pub fn start(
        &self,
        name: &str,
        runtime: &str,
        command: &str,
        args: &[&str],
        cwd: &Path,
    ) -> Value {
        self.start_with(name, runtime, command, args, cwd, &[])
    }

    /// Like `start`, with variables in the app's own `env`.
    pub fn start_with(
        &self,
        name: &str,
        runtime: &str,
        command: &str,
        args: &[&str],
        cwd: &Path,
        env: &[(&str, &str)],
    ) -> Value {
        let runtime: oxidepm_core::Runtime = runtime.parse().expect("a runtime name");
        let mut spec =
            oxidepm_core::AppSpec::new(name.into(), runtime, command.into(), cwd.to_path_buf());
        spec.args = args.iter().map(|a| a.to_string()).collect();
        spec.restart_policy.auto_restart = false;
        for (k, v) in env {
            spec.env.insert(k.to_string(), v.to_string());
        }
        self.rpc(json!({ "type": "start", "spec": spec }))
    }

    /// The status entry of the app, when it exists.
    pub fn app(&self, name: &str) -> Option<Value> {
        let status = self.rpc(json!({ "type": "status" }));
        status["apps"]
            .as_array()?
            .iter()
            .find(|x| x["spec"]["name"] == name)
            .cloned()
    }

    /// Wait until the app is running with a pid, and return it.
    pub fn wait_running(&self, name: &str) -> u32 {
        let mut pid = 0;
        assert!(
            wait_for(60, || {
                pid = self
                    .app(name)
                    .and_then(|x| x["state"]["pid"].as_u64())
                    .unwrap_or(0) as u32;
                pid != 0
            }),
            "{name} did not come up: {:?}\n{}",
            self.app(name),
            self.daemon_stderr()
        );
        pid
    }

    /// Wait for the app's state to be `state` (its status string).
    pub fn wait_state(&self, name: &str, state: &str) -> bool {
        wait_for(60, || {
            self.app(name)
                .is_some_and(|x| x["state"]["status"] == state)
        })
    }

    pub fn app_log(&self, name: &str, stream: &str) -> String {
        std::fs::read_to_string(self.home.join("logs").join(format!("{name}-{stream}.log")))
            .unwrap_or_default()
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
            .join(&self.adapter_name);
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

#[cfg(test)]
#[test]
fn creates_a_private_home() {
    let dir = private_tempdir();
    let home = create_home(dir.path());
    for path in [dir.path(), &home] {
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
}

#[cfg(test)]
#[test]
fn retries_only_when_required_sandbox_enforcement_is_unavailable() {
    let refusal = "Error: E_ADAPTER_INVALID: init requires the sandbox to enforce network, which this machine cannot (landlock abi 8, seccomp). The kernel cannot restrict connections to pathname Unix sockets. If you accept that, pass --allow-unenforced\n";
    let selftest = "Error: E_ADAPTER_UNAVAILABLE: the sandbox self-test of 'init' failed: oxidepm-sandbox: this system cannot enforce fs_write (landlock: no, seccomp: yes); the adapter requires it. Enable it with --allow-unenforced to run it anyway\n";
    for (code, stderr, expected) in [
        (1, refusal, true),
        (1, selftest, true),
        (0, refusal, false),
        (1, "Error: E_ADAPTER_INVALID: Enabling init requires explicit consent. Pass --accept fingerprint or --allow-unenforced", false),
        (1, "Error: E_ADAPTER_UNAVAILABLE: the sandbox self-test of 'init' failed: the launcher timed out", false),
    ] {
        let run = Run {
            code,
            stdout: String::new(),
            stderr: stderr.to_string(),
        };
        assert_eq!(run.missing_enforcement("init"), expected, "{stderr}");
        assert!(!run.missing_enforcement("watch"), "{stderr}");
    }
}

#[cfg(test)]
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
        adapter_name: String::new(),
        bundle: None,
    };

    drop(env);
}
