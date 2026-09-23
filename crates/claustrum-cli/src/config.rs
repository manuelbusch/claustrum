//! Configuration: `claustrum.toml` plus built-in defaults.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result};
use claustrum_sandbox::{NetworkPolicy, Policy, RuntimeConfig, Sandbox, SandboxBuilder};
use serde::Deserialize;

/// Packages installed by `claustrum pkg sync` when none are configured.
pub const DEFAULT_PACKAGES: &[(&str, &str)] = &[
    ("wasmer/bash", "bash.webc"),
    ("wasmer/coreutils", "coreutils.webc"),
];

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    #[serde(default)]
    pub sandbox: SandboxSection,
    #[serde(default)]
    pub packages: PackagesSection,
    #[serde(default)]
    pub mounts: Vec<MountEntry>,
    #[serde(default)]
    pub claude: ClaudeSection,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct SandboxSection {
    /// Host directory mounted at /workspace. Defaults to the current directory.
    pub workspace: Option<PathBuf>,
    /// `"disabled"` (default), `"host"`, or a list of ruleset entries.
    pub network: Option<Network>,
    /// Default command timeout in seconds. 0 disables the timeout.
    pub timeout_secs: Option<u64>,
    /// Bytes retained per output stream.
    pub max_output_bytes: Option<usize>,
    pub max_threads: Option<u32>,
    /// Extra environment variables for guest commands.
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum Network {
    Mode(String),
    Rules(Vec<String>),
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct PackagesSection {
    /// Directory holding the `.webc` files.
    pub dir: Option<PathBuf>,
    /// Allow resolving packages from the Wasmer registry at runtime.
    #[serde(default)]
    pub online: bool,
    /// Packages to load. Defaults to bash and coreutils.
    #[serde(default, rename = "package")]
    pub list: Vec<PackageEntry>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct PackageEntry {
    /// File name inside the packages directory, or an absolute path.
    pub file: PathBuf,
    /// Registry identity `namespace/name@version`, needed when other
    /// packages depend on this one.
    pub id: Option<String>,
    /// Registry spec used by `pkg sync` to (re)download the file.
    pub source: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct MountEntry {
    pub guest: String,
    pub host: PathBuf,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ClaudeSection {
    /// Path to the `claude` binary.
    pub binary: Option<PathBuf>,
    /// Built-in Claude Code tools to keep enabled (passed to `--tools`).
    /// Empty by default, which removes every built-in tool. `WebSearch` is a
    /// reasonable addition since it runs on the API side, not on the host.
    #[serde(default)]
    pub tools: Vec<String>,
    /// Extra arguments always passed to `claude`.
    #[serde(default)]
    pub args: Vec<String>,
    /// Additional text appended to the system prompt.
    pub system_prompt: Option<String>,
}

/// Fully resolved configuration.
#[derive(Debug)]
pub struct Config {
    pub file: FileConfig,
    pub path: Option<PathBuf>,
    pub packages_dir: PathBuf,
}

pub fn load(explicit: Option<&Path>, packages_dir: Option<&Path>) -> Result<Config> {
    let path = match explicit {
        Some(p) => Some(p.to_path_buf()),
        None => default_config_paths().into_iter().find(|p| p.is_file()),
    };
    let file = match &path {
        Some(p) => {
            let text = std::fs::read_to_string(p)
                .with_context(|| format!("cannot read config {}", p.display()))?;
            toml::from_str(&text).with_context(|| format!("invalid config {}", p.display()))?
        }
        None => FileConfig::default(),
    };
    let packages_dir = packages_dir
        .map(Path::to_path_buf)
        .or_else(|| file.packages.dir.clone())
        .map(|d| expand_home(&d))
        .unwrap_or_else(default_packages_dir);
    let packages_dir = if packages_dir.is_relative() {
        std::env::current_dir()?.join(packages_dir)
    } else {
        packages_dir
    };
    Ok(Config {
        file,
        path,
        packages_dir,
    })
}

fn default_config_paths() -> Vec<PathBuf> {
    let mut paths = vec![PathBuf::from("claustrum.toml")];
    if let Some(dirs) = project_dirs() {
        paths.push(dirs.config_dir().join("config.toml"));
    }
    paths
}

pub fn project_dirs() -> Option<directories::ProjectDirs> {
    directories::ProjectDirs::from("de", "buschmanuel", "claustrum")
}

pub fn default_packages_dir() -> PathBuf {
    project_dirs()
        .map(|d| d.data_dir().join("packages"))
        .unwrap_or_else(|| PathBuf::from("packages"))
}

fn expand_home(p: &Path) -> PathBuf {
    if let Ok(rest) = p.strip_prefix("~")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    p.to_path_buf()
}

impl Config {
    /// The packages to load, with file names resolved against the packages dir.
    pub fn packages(&self) -> Vec<PackageEntry> {
        let entries = if self.file.packages.list.is_empty() {
            DEFAULT_PACKAGES
                .iter()
                .map(|(source, file)| PackageEntry {
                    file: PathBuf::from(file),
                    id: None,
                    source: Some((*source).to_owned()),
                })
                .collect()
        } else {
            self.file.packages.list.clone()
        };
        entries
            .into_iter()
            .map(|mut e| {
                if e.file.is_relative() {
                    e.file = self.packages_dir.join(&e.file);
                }
                e
            })
            .collect()
    }

    pub fn workspace(&self, override_dir: Option<&Path>) -> Result<PathBuf> {
        let dir = override_dir
            .map(Path::to_path_buf)
            .or_else(|| self.file.sandbox.workspace.clone())
            .map(|d| expand_home(&d))
            .unwrap_or(std::env::current_dir()?);
        dir.canonicalize()
            .with_context(|| format!("workspace {} does not exist", dir.display()))
    }

    pub fn policy(&self) -> Result<Policy> {
        let s = &self.file.sandbox;
        let mut policy = Policy::default();
        if let Some(network) = &s.network {
            policy.network = match network {
                Network::Mode(m) if m == "disabled" || m == "none" => NetworkPolicy::Disabled,
                Network::Mode(m) if m == "host" => NetworkPolicy::Host,
                Network::Mode(m) => anyhow::bail!("unknown network mode `{m}`"),
                Network::Rules(rules) => NetworkPolicy::Ruleset(rules.clone()),
            };
        }
        if let Some(secs) = s.timeout_secs {
            policy.default_timeout = (secs > 0).then(|| Duration::from_secs(secs));
        }
        if let Some(bytes) = s.max_output_bytes {
            policy.max_output_bytes = bytes;
        }
        if let Some(threads) = s.max_threads {
            policy.max_threads = Some(threads);
        }
        Ok(policy)
    }

    /// Build the sandbox described by this configuration.
    pub async fn build_sandbox(&self, workspace: Option<&Path>) -> Result<Sandbox> {
        let workspace = self.workspace(workspace)?;
        let mut builder: SandboxBuilder = Sandbox::builder()
            .workspace(&workspace)
            .policy(self.policy()?)
            .runtime_config(RuntimeConfig {
                online: self.file.packages.online,
                ..RuntimeConfig::default()
            });

        let packages = self.packages();
        let missing: Vec<_> = packages.iter().filter(|p| !p.file.is_file()).collect();
        if !missing.is_empty() {
            anyhow::bail!(
                "missing package file(s): {}\nRun `claustrum pkg sync` to download them.",
                missing
                    .iter()
                    .map(|p| p.file.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        for p in packages {
            let id = p.id.clone().or_else(|| read_stamp(&p.file));
            builder = match id {
                Some(id) => builder.package_named(&p.file, id),
                None => builder.package(&p.file),
            };
        }
        for (k, v) in &self.file.sandbox.env {
            builder = builder.env(k, v);
        }
        for m in &self.file.mounts {
            builder = builder.mount(&m.guest, expand_home(&m.host));
        }
        Ok(builder.build().await?)
    }
}

/// `pkg sync` records the resolved identity next to each file so that
/// dependencies between packages resolve without explicit `id` entries.
pub fn stamp_path(webc: &Path) -> PathBuf {
    webc.with_extension("webc.id")
}

pub fn read_stamp(webc: &Path) -> Option<String> {
    std::fs::read_to_string(stamp_path(webc))
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}
