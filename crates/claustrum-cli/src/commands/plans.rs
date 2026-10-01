//! `claustrum plans`: the plans Claude wrote for a workspace.

use std::{path::PathBuf, time::SystemTime};

use anyhow::{Context, Result};

use crate::config::Config;

#[derive(clap::Args, Debug)]
pub struct Args {
    /// Workspace whose plans to list. Defaults to the current directory.
    #[arg(long)]
    pub workspace: Option<PathBuf>,
}

pub fn run(config: Config, args: Args) -> Result<()> {
    let workspace = config.workspace(args.workspace.as_deref())?;
    config.migrate_state(&workspace);
    let Some(plans) = config.host_plans(&workspace)? else {
        println!("Plans are disabled ([claude] plans = false).");
        return Ok(());
    };
    let names = plans.names().context("cannot read the plan ledger")?;
    let mut rows: Vec<(SystemTime, PathBuf)> = names
        .into_iter()
        .map(|n| plans.dir().join(n))
        .filter_map(|p| {
            // Only plain files; the ledger may name plans deleted since.
            let meta = std::fs::symlink_metadata(&p).ok()?;
            meta.is_file()
                .then(|| (meta.modified().unwrap_or(SystemTime::UNIX_EPOCH), p))
        })
        .collect();
    if rows.is_empty() {
        println!("No plans for {} yet.", workspace.display());
        return Ok(());
    }
    rows.sort_by(|a, b| b.cmp(a));
    for (modified, path) in rows {
        println!("{}  {}", age(modified), path.display());
    }
    Ok(())
}

/// Age of a file, coarse enough for a listing.
fn age(t: SystemTime) -> String {
    let secs = SystemTime::now()
        .duration_since(t)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    match secs {
        s if s < 3600 => format!("{:>3}m ago", s / 60),
        s if s < 86_400 => format!("{:>3}h ago", s / 3600),
        s => format!("{:>3}d ago", s / 86_400),
    }
}
