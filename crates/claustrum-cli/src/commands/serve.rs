//! `claustrum serve`: MCP server over stdio.

use std::path::PathBuf;

use anyhow::Result;

use crate::config::Config;

#[derive(clap::Args, Debug)]
pub struct Args {
    /// Host directory to mount at /workspace. Defaults to the current directory.
    #[arg(long)]
    pub workspace: Option<PathBuf>,
}

pub async fn run(config: Config, args: Args) -> Result<()> {
    let sandbox = config.build_sandbox(args.workspace.as_deref()).await?;
    tracing::info!(
        workspace = %sandbox.workspace_dir().display(),
        commands = sandbox.commands().len(),
        "sandbox ready"
    );
    if let Some(notice) = Config::actions_notice(sandbox.actions()) {
        eprintln!("claustrum: {notice}");
    }
    claustrum_mcp::serve_stdio(sandbox).await
}
