//! Smoke test: run bash + coreutils inside the sandbox.
//!
//! Usage: cargo run -p claustrum-sandbox --example spike -- <workspace> <script>

use claustrum_sandbox::{ExecOptions, Sandbox};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    let mut args = std::env::args().skip(1);
    let workspace = args.next().unwrap_or_else(|| ".".into());
    let script = args.next().unwrap_or_else(|| {
        "echo hello from $0; pwd; ls -la /workspace | head; echo piped | tr a-z A-Z".into()
    });

    let root = env!("CARGO_MANIFEST_DIR");
    let sandbox = Sandbox::builder()
        .workspace(workspace)
        .package_named(
            format!("{root}/../../packages/bash.webc"),
            "wasmer/bash@1.0.25",
        )
        .package_named(
            format!("{root}/../../packages/coreutils.webc"),
            "wasmer/coreutils@1.0.25",
        )
        .build()
        .await?;

    eprintln!("commands: {}", sandbox.commands().join(" "));

    let out = sandbox.bash(&script, ExecOptions::default()).await?;
    println!(
        "--- exit {} ({:?}) in {:?}",
        out.exit_code, out.reason, out.duration
    );
    println!("--- stdout\n{}", out.stdout_lossy());
    println!("--- stderr\n{}", out.stderr_lossy());
    Ok(())
}
