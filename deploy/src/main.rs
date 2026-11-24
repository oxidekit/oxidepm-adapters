mod deploy;
mod diff;
mod service;
mod shell;
mod ssh;

use oxidepm_adapter_sdk::Exit;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    if let Exit::Refused(why) =
        oxidepm_adapter_sdk::run("deploy", env!("CARGO_PKG_VERSION"), |_| service::Deploy).await
    {
        eprintln!("deploy: {why}");
        std::process::exit(1);
    }
}
