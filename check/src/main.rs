mod args;
mod checks;
mod port;
mod service;

use oxidepm_adapter_sdk::Exit;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    if let Exit::Refused(why) =
        oxidepm_adapter_sdk::run("check", env!("CARGO_PKG_VERSION"), |_| service::Check).await
    {
        eprintln!("check: {why}");
        std::process::exit(1);
    }
}
