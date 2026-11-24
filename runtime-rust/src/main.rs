//! The rust and cargo runtimes.
//!
//! The daemon asks what to run for an app (`runtime.prepare`, then `runtime.plan`) and runs it
//! itself, the build included; this program only answers. It reads no file and starts no process.

mod provider;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    match oxidepm_adapter_sdk::run_runtime(
        "runtime-rust",
        env!("CARGO_PKG_VERSION"),
        provider::Rust,
    )
    .await
    {
        oxidepm_adapter_sdk::Exit::Clean => {}
        oxidepm_adapter_sdk::Exit::Refused(why) => {
            eprintln!("runtime-rust: {why}");
            std::process::exit(2);
        }
    }
}
