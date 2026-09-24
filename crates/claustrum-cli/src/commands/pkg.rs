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
    /// Register a self-built WASIX `.wasm` binary as a directory package.
    ///
    /// Creates `<packages-dir>/<name>/` with a `wasmer.toml` and a copy of the
    /// module. Add it to the configuration with `file = "<name>"`.
    AddWasm {
        /// Package and command name, e.g. `jq`.
        name: String,
        /// Path to the `.wasm` file compiled for wasm32-wasmer-wasi.
        wasm: std::path::PathBuf,
        /// Additional command names served by the same module.
        #[arg(long = "alias")]
        aliases: Vec<String>,
    },
    /// Show configured packages and whether they are installed.
    List,
    /// Print the commands provided by the installed packages.
    Commands,
    /// Compile all installed packages into the module cache ahead of time.
    Precompile,
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
        Command::AddWasm {
            name,
            wasm,
            aliases,
        } => add_wasm(&config, &name, &wasm, &aliases),
        Command::List => {
            println!("packages directory: {}", config.packages_dir.display());
            for p in config.packages() {
                let status = if p.file.is_dir() && p.is_installed() {
                    "installed (directory package)".to_owned()
                } else if p.file.is_file() {
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
            let sandbox = config
                .build_sandbox(Some(&std::env::temp_dir()), None)
                .await?;
            for c in sandbox.commands() {
                println!("{c}");
            }
            Ok(())
        }
        Command::Precompile => precompile(&config).await,
    }
}

async fn precompile(config: &Config) -> Result<()> {
    let sandbox = config
        .build_sandbox(Some(&std::env::temp_dir()), None)
        .await?;
    let started = std::time::Instant::now();
    let n = sandbox.precompile().await?;
    println!("{n} module(s) ready in {:.1?}", started.elapsed());
    Ok(())
}

fn add_wasm(config: &Config, name: &str, wasm: &std::path::Path, aliases: &[String]) -> Result<()> {
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_".contains(c))
    {
        anyhow::bail!("package name must consist of letters, digits, `-` or `_`");
    }
    let bytes = std::fs::read(wasm).with_context(|| format!("cannot read {}", wasm.display()))?;
    if !bytes.starts_with(b"\0asm") {
        anyhow::bail!("{} is not a WebAssembly module", wasm.display());
    }
    let dir = config.packages_dir.join(name);
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    std::fs::write(dir.join(format!("{name}.wasm")), &bytes)?;

    let mut manifest = format!(
        "[package]\nname = \"claustrum/{name}\"\nversion = \"0.0.0\"\nentrypoint = \"{name}\"\n\n\
         [[module]]\nname = \"{name}\"\nsource = \"./{name}.wasm\"\nabi = \"wasi\"\n"
    );
    for command in std::iter::once(name).chain(aliases.iter().map(String::as_str)) {
        manifest.push_str(&format!(
            "\n[[command]]\nname = \"{command}\"\nmodule = \"{name}\"\nrunner = \"https://webc.org/runner/wasi\"\n\n\
             [command.annotations.wasi]\natom = \"{name}\"\n"
        ));
    }
    std::fs::write(dir.join("wasmer.toml"), manifest)?;
    println!("created directory package {}", dir.display());
    println!("add to claustrum.toml:\n\n[[packages.package]]\nfile = \"{name}\"");
    Ok(())
}

async fn sync(config: &Config, force: bool) -> Result<()> {
    std::fs::create_dir_all(&config.packages_dir)
        .with_context(|| format!("cannot create {}", config.packages_dir.display()))?;
    for p in config.packages() {
        if p.is_installed() && !force {
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
    println!("compiling modules (this can take a minute for python) ...");
    precompile(config).await
}
