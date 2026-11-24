//! The bundle of adapters that a release of OxidePM carries under `share/oxidepm/adapters`: one
//! archive per adapter, an `index.json` that describes them, and their `SHA256SUMS`.
//! `oxidepm adapter import --bundled` checks the index and archives against the checksum list.
//! [`build`] also supplies the trusted comment for signing the index for online publication.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use oxidepm_adapter_proto::{trusted_comment_for, Index, Manifest, ValidateCtx, INDEX_MAX_BYTES};
use oxidepm_fetch::{extract_tar_gz, ExtractLimits};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

const CHECKSUMS: &str = "SHA256SUMS";

/// One adapter to put in the bundle: its crate directory (the manifest is there) and its built
/// executable.
#[derive(Debug, Clone)]
pub struct Input {
    pub manifest_dir: PathBuf,
    pub exe: PathBuf,
}

#[derive(Debug, Clone)]
pub struct Options {
    /// The target triple the executables were built for, as the index names it.
    pub target: String,
    /// Where the archives will be published (https). The bundle itself is read by file name, so
    /// this only has to be a valid address.
    pub url_base: String,
    /// The sequence number of the index; it must be higher than that of the last one published.
    pub sequence: u64,
    pub generated: DateTime<Utc>,
    pub valid_for: Duration,
}

/// What [`build`] wrote.
#[derive(Debug)]
pub struct Built {
    pub index_path: PathBuf,
    /// The trusted comment the signature of `index.json` has to carry.
    pub trusted_comment: String,
    pub archives: Vec<PathBuf>,
}

fn hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// An archive of the manifest and the executable, the same bytes every time for the same inputs.
fn archive(manifest: &[u8], exec_name: &str, exe: &[u8]) -> Result<Vec<u8>, String> {
    let gz = flate2::GzBuilder::new()
        .mtime(0)
        .write(Vec::new(), flate2::Compression::best());
    let mut builder = tar::Builder::new(gz);
    for (name, data, mode) in [("manifest.json", manifest, 0o644), (exec_name, exe, 0o755)] {
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(mode);
        header.set_mtime(0);
        header.set_uid(0);
        header.set_gid(0);
        header.set_entry_type(tar::EntryType::Regular);
        builder
            .append_data(&mut header, name, data)
            .map_err(|e| format!("cannot write the archive: {e}"))?;
    }
    builder
        .into_inner()
        .and_then(|gz| gz.finish())
        .map_err(|e| format!("cannot write the archive: {e}"))
}

fn check_executable(
    manifest: &Manifest,
    root: &Path,
    bytes: &[u8],
    target: &str,
) -> Result<(), String> {
    let darwin = target.ends_with("-darwin") || target.ends_with("-macos");
    if darwin == cfg!(target_os = "macos") {
        return manifest.check_exec_file(root).map_err(|e| e.detail);
    }

    // The importer's executable check uses the OS it is running on. A cross-build
    // must instead use the executable format of the target.
    let native = if darwin {
        matches!(
            bytes.get(..4),
            Some(
                [0xfe, 0xed, 0xfa, 0xce]
                    | [0xfe, 0xed, 0xfa, 0xcf]
                    | [0xce, 0xfa, 0xed, 0xfe]
                    | [0xcf, 0xfa, 0xed, 0xfe]
                    | [0xca, 0xfe, 0xba, 0xbe]
            )
        )
    } else {
        bytes.starts_with(b"\x7fELF")
    };
    if native {
        Ok(())
    } else {
        Err(format!(
            "exec {:?} is not a native executable for {target}",
            manifest.exec
        ))
    }
}

/// Check that replacing the output directory will only remove a previous bundle.
fn check_previous_bundle(out: &Path) -> Result<(), String> {
    let path = out.join("index.json");
    let mut files = fs::read_dir(out)
        .map_err(|e| format!("cannot list {}: {e}", out.display()))?
        .peekable();
    if files.peek().is_none() {
        return Ok(());
    }
    let meta = fs::symlink_metadata(&path)
        .map_err(|e| format!("cannot replace {}: {e}", out.display()))?;
    if !meta.is_file() || meta.len() > INDEX_MAX_BYTES as u64 {
        return Err(format!(
            "cannot replace {}: index.json is not a regular index file",
            out.display()
        ));
    }
    let bytes = fs::read(&path).map_err(|e| format!("cannot replace {}: {e}", out.display()))?;
    let index = Index::parse(&bytes)
        .map_err(|e| format!("cannot replace {}: {}", out.display(), e.detail))?;
    let mut names = std::collections::HashSet::from([
        "index.json".to_string(),
        "index.json.minisig".to_string(),
        CHECKSUMS.to_string(),
    ]);
    for adapter in index.adapters {
        for release in adapter.releases {
            names.insert(format!("{}-{}.tar.gz", adapter.name, release.version));
        }
    }
    for file in files {
        let file = file.map_err(|e| format!("cannot list {}: {e}", out.display()))?;
        let kind = file
            .file_type()
            .map_err(|e| format!("cannot inspect {}: {e}", file.path().display()))?;
        if !kind.is_file()
            || !file
                .file_name()
                .to_str()
                .is_some_and(|name| names.contains(name))
        {
            return Err(format!(
                "cannot replace {}: it contains an unexpected entry {}",
                out.display(),
                file.path().display()
            ));
        }
    }
    Ok(())
}

fn publish(staged: &Path, out: &Path) -> Result<(), String> {
    let error = |e: io::Error| format!("cannot publish {}: {e}", out.display());
    match fs::symlink_metadata(out) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => fs::rename(staged, out).map_err(error),
        Err(e) => Err(error(e)),
        Ok(meta) if !meta.is_dir() => Err(format!("{} is not a bundle directory", out.display())),
        Ok(_) => {
            check_previous_bundle(out)?;
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            {
                use rustix::fs::{renameat_with, RenameFlags, CWD};
                renameat_with(CWD, staged, CWD, out, RenameFlags::EXCHANGE)
                    .map_err(|e| error(e.into()))
            }
            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
            fs::rename(staged, out).map_err(error)
        }
    }
}

/// Write the archives, `index.json` and `SHA256SUMS` into a new, empty or previously built bundle directory.
pub fn build(out: &Path, inputs: &[Input], opts: &Options) -> Result<Built, String> {
    if !opts.url_base.starts_with("https://") {
        return Err(format!(
            "the base address {:?} is not an https address",
            opts.url_base
        ));
    }
    if inputs.is_empty() {
        return Err("no adapter given".to_string());
    }
    let name = out
        .file_name()
        .ok_or_else(|| format!("{} must name a bundle directory", out.display()))?;
    let parent = out
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let out = &parent.join(name);
    fs::create_dir_all(parent).map_err(|e| format!("cannot make {}: {e}", parent.display()))?;
    let work =
        tempfile::tempdir_in(parent).map_err(|e| format!("cannot stage {}: {e}", out.display()))?;
    let staged = work.path().join("bundle");
    fs::create_dir(&staged).map_err(|e| format!("cannot stage {}: {e}", out.display()))?;
    let mut adapters: Vec<(String, Value)> = Vec::new();
    let mut archives = Vec::new();
    let mut archive_checksums = Vec::new();
    for input in inputs {
        let manifest_path = input.manifest_dir.join("manifest.json");
        let manifest_bytes = std::fs::read(&manifest_path)
            .map_err(|e| format!("cannot read {}: {e}", manifest_path.display()))?;
        let manifest = Manifest::parse(&manifest_bytes)
            .map_err(|e| format!("{}: {}", manifest_path.display(), e.detail))?;
        // Validate the declared minimum version and the current protocol. Version eligibility
        // for a particular daemon is checked when it reads the signed index.
        let version = semver::Version::parse(&manifest.min_daemon)
            .map_err(|e| format!("{}: invalid min_daemon: {e}", manifest_path.display()))?;
        let mut ctx = ValidateCtx::new(version);
        ctx.signed_by_embedded_key = true;
        manifest
            .validate(&ctx)
            .map_err(|e| format!("{}: {}", manifest_path.display(), e.detail))?;
        let limits = ExtractLimits::default();
        let exe = std::fs::read(&input.exe)
            .map_err(|e| format!("cannot read {}: {e}", input.exe.display()))?;
        if exe.len() as u64 > limits.max_file {
            return Err(format!(
                "{} exceeds the archive's file size limit",
                input.exe.display()
            ));
        }
        let archive = archive(&manifest_bytes, &manifest.exec, &exe)?;
        let file = format!("{}-{}.tar.gz", manifest.name, manifest.version);
        let path = staged.join(&file);
        fs::write(&path, &archive).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
        let unpacked =
            tempfile::tempdir_in(work.path()).map_err(|e| format!("cannot check {file}: {e}"))?;
        extract_tar_gz(&path, unpacked.path(), &limits).map_err(|e| format!("{file}: {e}"))?;
        check_executable(&manifest, unpacked.path(), &exe, &opts.target)
            .map_err(|e| format!("{}: {e}", input.exe.display()))?;
        archives.push(out.join(&file));
        let archive_sha256 = hex(&archive);
        let raw: Value = serde_json::from_slice(&manifest_bytes).map_err(|e| e.to_string())?;
        let text = |key: &str| raw[key].as_str().unwrap_or_default().to_string();
        adapters.push((
            manifest.name.clone(),
            json!({
                "name": manifest.name,
                "kind": raw["kind"],
                "tags": raw["tags"],
                "summary": text("summary"),
                "description": text("description"),
                "releases": [{
                    "version": manifest.version.to_string(),
                    "released": opts.generated.format("%Y-%m-%d").to_string(),
                    "min_daemon": text("min_daemon"),
                    "protocol": raw["protocol"],
                    "yanked": false,
                    "artifacts": {
                        opts.target.clone(): {
                            "url": format!("{}/{}", opts.url_base.trim_end_matches('/'), file),
                            "size": archive.len(),
                            "sha256": archive_sha256,
                            "exe_sha256": hex(&exe),
                            "manifest_sha256": hex(&manifest_bytes),
                        }
                    }
                }]
            }),
        ));
        archive_checksums.push((file, archive_sha256));
    }
    adapters.sort_by(|a, b| a.0.cmp(&b.0));
    let doc = json!({
        "schema": 1,
        "sequence": opts.sequence,
        "generated": opts.generated.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        "expires": (opts.generated + opts.valid_for).format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        "revoked": [],
        "adapters": adapters.into_iter().map(|(_, v)| v).collect::<Vec<_>>(),
    });
    let bytes = serde_json::to_vec_pretty(&doc).map_err(|e| e.to_string())?;
    // what the CLI will make of it: refuse here what it would refuse there
    let index =
        Index::parse(&bytes).map_err(|e| format!("the index is not valid: {}", e.detail))?;
    fs::write(staged.join("index.json"), &bytes)
        .map_err(|e| format!("cannot stage index.json: {e}"))?;
    archive_checksums.sort_unstable();
    let checksums: String = std::iter::once(("index.json".to_string(), hex(&bytes)))
        .chain(archive_checksums)
        .map(|(name, digest)| format!("{digest}  {name}\n"))
        .collect();
    fs::write(staged.join(CHECKSUMS), checksums)
        .map_err(|e| format!("cannot stage {CHECKSUMS}: {e}"))?;
    publish(&staged, out)?;
    Ok(Built {
        index_path: out.join("index.json"),
        trusted_comment: trusted_comment_for(&index),
        archives,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxidepm_adapter_proto::signing::testkeys::TestKey;
    use oxidepm_adapter_proto::{verify_index, IndexKey};
    use std::io::Read;

    fn adapter_dir(root: &Path, name: &str, version: &str, tags: &str) -> Input {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let manifest = json!({
            "schema": 1, "name": name, "version": version, "kind": "tool",
            "summary": format!("The {name} adapter."), "description": "Made for the bundle tests.",
            "tags": serde_json::from_str::<Value>(tags).unwrap(), "protocol": [1], "min_daemon": "0.1.0",
            "exec": format!("oxidepm-{name}"),
            "provides": {"runtimes": ["node"], "programs": ["node"]},
            "permissions": {
                "host": {"level": "read", "scope": "all"}, "events": [], "env_pass": [],
                "network": {"mode": "none"}, "fs": {"read": [], "write": []}, "spawn": {"mode": "none"},
                "memory_mb": 64, "requires_enforcement": []
            }
        });
        std::fs::write(dir.join("manifest.json"), manifest.to_string()).unwrap();
        let exe = root.join(format!("exe-{name}"));
        std::fs::write(&exe, format!("\x7fELF pretend executable of {name}")).unwrap();
        Input {
            manifest_dir: dir,
            exe,
        }
    }

    fn options() -> Options {
        Options {
            target: "x86_64-unknown-linux-gnu".into(),
            url_base: "https://example.org/releases".into(),
            sequence: 7,
            generated: "2026-11-01T10:00:00Z".parse().unwrap(),
            valid_for: Duration::days(90),
        }
    }

    #[test]
    fn the_index_describes_the_archives_that_were_written() {
        let tmp = tempfile::tempdir().unwrap();
        let inputs = [
            adapter_dir(tmp.path(), "zeta", "1.2.3", r#"["runtime"]"#),
            adapter_dir(tmp.path(), "alpha", "0.1.0", r#"["ui"]"#),
        ];
        let out = tmp.path().join("bundle");
        let built = build(&out, &inputs, &options()).unwrap();
        let bytes = std::fs::read(&built.index_path).unwrap();
        let index = Index::parse(&bytes).unwrap();
        assert_eq!((index.sequence, index.adapters.len()), (7, 2));
        assert_eq!(
            index
                .adapters
                .iter()
                .map(|a| a.name.as_str())
                .collect::<Vec<_>>(),
            ["alpha", "zeta"],
            "sorted by name"
        );
        assert_eq!(index.adapters[1].tags, ["runtime"]);
        assert_eq!(
            built.trusted_comment,
            "index sequence=7 generated=2026-11-01T10:00:00Z"
        );
        assert_eq!(
            index.expires.to_rfc3339(),
            "2027-01-30T10:00:00+00:00",
            "90 days after it was generated"
        );
        let mut checksums = format!("{}  index.json\n", hex(&bytes));
        for adapter in &index.adapters {
            let release = &adapter.releases[0];
            let artifact = &release.artifacts["x86_64-unknown-linux-gnu"];
            let file = artifact.url.rsplit('/').next().unwrap();
            assert_eq!(file, format!("{}-{}.tar.gz", adapter.name, release.version));
            let archive = std::fs::read(out.join(file)).unwrap();
            assert_eq!(artifact.size as usize, archive.len());
            assert_eq!(artifact.sha256, hex(&archive));
            checksums.push_str(&format!("{}  {file}\n", artifact.sha256));
            // The index binds both files in the archive to their digests.
            let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(&archive[..]));
            let mut seen = Vec::new();
            for entry in tar.entries().unwrap() {
                let mut entry = entry.unwrap();
                let name = entry.path().unwrap().to_string_lossy().to_string();
                let mut data = Vec::new();
                entry.read_to_end(&mut data).unwrap();
                if name == "manifest.json" {
                    assert_eq!(artifact.manifest_sha256, Some(hex(&data)));
                } else {
                    assert_eq!(artifact.exe_sha256, hex(&data));
                    assert_eq!(
                        entry.header().mode().unwrap() & 0o111,
                        0o111,
                        "the executable is executable"
                    );
                }
                seen.push(name);
            }
            assert_eq!(
                seen,
                [
                    "manifest.json".to_string(),
                    format!("oxidepm-{}", adapter.name)
                ]
            );
        }
        assert_eq!(fs::read_to_string(out.join(CHECKSUMS)).unwrap(), checksums);
    }

    #[test]
    fn the_same_inputs_give_the_same_bytes_and_a_key_can_sign_the_index() {
        let tmp = tempfile::tempdir().unwrap();
        let inputs = [adapter_dir(tmp.path(), "alpha", "0.1.0", r#"["runtime"]"#)];
        let first = build(&tmp.path().join("one"), &inputs, &options()).unwrap();
        let second = build(&tmp.path().join("two"), &inputs, &options()).unwrap();
        assert_eq!(
            std::fs::read(&first.index_path).unwrap(),
            std::fs::read(&second.index_path).unwrap()
        );
        assert_eq!(
            std::fs::read(&first.archives[0]).unwrap(),
            std::fs::read(&second.archives[0]).unwrap(),
            "reproducible archives"
        );
        // the index is what a signature with the stated trusted comment covers
        let key = TestKey::generate();
        let body = std::fs::read(&first.index_path).unwrap();
        let sig = key.sign(&body, &first.trusted_comment);
        let keys = [IndexKey::parse(&key.public_base64()).unwrap()];
        assert!(verify_index(&body, &sig, &keys).is_ok());
    }

    #[test]
    fn a_script_cannot_be_bundled_as_an_adapter() {
        let tmp = tempfile::tempdir().unwrap();
        let input = adapter_dir(tmp.path(), "alpha", "0.1.0", "[]");
        std::fs::write(&input.exe, b"#!/bin/sh\nexit 0\n").unwrap();
        let out = tmp.path().join("out");
        let error = build(&out, &[input], &options()).unwrap_err();
        assert!(error.contains("native executable"), "{error}");
        assert!(!out.exists());
    }

    #[test]
    fn a_manifest_that_import_would_reject_cannot_be_bundled() {
        let tmp = tempfile::tempdir().unwrap();
        let input = adapter_dir(tmp.path(), "alpha", "0.1.0", "[]");
        let path = input.manifest_dir.join("manifest.json");
        let mut manifest: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        manifest["provides"]["programs"] = json!([]);
        std::fs::write(path, manifest.to_string()).unwrap();
        let out = tmp.path().join("out");
        let error = build(&out, &[input], &options()).unwrap_err();
        assert!(error.contains("must list provides.programs"), "{error}");
        assert!(!out.exists());
    }

    #[test]
    fn an_invalid_index_leaves_the_previous_bundle_intact() {
        let tmp = tempfile::tempdir().unwrap();
        let input = adapter_dir(tmp.path(), "alpha", "0.1.0", "[]");
        let out = tmp.path().join("out");
        let previous = build(&out, std::slice::from_ref(&input), &options()).unwrap();
        let original_archive = std::fs::read(&previous.archives[0]).unwrap();
        let original_index = std::fs::read(&previous.index_path).unwrap();
        let original_checksums = fs::read(out.join(CHECKSUMS)).unwrap();
        std::fs::write(out.join("index.json.minisig"), b"old signature").unwrap();
        let mut changed = std::fs::read(&input.exe).unwrap();
        changed.push(b'!');
        std::fs::write(&input.exe, changed).unwrap();
        let mut invalid = options();
        invalid.target = "bad target".into();
        assert!(build(&out, std::slice::from_ref(&input), &invalid)
            .unwrap_err()
            .contains("index is not valid"));
        assert_eq!(
            std::fs::read(&previous.archives[0]).unwrap(),
            original_archive
        );
        assert_eq!(std::fs::read(&previous.index_path).unwrap(), original_index);
        assert_eq!(fs::read(out.join(CHECKSUMS)).unwrap(), original_checksums);
        assert_eq!(
            std::fs::read(out.join("index.json.minisig")).unwrap(),
            b"old signature"
        );

        let replacement = build(&out, &[input], &options()).unwrap();
        assert_ne!(
            fs::read(replacement.archives[0].as_path()).unwrap(),
            original_archive
        );
        assert_ne!(fs::read(replacement.index_path).unwrap(), original_index);
        assert_ne!(fs::read(out.join(CHECKSUMS)).unwrap(), original_checksums);
        assert!(!out.join("index.json.minisig").exists());
    }

    #[test]
    fn bad_input_is_refused_with_a_reason() {
        let tmp = tempfile::tempdir().unwrap();
        let ok = adapter_dir(tmp.path(), "alpha", "0.1.0", "[]");
        let out = tmp.path().join("out");
        let mut http = options();
        http.url_base = "http://example.org".into();
        assert!(build(&out, std::slice::from_ref(&ok), &http)
            .unwrap_err()
            .contains("not an https"));
        assert!(build(&out, &[], &options())
            .unwrap_err()
            .contains("no adapter"));
        let missing_exe = Input {
            exe: tmp.path().join("nothing"),
            ..ok.clone()
        };
        assert!(build(&out, &[missing_exe], &options())
            .unwrap_err()
            .contains("cannot read"));
        std::fs::write(ok.manifest_dir.join("manifest.json"), "{}").unwrap();
        assert!(build(&out, &[ok], &options()).is_err());
    }
}
