//! `claustrum`: run Claude Code against a WASIX sandbox.

mod commands;
mod config;

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
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("CLAUSTRUM_LOG")
                .or_else(|_| tracing_subscriber::EnvFilter::try_from_default_env())
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    let config = config::load(cli.config.as_deref(), cli.packages_dir.as_deref())?;

    match cli.command {
        Command::Run(args) => commands::run::run(config, args),
        Command::Serve(args) => runtime()?.block_on(commands::serve::run(config, args)),
        Command::Pkg(cmd) => runtime()?.block_on(commands::pkg::run(config, cmd)),
    }
}

fn runtime() -> anyhow::Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("claustrum")
        .build()?)
}
