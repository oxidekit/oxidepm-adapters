//! `oxidepm-ui-web`: the web API adapter. It is started by `oxidepmd` and speaks the adapter
//! protocol on stdin and stdout; stderr is its log.

mod api;
mod config;
mod daemon;
mod guard;
mod hub;
mod server;
mod service;

use oxidepm_adapter_sdk::Exit;

const MANIFEST: &str = include_str!("../manifest.json");

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let manifest: serde_json::Value = serde_json::from_str(MANIFEST).expect("the manifest is JSON");
    let name = manifest["name"].as_str().expect("a name").to_string();
    let version = manifest["version"].as_str().expect("a version").to_string();
    match oxidepm_adapter_sdk::run_with_host(&name, &version, service::Web::new).await {
        Exit::Clean => {}
        Exit::Refused(why) => {
            eprintln!("ui-web: {why}");
            std::process::exit(1);
        }
    }
}
