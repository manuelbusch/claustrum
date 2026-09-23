//! `claustrum pkg`: manage the `.webc` packages.

use anyhow::{Context, Result};
use clap::Subcommand;

use crate::config::{Config, read_stamp, stamp_path};

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Download all configured packages that are missing (or all with --force).
    Sync {
        /// Re-download packages that are already present.
        #[arg(long)]
        force: bool,
    },
    /// Download one package from the Wasmer registry into the packages directory.
    Add {
        /// Package spec, e.g. `wasmer/python` or `wasmer/bash@1.0.25`.
        spec: String,
    },
    /// Show configured packages and whether they are installed.
    List,
    /// Print the commands provided by the installed packages.
    Commands,
}

pub async fn run(config: Config, cmd: Command) -> Result<()> {
    match cmd {
        Command::Sync { force } => sync(&config, force).await,
        Command::Add { spec } => {
            let d = claustrum_sandbox::download(&spec, &config.packages_dir).await?;
            std::fs::write(stamp_path(&d.path), format!("{}\n", d.id))?;
            println!("{} -> {}", d.id, d.path.display());
            Ok(())
        }
        Command::List => {
            println!("packages directory: {}", config.packages_dir.display());
            for p in config.packages() {
                let status = if p.file.is_file() {
                    p.id.clone()
                        .or_else(|| read_stamp(&p.file))
                        .map(|id| format!("installed ({id})"))
                        .unwrap_or_else(|| "installed".to_owned())
                } else {
                    "missing".to_owned()
                };
                println!(
                    "{:<40} {}",
                    p.file
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    status
                );
            }
            Ok(())
        }
        Command::Commands => {
            let sandbox = config.build_sandbox(Some(&std::env::temp_dir())).await?;
            for c in sandbox.commands() {
                println!("{c}");
            }
            Ok(())
        }
    }
}

async fn sync(config: &Config, force: bool) -> Result<()> {
    std::fs::create_dir_all(&config.packages_dir)
        .with_context(|| format!("cannot create {}", config.packages_dir.display()))?;
    for p in config.packages() {
        if p.file.is_file() && !force {
            println!("{} already present", p.file.display());
            continue;
        }
        let Some(spec) = &p.source else {
            println!(
                "{} is missing and has no `source`; add it manually",
                p.file.display()
            );
            continue;
        };
        print!("downloading {spec} ... ");
        let d = claustrum_sandbox::download(spec, &config.packages_dir).await?;
        if d.path != p.file {
            std::fs::rename(&d.path, &p.file)?;
        }
        std::fs::write(stamp_path(&p.file), format!("{}\n", d.id))?;
        println!("{} ({})", d.id, p.file.display());
    }
    Ok(())
}
