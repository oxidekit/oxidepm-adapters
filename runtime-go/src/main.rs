//! The go runtime.
//!
//! The daemon asks what to run for an app (`runtime.prepare`, then `runtime.plan`) and runs it
//! itself; this program only answers. It reads no file and starts no process.

mod provider;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    match oxidepm_adapter_sdk::run_runtime("runtime-go", env!("CARGO_PKG_VERSION"), provider::Go)
        .await
    {
        oxidepm_adapter_sdk::Exit::Clean => {}
        oxidepm_adapter_sdk::Exit::Refused(why) => {
            eprintln!("runtime-go: {why}");
            std::process::exit(2);
        }
    }
}
