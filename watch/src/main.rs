mod config;
mod debounce;
mod ignore;
mod service;
mod watcher;

use oxidepm_adapter_sdk::{run_with_host, Exit};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    if let Exit::Refused(reason) = run_with_host("watch", env!("CARGO_PKG_VERSION"), |_, host| {
        service::Watch::new(host)
    })
    .await
    {
        eprintln!("{reason}");
        std::process::exit(1);
    }
}
