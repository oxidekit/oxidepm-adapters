//! The node, npm, pnpm and yarn runtimes.
//!
//! The daemon asks what to run for an app (`runtime.prepare`, then `runtime.plan`) and runs it
//! itself; this program only answers. It reads no file and starts no process.

mod provider;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    match oxidepm_adapter_sdk::run_runtime(
        "runtime-node",
        env!("CARGO_PKG_VERSION"),
        provider::Node,
    )
    .await
    {
        oxidepm_adapter_sdk::Exit::Clean => {}
        oxidepm_adapter_sdk::Exit::Refused(why) => {
            eprintln!("runtime-node: {why}");
            std::process::exit(2);
        }
    }
}
