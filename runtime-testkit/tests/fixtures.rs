use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use oxidepm_runtime_testkit::fixtures::{
    run_hang_and_kill, runs_fixture, unreal_fixture, unreal_samples_dir, FakeHost,
    CONFLICTING_INSTANCE_EXIT_CODE, CONFLICTING_INSTANCE_PREFIX,
};

struct FixtureCase {
    name: &'static str,
    path: std::path::PathBuf,
    default_marker: &'static str,
}

fn all_fixtures() -> Vec<FixtureCase> {
    vec![
        FixtureCase {
            name: "fake-ubt",
            path: unreal_fixture("fake-ubt"),
            default_marker: "Result: Succeeded",
        },
        FixtureCase {
            name: "fake-uat",
            path: unreal_fixture("fake-uat"),
            default_marker: "BUILD SUCCESSFUL",
        },
        FixtureCase {
            name: "fake-editor-cmd",
            path: unreal_fixture("fake-editor-cmd"),
            default_marker: "FIXTURE_MARKER: PASS",
        },
        FixtureCase {
            name: "fake-packaged-game",
            path: unreal_fixture("fake-packaged-game"),
            default_marker: "FIXTURE_MARKER: PASS",
        },
        FixtureCase {
            name: "fake-ssh",
            path: runs_fixture("fake-ssh"),
            default_marker: "REMOTE_OK",
        },
        FixtureCase {
            name: "fake-rsync",
            path: runs_fixture("fake-rsync"),
            default_marker: "RSYNC_OK",
        },
    ]
}

fn run_fixture(path: &Path, args: &[&str], envs: &[(&str, &str)]) -> std::process::Output {
    let mut cmd = Command::new(path);
    cmd.args(args);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    cmd.output()
        .unwrap_or_else(|e| panic!("failed to run {}: {e}", path.display()))
}

#[test]
fn every_fixture_supports_success_nonzero_conflict_hang_and_missing_marker() {
    let custom_hash = "50040620fdce9c2d1f006fcd84378adee89929f8";

    for fixture in all_fixtures() {
        // 1. success
        let out = run_fixture(&fixture.path, &[], &[("FAKE_MODE", "success")]);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(
            out.status.code(),
            Some(0),
            "{} success failed: stdout={stdout} stderr={stderr}",
            fixture.name
        );
        assert!(
            stdout.contains(fixture.default_marker),
            "{} success missing marker {:?}: {stdout}",
            fixture.name,
            fixture.default_marker
        );

        // 2. nonzero
        let out = run_fixture(
            &fixture.path,
            &[],
            &[("FAKE_MODE", "nonzero"), ("FAKE_EXIT_CODE", "7")],
        );
        assert_eq!(
            out.status.code(),
            Some(7),
            "{} nonzero exit mismatch",
            fixture.name
        );

        // 3. ConflictingInstance (exit 10 + exact message format)
        let out = run_fixture(
            &fixture.path,
            &[],
            &[("FAKE_MODE", "conflict"), ("FAKE_MUTEX_HASH", custom_hash)],
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert_eq!(
            out.status.code(),
            Some(CONFLICTING_INSTANCE_EXIT_CODE),
            "{} conflict must exit 10: {stdout}",
            fixture.name
        );
        let expected_msg =
            format!("{CONFLICTING_INSTANCE_PREFIX}{custom_hash} is already running.");
        assert!(
            stdout.contains(&expected_msg),
            "{} conflict missing expected message {:?}: {stdout}",
            fixture.name,
            expected_msg
        );

        // 4. missing-marker (exit 0 without pass marker)
        let out = run_fixture(&fixture.path, &[], &[("FAKE_MODE", "missing-marker")]);
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert_eq!(
            out.status.code(),
            Some(0),
            "{} missing-marker must exit 0: {stdout}",
            fixture.name
        );
        assert!(
            !stdout.contains(fixture.default_marker),
            "{} missing-marker unexpectedly printed {:?}: {stdout}",
            fixture.name,
            fixture.default_marker
        );

        // 5. hang (blocks until killed by the test)
        let mut cmd = Command::new(&fixture.path);
        cmd.env("FAKE_MODE", "hang").env("FAKE_HANG_SECONDS", "30");
        let status = run_hang_and_kill(cmd, Duration::from_secs(5));
        assert!(
            !status.success(),
            "{} hang should terminate non-zero after kill: {status:?}",
            fixture.name
        );
    }
}

#[test]
fn unreal_fixtures_support_abslog_args_log_delay_and_archive_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let log_file = tmp.path().join("logs/uat.log");
    let args_log = tmp.path().join("args.log");
    let archive_dir = tmp.path().join("archive");

    let abslog_arg = format!("-abslog={}", log_file.display());
    let archive_arg = format!("-archivedirectory={}", archive_dir.display());

    let start = Instant::now();
    let out = run_fixture(
        &unreal_fixture("fake-uat"),
        &[&abslog_arg, &archive_arg, "-NoEngineChanges"],
        &[
            ("FAKE_MODE", "success"),
            ("FAKE_DELAY_SECONDS", "0.05"),
            ("FAKE_ARGS_LOG", args_log.to_str().unwrap()),
            ("FAKE_PROJECT", "DemoProject"),
        ],
    );
    let elapsed = start.elapsed();
    assert_eq!(out.status.code(), Some(0));
    assert!(
        elapsed >= Duration::from_millis(40),
        "slow mode finished too quickly: {elapsed:?}"
    );

    let mirrored = fs::read_to_string(&log_file).unwrap();
    assert!(mirrored.contains("BUILD SUCCESSFUL"), "{mirrored}");
    assert!(mirrored.contains("DemoProject"), "{mirrored}");

    let recorded_args = fs::read_to_string(&args_log).unwrap();
    assert!(
        recorded_args.contains("-NoEngineChanges"),
        "{recorded_args}"
    );

    let pak = archive_dir.join("Linux/DemoProject/Content/Paks/DemoProject-Linux.pak");
    assert!(pak.is_file(), "expected fake pak at {}", pak.display());
}

#[test]
fn fake_host_runs_ssh_and_rsync_against_local_target_directory() {
    let host = FakeHost::new();
    let src = tempfile::tempdir().unwrap();
    fs::write(src.path().join("hello.txt"), "payload-v1\n").unwrap();

    // Sync files to the fake host using `rsync` from FakeHost's PATH.
    let mut rsync = Command::new(host.bin_dir.join("rsync"));
    host.configure_command(&mut rsync);
    let src_arg = format!("{}/", src.path().display());
    let out = rsync
        .args(["-az", &src_arg, "test-host:/srv/app/"])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "rsync failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        fs::read_to_string(host.root.join("srv/app/hello.txt")).unwrap(),
        "payload-v1\n"
    );

    // Run a remote command through `ssh` inside the fake host root.
    let mut ssh = Command::new(host.bin_dir.join("ssh"));
    host.configure_command(&mut ssh);
    let out = ssh
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=10",
            "--",
            "test-host",
            "cat srv/app/hello.txt",
        ])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "ssh failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "payload-v1\n");

    assert_eq!(host.rsync_calls().len(), 1);
    assert_eq!(host.ssh_calls().len(), 1);
    assert!(host.ssh_calls()[0].contains("test-host"));

    // Verify path traversal is rejected by fake-rsync.
    let mut bad_rsync = Command::new(host.bin_dir.join("rsync"));
    host.configure_command(&mut bad_rsync);
    let out = bad_rsync
        .args(["-a", &src_arg, "test-host:/srv/../escape/"])
        .output()
        .unwrap();
    assert_ne!(out.status.code(), Some(0));
}

#[test]
fn scrubbed_unreal_log_samples_exist_and_contain_no_private_paths() {
    let dir = unreal_samples_dir();
    let mut count = 0;
    for entry in fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("log") {
            continue;
        }
        count += 1;
        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.is_empty(), "{} is empty", path.display());
        for forbidden in ["/home/", "/Users/", "/mnt/", "/root/", "C:\\Users\\"] {
            assert!(
                !text.contains(forbidden),
                "{} contains host-specific path prefix {forbidden:?}",
                path.display()
            );
        }
    }
    assert!(
        count >= 8,
        "expected at least 8 scrubbed log samples, found {count}"
    );
}
