mod detect;
mod render;
mod service;

use oxidepm_adapter_sdk::Exit;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    if let Exit::Refused(why) =
        oxidepm_adapter_sdk::run("init", env!("CARGO_PKG_VERSION"), |_| service::Init).await
    {
        eprintln!("init: {why}");
        std::process::exit(1);
    }
}
