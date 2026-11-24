//! `oxidepm-exporter-prometheus`: Prometheus metrics for the managed apps. It is started by
//! `oxidepmd` and speaks the adapter protocol on stdin and stdout; stderr is its log.

mod config;
mod render;
mod server;
mod service;
mod source;

use oxidepm_adapter_sdk::Exit;

const MANIFEST: &str = include_str!("../manifest.json");

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let manifest: serde_json::Value = serde_json::from_str(MANIFEST).expect("the manifest is JSON");
    let name = manifest["name"].as_str().expect("a name").to_string();
    let version = manifest["version"].as_str().expect("a version").to_string();
    match oxidepm_adapter_sdk::run_with_host(&name, &version, service::Exporter::new).await {
        Exit::Clean => {}
        Exit::Refused(why) => {
            eprintln!("exporter-prometheus: {why}");
            std::process::exit(1);
        }
    }
}
