//! `oxidepm-ui-tui`: the terminal dashboard adapter (`oxidepm monit`). The daemon starts it for
//! one session on the person's terminal; it speaks the adapter protocol over a socket the daemon
//! made for that session, and stderr is its log.

mod app;
mod daemon;
mod run;
mod service;
mod term;
#[cfg(test)]
mod testkit;
mod view;

use oxidepm_adapter_sdk::Exit;

const MANIFEST: &str = include_str!("../manifest.json");

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let manifest: serde_json::Value = serde_json::from_str(MANIFEST).expect("the manifest is JSON");
    let name = manifest["name"].as_str().expect("a name").to_string();
    let version = manifest["version"].as_str().expect("a version").to_string();
    // started for a session the daemon names a socket; started any other way (when the adapter is
    // enabled the daemon runs it once to check that it answers) the protocol is on stdin and stdout
    let exit = if std::env::var_os("OXIDEPM_SESSION_SOCKET").is_some() {
        oxidepm_adapter_sdk::run_session(&name, &version, |_, host| {
            service::Monitor::new(host, true)
        })
        .await
    } else {
        oxidepm_adapter_sdk::run_with_host(&name, &version, |_, host| {
            service::Monitor::new(host, false)
        })
        .await
    };
    match exit {
        Exit::Clean => {}
        Exit::Refused(why) => {
            eprintln!("ui-tui: {why}");
            std::process::exit(1);
        }
    }
}
