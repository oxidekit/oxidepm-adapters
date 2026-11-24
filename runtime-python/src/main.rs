//! The python runtime.
//!
//! The daemon asks what to run for an app (`runtime.prepare`, then `runtime.plan`) and runs it
//! itself; this program only answers. It reads no file and starts no process.

mod provider;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    match oxidepm_adapter_sdk::run_runtime(
        "runtime-python",
        env!("CARGO_PKG_VERSION"),
        provider::Python,
    )
    .await
    {
        oxidepm_adapter_sdk::Exit::Clean => {}
        oxidepm_adapter_sdk::Exit::Refused(why) => {
            eprintln!("runtime-python: {why}");
            std::process::exit(2);
        }
    }
}
