//! `oxidepm-adapter-bundle --out DIR --target TRIPLE --url-base https://... --sequence N
//! ADAPTER_DIR:EXECUTABLE...`
//!
//! Writes the archives, `index.json` and `SHA256SUMS` for a release bundle.
//! Prints a signing command for publishing the catalog over the network.

use std::path::PathBuf;

use oxidepm_adapter_bundle::{build, Input, Options};

const USAGE: &str = "usage: oxidepm-adapter-bundle --out DIR --target TRIPLE --url-base https://... --sequence N [--valid-days 90] ADAPTER_DIR:EXECUTABLE...";

fn main() {
    if let Err(why) = run() {
        eprintln!("oxidepm-adapter-bundle: {why}");
        std::process::exit(2);
    }
}

fn run() -> Result<(), String> {
    let (mut out, mut target, mut url_base, mut sequence, mut days) =
        (None, None, None, None, 90i64);
    let mut inputs = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = |name: &str| {
            args.next()
                .ok_or_else(|| format!("{name} needs a value\n{USAGE}"))
        };
        match arg.as_str() {
            "--out" => out = Some(PathBuf::from(value("--out")?)),
            "--target" => target = Some(value("--target")?),
            "--url-base" => url_base = Some(value("--url-base")?),
            "--sequence" => {
                sequence = Some(
                    value("--sequence")?
                        .parse::<u64>()
                        .map_err(|e| format!("--sequence: {e}"))?,
                )
            }
            "--valid-days" => {
                days = value("--valid-days")?
                    .parse::<i64>()
                    .map_err(|e| format!("--valid-days: {e}"))?
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            other if other.starts_with("--") => {
                return Err(format!("unknown option {other}\n{USAGE}"))
            }
            pair => {
                let (dir, exe) = pair
                    .split_once(':')
                    .ok_or_else(|| format!("{pair:?} is not ADAPTER_DIR:EXECUTABLE\n{USAGE}"))?;
                inputs.push(Input {
                    manifest_dir: dir.into(),
                    exe: exe.into(),
                });
            }
        }
    }
    let need =
        |v: Option<String>, name: &str| v.ok_or_else(|| format!("{name} is required\n{USAGE}"));
    let out = out.ok_or_else(|| format!("--out is required\n{USAGE}"))?;
    let opts = Options {
        target: need(target, "--target")?,
        url_base: need(url_base, "--url-base")?,
        sequence: sequence.ok_or_else(|| format!("--sequence is required\n{USAGE}"))?,
        generated: chrono::Utc::now(),
        valid_for: chrono::Duration::days(days),
    };
    let built = build(&out, &inputs, &opts)?;
    println!(
        "wrote {} archives, {} and SHA256SUMS",
        built.archives.len(),
        built.index_path.display()
    );
    println!(
        "for a network catalog, sign the index with the release key using this trusted comment:"
    );
    println!(
        "  minisign -S -s <secret key> -m {} -x {}.minisig -t '{}'",
        built.index_path.display(),
        built.index_path.display(),
        built.trusted_comment
    );
    Ok(())
}
