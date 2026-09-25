//! `claustrum`: run Claude Code against a WASIX sandbox.

mod commands;
mod config;
mod trust;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "claustrum", version, about = "A WASIX sandbox for Claude Code")]
struct Cli {
    /// Configuration file. Defaults to ./claustrum.toml, then the user config
    /// directory, then built-in defaults.
    #[arg(long, global = true, env = "CLAUSTRUM_CONFIG")]
    config: Option<PathBuf>,

    /// Directory containing the `.webc` packages.
    #[arg(long, global = true, env = "CLAUSTRUM_PACKAGES_DIR")]
    packages_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Launch Claude Code with its built-in tools replaced by the sandbox.
    Run(commands::run::Args),
    /// Serve the sandbox tools over MCP on stdio (used by `run`).
    Serve(commands::serve::Args),
    /// Manage the WASIX packages available inside the sandbox.
    #[command(subcommand)]
    Pkg(commands::pkg::Command),
    /// Inspect network decisions and build the allowlist.
    #[command(subcommand)]
    Network(commands::network::Command),
    /// List the plans Claude wrote for this workspace.
    Plans(commands::plans::Args),
    /// Review the project's claustrum.toml and trust it (or `--revoke`).
    Trust {
        /// Forget that the file was trusted.
        #[arg(long)]
        revoke: bool,
    },
    /// Internal: the confined half of `serve`, started by it.
    #[cfg(unix)]
    #[command(name = "__worker", hide = true)]
    Worker(commands::serve::WorkerArgs),
}

fn main() -> anyhow::Result<()> {
    // The confinement helper runs between the sandbox setup and the confined
    // program; it must not load configuration or print anything.
    if std::env::args_os().nth(1).as_deref() == Some(claustrum_confine::HELPER_ARG.as_ref()) {
        claustrum_confine::helper_main();
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("CLAUSTRUM_LOG")
                .or_else(|_| tracing_subscriber::EnvFilter::try_from_default_env())
                // The module cache logs a warning on every cache miss
                // ("unable to remove the corrupted cache file"); that is
                // normal on first use, so keep it out of the default output.
                .unwrap_or_else(|_| {
                    tracing_subscriber::EnvFilter::new(
                        "warn,wasmer_wasix::runtime::module_cache::filesystem=error",
                    )
                }),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    let config = config::load(cli.config.as_deref(), cli.packages_dir.as_deref())?;
    // A project's own claustrum.toml configures the sandbox meant to contain
    // that project; nothing uses it before the user trusted it.
    if !matches!(cli.command, Command::Trust { .. }) {
        trust::ensure(&config)?;
    }

    match cli.command {
        Command::Run(args) => commands::run::run(config, args),
        Command::Serve(args) => runtime()?.block_on(commands::serve::run(config, args)),
        Command::Pkg(cmd) => runtime()?.block_on(commands::pkg::run(config, cmd)),
        Command::Network(cmd) => commands::network::run(config, cmd),
        Command::Plans(args) => commands::plans::run(config, args),
        Command::Trust { revoke } => trust::command(&config, revoke),
        #[cfg(unix)]
        Command::Worker(args) => runtime()?.block_on(commands::serve::worker(config, args)),
    }
}

fn runtime() -> anyhow::Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("claustrum")
        .build()?)
}
