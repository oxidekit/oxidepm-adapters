mod deps;
mod git;
mod service;
mod spec;
mod url;

use oxidepm_adapter_sdk::Exit;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    if let Exit::Refused(why) =
        oxidepm_adapter_sdk::run("git-start", env!("CARGO_PKG_VERSION"), |init| {
            service::GitStart {
                state: init.paths.state.clone().into(),
            }
        })
        .await
    {
        eprintln!("git-start: {why}");
        std::process::exit(1);
    }
}
