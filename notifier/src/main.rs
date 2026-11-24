//! `oxidepm-notifier`: the notification adapter. It is started by `oxidepmd` and speaks the adapter
//! protocol on stdin and stdout; stderr is its log.

mod channels;
mod config;
mod event;
mod limiter;
mod service;
mod verbs;

use oxidepm_adapter_sdk::Exit;

const MANIFEST: &str = include_str!("../manifest.json");

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let manifest: serde_json::Value = serde_json::from_str(MANIFEST).expect("the manifest is JSON");
    let name = manifest["name"].as_str().expect("a name").to_string();
    let version = manifest["version"].as_str().expect("a version").to_string();
    match oxidepm_adapter_sdk::run(&name, &version, service::Notifier::new).await {
        Exit::Clean => {}
        Exit::Refused(why) => {
            eprintln!("notifier: {why}");
            std::process::exit(1);
        }
    }
}
