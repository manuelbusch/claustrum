//! Configuration: `claustrum.toml` plus built-in defaults.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result};
use claustrum_sandbox::{
    ActionDef, Confinement, ConfinementMode, NetworkPolicy, Policy, RuntimeConfig, Sandbox,
    SandboxBuilder,
    action::{self, ActionExecutor, CompileContext},
    net::{NetMode, NetPolicy},
    plans::{HostPlans, PlanStore},
};
use serde::Deserialize;

/// Claude Code settings files in a project's `.claude` directory.
pub const CLAUDE_SETTINGS: &[&str] = &["settings.json", "settings.local.json"];

/// Packages installed by `claustrum pkg sync` when none are configured.
/// Pinned with `=`: a bare `@1.0.25` is a semver requirement (`^1.0.25`), and
/// the registry lookup takes the highest match.
pub const DEFAULT_PACKAGES: &[(&str, &str)] = &[
    ("wasmer/bash@=1.0.25", "bash.webc"),
    ("wasmer/coreutils@=1.0.25", "coreutils.webc"),
    ("python/python@=3.13.20", "python.webc"),
    ("syrusakbary/jq@=0.1.0", "jq.webc"),
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
    #[serde(default)]
    pub actions: ActionsSection,
    #[serde(default)]
    pub network: NetworkSection,
}

/// Network access for the guest and for host actions; see
/// `claustrum_sandbox::net`.
#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct NetworkSection {
    /// `"disabled"` (default), `"allowlist"`, `"audit"` or `"host"`.
    pub mode: Option<String>,
    /// Destinations such as `crates.io`, `*.github.com:443`, `10.0.0.5:5432`.
    #[serde(default)]
    pub allow: Vec<String>,
    /// JSONL file for every decision. Defaults to a per-workspace file in
    /// the user state directory.
    pub log: Option<PathBuf>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct SandboxSection {
    /// Host directory mounted at /workspace. Defaults to the current directory.
    pub workspace: Option<PathBuf>,
    /// Shorthand for `[network].mode`: `"disabled"` or `"host"`.
    pub network: Option<Network>,
    /// Default command timeout in seconds. 0 disables the timeout.
    pub timeout_secs: Option<u64>,
    /// Bytes retained per output stream.
    pub max_output_bytes: Option<usize>,
    pub max_threads: Option<u32>,
    /// Largest memory one guest process may use, in MiB. 0 allows wasm32's
    /// 4 GiB. Defaults to 1024.
    pub max_memory_mb: Option<u64>,
    /// Extra environment variables for guest commands.
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
    /// OS sandbox around the Wasmer worker and the host actions (the second
    /// layer): `"best-effort"` (default), `"required"` or `"off"`.
    pub confinement: Option<String>,
    /// Host paths confined processes may never read, in addition to the
    /// built-in list of credential stores (`~/.ssh`, `~/.aws`, keychains...).
    #[serde(default)]
    pub deny_read: Vec<PathBuf>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum Network {
    Mode(String),
    /// Old Wasmer ruleset strings; only recognised to explain the migration.
    Rules(#[allow(dead_code)] Vec<String>),
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
    /// A `.webc` file, or a directory containing `wasmer.toml` and `.wasm`
    /// modules (see `claustrum pkg add-wasm`). Relative to the packages
    /// directory unless absolute.
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
    /// Whether the guest may change the directory. Read-only by default.
    #[serde(default)]
    pub writable: bool,
}

/// Host actions the guest may trigger; see `claustrum_sandbox::action`.
#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ActionsSection {
    /// Guest command that triggers the actions. Defaults to `host`.
    pub command: Option<String>,
    #[serde(default, rename = "action")]
    pub list: Vec<ActionDef>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaudeSection {
    /// Path to the `claude` binary.
    pub binary: Option<PathBuf>,
    /// Built-in Claude Code tools to keep enabled (passed to `--tools`).
    /// Empty by default, which removes every built-in tool except
    /// `AskUserQuestion`, which only prompts the user. `WebSearch` is a
    /// reasonable addition since it runs on the API side, not on the host;
    /// `Agent` enables subagents and `Workflow` multi-agent workflows, whose
    /// agents only get the session's tools.
    #[serde(default)]
    pub tools: Vec<String>,
    /// Extra arguments always passed to `claude`.
    #[serde(default)]
    pub args: Vec<String>,
    /// Additional text appended to the system prompt.
    pub system_prompt: Option<String>,
    /// Plan mode: keep `EnterPlanMode`/`ExitPlanMode` and give Claude a plan
    /// directory of its own for this workspace. On by default.
    #[serde(default = "default_true")]
    pub plans: bool,
}

impl Default for ClaudeSection {
    fn default() -> Self {
        Self {
            binary: None,
            tools: Vec::new(),
            args: Vec::new(),
            system_prompt: None,
            plans: true,
        }
    }
}

fn default_true() -> bool {
    true
}

/// Fully resolved configuration.
#[derive(Debug)]
pub struct Config {
    pub file: FileConfig,
    pub path: Option<PathBuf>,
    pub packages_dir: PathBuf,
    /// Where the file came from; decides whether it needs the user's trust.
    pub source: Source,
    /// SHA-256 of the file as it was parsed, hex.
    pub digest: Option<String>,
}

/// What the confined worker reaches through the broker.
pub struct Remote {
    pub executor: Option<Arc<dyn ActionExecutor>>,
    pub plans: Option<Arc<dyn PlanStore>>,
}

/// Origin of the configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// `--config` or `CLAUSTRUM_CONFIG`: chosen by the user.
    Explicit,
    /// `./claustrum.toml`: part of the project, possibly a cloned one, so it
    /// is only used once the user trusted this exact content (see
    /// [`crate::trust`]).
    Workspace,
    /// The user configuration directory.
    User,
    /// No file; built-in defaults.
    BuiltIn,
}

/// `[claude] args` that would undo what `claustrum run` sets up: they come
/// after Claustrum's own arguments and win. Refused in every configuration
/// file; pass them after `--` on the command line if they are really meant.
const FORBIDDEN_CLAUDE_ARGS: &[&str] = &[
    "--tools",
    "--allowedTools",
    "--allowed-tools",
    "--disallowedTools",
    "--disallowed-tools",
    "--mcp-config",
    "--strict-mcp-config",
    "--permission-mode",
    "--dangerously-skip-permissions",
    "--allow-dangerously-skip-permissions",
    "--settings",
    "--setting-sources",
    "--add-dir",
    "--plugin-dir",
    "--plugin-url",
    "--system-prompt",
    "--system-prompt-file",
];

fn check_claude_args(args: &[String]) -> Result<()> {
    for a in args {
        let flag = a.split('=').next().unwrap_or(a);
        if FORBIDDEN_CLAUDE_ARGS.contains(&flag) {
            anyhow::bail!(
                "[claude] args: `{flag}` would override the sandbox set up by `claustrum run` and \
                 is not allowed in a configuration file; use `[claude] tools` for built-in \
                 tools, `--permission-mode` on the command line, or pass it after `--`"
            );
        }
    }
    Ok(())
}

/// Hex SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub fn load(explicit: Option<&Path>, packages_dir: Option<&Path>) -> Result<Config> {
    let (path, source) = match explicit {
        Some(p) => (Some(p.to_path_buf()), Source::Explicit),
        None => {
            let found = default_config_paths().into_iter().find(|p| p.is_file());
            let source = match &found {
                Some(p) if p.is_relative() => Source::Workspace,
                Some(_) => Source::User,
                None => Source::BuiltIn,
            };
            (found, source)
        }
    };
    let (file, digest) = match &path {
        Some(p) => {
            let text = std::fs::read_to_string(p)
                .with_context(|| format!("cannot read config {}", p.display()))?;
            let file: FileConfig =
                toml::from_str(&text).with_context(|| format!("invalid config {}", p.display()))?;
            check_claude_args(&file.claude.args)
                .with_context(|| format!("invalid config {}", p.display()))?;
            (file, Some(sha256_hex(text.as_bytes())))
        }
        None => (FileConfig::default(), None),
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
        source,
        digest,
    })
}

pub fn default_config_paths() -> Vec<PathBuf> {
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

/// Per-user directory for logs, plans and trust records;
/// `CLAUSTRUM_STATE_DIR` overrides it.
pub fn state_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("CLAUSTRUM_STATE_DIR") {
        return PathBuf::from(dir);
    }
    project_dirs()
        .map(|d| {
            d.state_dir()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| d.data_dir().to_path_buf())
        })
        .unwrap_or_else(|| PathBuf::from("."))
}

/// File name stem that identifies a workspace in the state directory:
/// `<directory name>-<hash of the path>`.
fn workspace_key(workspace: &Path) -> String {
    // SHA-256 rather than `DefaultHasher`, whose output may change with the
    // Rust release and would orphan logs, plan ledgers and caches.
    let hash = sha256_hex(workspace.as_os_str().as_encoded_bytes());
    format!("{}-{}", workspace_name(workspace), &hash[..16])
}

fn workspace_name(workspace: &Path) -> String {
    workspace
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "root".into())
}

/// The key of earlier versions, only used to find their files.
fn legacy_workspace_key(workspace: &Path) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    workspace.hash(&mut h);
    format!("{}-{:016x}", workspace_name(workspace), h.finish())
}

/// Claude Code's default plan directory (`plansDirectory` can only point
/// inside the project, which the guest controls, so Claustrum keeps the
/// default).
pub fn claude_plans_dir() -> PathBuf {
    claude_config_dir().join("plans")
}

/// Claude Code's configuration directory: sessions, credentials, plans.
pub fn claude_config_dir() -> PathBuf {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| expand_home(Path::new("~/.claude")))
}

pub fn expand_home(p: &Path) -> PathBuf {
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

    /// Every file Claustrum may read its configuration from. The guest must
    /// not be able to change or create any of them, or it could loosen its own
    /// sandbox for the next run: the file in the workspace (where `claustrum
    /// run` is usually started), the default search paths and the file that
    /// was actually loaded.
    ///
    /// The Claude Code settings in the workspace are protected for the same
    /// reason: Claude Code runs on the host, outside the sandbox, and executes
    /// the hooks, status line and helper commands configured there. Those
    /// next to the loaded file are protected as well: with `[sandbox]
    /// workspace` pointing elsewhere (e.g. `~`), the project directory the
    /// file comes from may still lie inside the mounted tree.
    pub fn protected_paths(&self, workspace: &Path) -> Vec<PathBuf> {
        let mut paths = vec![workspace.join("claustrum.toml")];
        for dir in self.claude_settings_dirs(workspace) {
            paths.extend(CLAUDE_SETTINGS.iter().map(|f| dir.join(f)));
        }
        paths.extend(default_config_paths());
        paths.extend(self.path.clone());
        paths
    }

    /// The `.claude` directories whose settings are protected: the
    /// workspace's and, if it lies inside the workspace, the one next to the
    /// loaded configuration file (the project directory when `[sandbox]
    /// workspace` points to a directory above it).
    pub fn claude_settings_dirs(&self, workspace: &Path) -> Vec<PathBuf> {
        let mut dirs = vec![workspace.join(".claude")];
        let project = self
            .path
            .as_deref()
            .and_then(Path::parent)
            .map(claustrum_confine::resolve);
        if let Some(dir) = project
            && dir != workspace
            && dir.starts_with(workspace)
        {
            dirs.push(dir.join(".claude"));
        }
        dirs
    }

    /// Guest command name for the actions.
    pub fn action_command(&self) -> &str {
        self.file
            .actions
            .command
            .as_deref()
            .unwrap_or(action::DEFAULT_COMMAND)
    }

    /// Check the action definitions against the workspace without building
    /// the sandbox, so that `claustrum run` fails before `claude` starts.
    /// Warnings about risky definitions are logged by `compile_all`.
    pub fn validate_actions(&self, workspace: &Path) -> Result<Vec<claustrum_sandbox::ActionSpec>> {
        let policy = self.policy(workspace)?;
        action::compile_all(
            &self.file.actions.list,
            &CompileContext {
                workspace,
                default_timeout: policy.default_timeout,
                max_output_bytes: policy.max_output_bytes,
                resolve_programs: true,
            },
        )
        .map_err(|e| anyhow::anyhow!("invalid [actions] configuration: {e}"))
    }

    /// One line for stderr saying which actions are live and whether they
    /// run in the OS sandbox, so that every start shows what the sandbox can
    /// reach on the host.
    pub fn actions_notice(
        specs: &[claustrum_sandbox::ActionSpec],
        confined: bool,
    ) -> Option<String> {
        if specs.is_empty() {
            return None;
        }
        Some(format!(
            "host actions enabled: {}",
            specs
                .iter()
                .map(|s| {
                    let how = if confined && s.confine == action::Confine::Os {
                        "confined"
                    } else {
                        "UNCONFINED, full user rights"
                    };
                    format!("{} → {} ({how})", s.name, s.command_line())
                })
                .collect::<Vec<_>>()
                .join("; ")
        ))
    }

    /// One line for stderr describing the second sandbox layer.
    pub fn confinement_notice(confined: bool) -> String {
        match claustrum_confine::backend() {
            Ok(backend) if confined => {
                format!("confinement: {backend} around the Wasmer worker and the host actions")
            }
            _ => "confinement: OFF, the Wasmer runtime and host actions run with your full \
                  rights"
                .to_owned(),
        }
    }

    /// The `[sandbox] confinement` settings.
    pub fn confinement(&self) -> Result<Confinement> {
        let s = &self.file.sandbox;
        let mode = match &s.confinement {
            Some(m) => m
                .parse::<ConfinementMode>()
                .map_err(|e| anyhow::anyhow!("[sandbox] {e}"))?,
            None => ConfinementMode::BestEffort,
        };
        let mut deny_read = std::env::var_os("HOME")
            .map(|h| claustrum_confine::secret_paths(Path::new(&h)))
            .unwrap_or_default();
        deny_read.extend(s.deny_read.iter().map(|p| expand_home(p)));
        Ok(Confinement { mode, deny_read })
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

    /// The network mode from `[network].mode` or the `[sandbox].network`
    /// shorthand.
    pub fn network_mode(&self) -> Result<NetMode> {
        let section = &self.file.network;
        match (&self.file.sandbox.network, &section.mode) {
            (Some(_), Some(_)) => anyhow::bail!(
                "set the network mode either in [network] mode or in [sandbox] network, not both"
            ),
            (Some(Network::Rules(_)), None) => anyhow::bail!(
                "[sandbox] network no longer takes Wasmer ruleset strings; use [network] \
                 mode = \"allowlist\" with allow = [\"host:port\", ...] instead"
            ),
            (Some(Network::Mode(m)), None) => match m.as_str() {
                "disabled" | "none" => Ok(NetMode::Disabled),
                "host" => Ok(NetMode::Host),
                other => anyhow::bail!(
                    "[sandbox] network = \"{other}\" is not supported; use \"disabled\", \"host\" \
                     or the [network] section"
                ),
            },
            (None, Some(m)) => m.parse().map_err(anyhow::Error::msg),
            (None, None) => Ok(NetMode::Disabled),
        }
    }

    /// Where the network decisions for `workspace` are logged.
    pub fn network_log_path(&self, workspace: &Path) -> PathBuf {
        if let Some(p) = &self.file.network.log {
            let p = expand_home(p);
            return if p.is_relative() {
                workspace.join(p)
            } else {
                p
            };
        }
        state_dir()
            .join("network")
            .join(format!("{}.jsonl", workspace_key(workspace)))
    }

    /// Rename the network log and plan ledger of `workspace` from the file
    /// names of earlier versions (see [`workspace_key`]). Best effort, run by
    /// the unconfined commands before they use them.
    pub fn migrate_state(&self, workspace: &Path) {
        let (new, old) = (workspace_key(workspace), legacy_workspace_key(workspace));
        if new == old {
            return;
        }
        let dir = state_dir();
        let mut moves = vec![(
            dir.join("plans").join(format!("{old}.list")),
            dir.join("plans").join(format!("{new}.list")),
        )];
        if self.file.network.log.is_none() {
            moves.push((
                dir.join("network").join(format!("{old}.jsonl")),
                dir.join("network").join(format!("{new}.jsonl")),
            ));
        }
        for (from, to) in moves {
            if from.is_file() && !to.exists() {
                match std::fs::rename(&from, &to) {
                    Ok(()) => {
                        tracing::info!(from = %from.display(), to = %to.display(), "renamed state file")
                    }
                    Err(e) => {
                        tracing::warn!(from = %from.display(), error = %e, "cannot rename state file")
                    }
                }
            }
        }
    }

    /// The cache the confined worker of `workspace` may write: its compiled
    /// modules, downloaded packages and the host command shim. The user-wide
    /// cache is only written by unconfined processes (`claustrum pkg`, `serve`
    /// without confinement) and read by the worker, so a compromised worker
    /// cannot plant native code for other workspaces or for those processes.
    pub fn worker_cache_dir(&self, workspace: &Path) -> PathBuf {
        RuntimeConfig::default()
            .cache_dir
            .join("workspaces")
            .join(workspace_key(workspace))
    }

    /// Where Claude Code's plans for `workspace` are written, if plan mode
    /// is enabled: Claude Code's own plan directory, with a ledger in the
    /// user state directory of the files this workspace created there.
    pub fn host_plans(&self, workspace: &Path) -> Option<HostPlans> {
        if !self.file.claude.plans {
            return None;
        }
        Some(HostPlans::new(
            claude_plans_dir(),
            state_dir()
                .join("plans")
                .join(format!("{}.list", workspace_key(workspace))),
        ))
    }

    pub fn network_policy(&self, workspace: &Path) -> Result<NetworkPolicy> {
        let mode = self.network_mode()?;
        let allow = NetPolicy::parse_entries(&self.file.network.allow)
            .map_err(|e| anyhow::anyhow!("[network] allow: {e}"))?;
        if !allow.is_empty() && matches!(mode, NetMode::Disabled | NetMode::Host) {
            tracing::warn!(
                mode = mode.as_str(),
                "[network] allow has no effect in this mode; use \"allowlist\" or \"audit\""
            );
        }
        Ok(NetworkPolicy {
            mode,
            allow,
            log: Some(self.network_log_path(workspace)),
        })
    }

    /// One line for stderr describing network access.
    pub fn network_notice(policy: &NetworkPolicy) -> String {
        let entries: Vec<String> = policy.allow.iter().map(ToString::to_string).collect();
        let log = policy
            .log
            .as_ref()
            .map(|p| format!("; log: {}", p.display()))
            .unwrap_or_default();
        match policy.mode {
            NetMode::Disabled => format!("network: disabled{log}"),
            NetMode::Host => format!("network: unrestricted (host mode){log}"),
            NetMode::Allowlist if entries.is_empty() => {
                format!("network: allowlist with no entries, nothing is reachable{log}")
            }
            NetMode::Allowlist => format!("network: allowlist {}{log}", entries.join(", ")),
            NetMode::Audit => format!(
                "network: audit (everything allowed and logged; reference allowlist: {}){log}",
                if entries.is_empty() {
                    "empty".to_owned()
                } else {
                    entries.join(", ")
                }
            ),
        }
    }

    pub fn policy(&self, workspace: &Path) -> Result<Policy> {
        let s = &self.file.sandbox;
        let mut policy = Policy {
            network: self.network_policy(workspace)?,
            ..Policy::default()
        };
        if let Some(secs) = s.timeout_secs {
            policy.default_timeout = (secs > 0).then(|| Duration::from_secs(secs));
        }
        if let Some(bytes) = s.max_output_bytes {
            policy.max_output_bytes = bytes;
        }
        if let Some(threads) = s.max_threads {
            policy.max_threads = Some(threads);
        }
        if let Some(mb) = s.max_memory_mb {
            policy.max_memory_bytes = (mb > 0).then(|| mb * 1024 * 1024);
        }
        policy.confinement = self.confinement()?;
        Ok(policy)
    }

    /// Build the sandbox described by this configuration.
    ///
    /// With `remote` the sandbox runs in the confined worker: host actions
    /// and plan files go through the broker, and compiled modules are cached
    /// per workspace (see [`Config::worker_cache_dir`]). Without it plan
    /// files are written directly to [`Config::host_plans`].
    pub async fn build_sandbox(
        &self,
        workspace: Option<&Path>,
        remote: Option<Remote>,
    ) -> Result<Sandbox> {
        self.build(workspace, remote, true).await
    }

    /// A sandbox with the packages only, for `claustrum pkg`: no host
    /// actions, so a configuration written for a machine with `cargo` can
    /// still sync packages on one without it.
    pub async fn build_package_sandbox(&self) -> Result<Sandbox> {
        self.build(Some(&std::env::temp_dir()), None, false).await
    }

    async fn build(
        &self,
        workspace: Option<&Path>,
        remote: Option<Remote>,
        with_actions: bool,
    ) -> Result<Sandbox> {
        let workspace = self.workspace(workspace)?;
        let (executor, plans, runtime) = match remote {
            Some(r) => (
                r.executor,
                r.plans,
                RuntimeConfig {
                    cache_dir: self.worker_cache_dir(&workspace),
                    shared_modules: Some(RuntimeConfig::default().cache_dir.join("modules")),
                    online: self.file.packages.online,
                    ..RuntimeConfig::default()
                },
            ),
            None => (
                None,
                None,
                RuntimeConfig {
                    online: self.file.packages.online,
                    ..RuntimeConfig::default()
                },
            ),
        };
        let mut builder: SandboxBuilder = Sandbox::builder()
            .workspace(&workspace)
            .policy(self.policy(&workspace)?)
            .runtime_config(runtime);

        let packages = self.packages();
        let missing: Vec<_> = packages.iter().filter(|p| !p.is_installed()).collect();
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
            if p.file.is_dir() {
                builder = builder.package_dir(&p.file);
                continue;
            }
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
            builder = if m.writable {
                builder.mount_writable(&m.guest, expand_home(&m.host))
            } else {
                builder.mount(&m.guest, expand_home(&m.host))
            };
        }
        if let Some(host) = self.host_plans(&workspace) {
            let dir = host.dir().to_string_lossy().into_owned();
            let store = plans.unwrap_or_else(|| Arc::new(host));
            builder = builder.plans(dir, store);
        }
        for p in self.protected_paths(&workspace) {
            builder = builder.protect(p);
        }
        if with_actions {
            builder = builder
                .action_command(self.action_command())
                .actions(self.file.actions.list.iter().cloned());
        }
        if let Some(executor) = executor {
            builder = builder.action_executor(executor);
        }
        Ok(builder.build().await?)
    }
}

impl PackageEntry {
    /// True when the `.webc` file exists, or the directory holds a manifest.
    pub fn is_installed(&self) -> bool {
        self.file.is_file() || self.file.join("wasmer.toml").is_file()
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The bundled `packages/*.id` (used by the tests) name the pinned defaults.
    #[test]
    fn default_packages_match_the_bundled_ids() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages");
        for (source, file) in DEFAULT_PACKAGES {
            let id = read_stamp(&dir.join(file));
            assert_eq!(
                id.as_deref(),
                Some(source.replace("@=", "@").as_str()),
                "{file}"
            );
        }
    }

    #[test]
    fn parses_actions() {
        let cfg: FileConfig = toml::from_str(
            r#"
[actions]
command = "run"

[[actions.action]]
name = "test"
command = ["cargo", "test"]

[[actions.action]]
name = "say"
command = ["/bin/echo", "{word}"]
[[actions.action.input]]
name = "word"
pattern = "[a-z]+"
"#,
        )
        .unwrap();
        assert_eq!(cfg.actions.command.as_deref(), Some("run"));
        assert_eq!(cfg.actions.list.len(), 2);
        assert_eq!(cfg.actions.list[1].inputs[0].name, "word");

        let err = toml::from_str::<FileConfig>("[[actions.action]]\nname = \"x\"\nshell = true\n")
            .unwrap_err();
        assert!(err.to_string().contains("shell"), "{err}");
    }

    #[test]
    fn mounts_are_read_only_by_default() {
        let cfg: FileConfig = toml::from_str(
            r#"
[[mounts]]
guest = "/data"
host = "/srv/data"

[[mounts]]
guest = "/out"
host = "/srv/out"
writable = true
"#,
        )
        .unwrap();
        assert!(!cfg.mounts[0].writable);
        assert!(cfg.mounts[1].writable);
    }

    fn config(text: &str) -> Config {
        Config {
            file: toml::from_str(text).unwrap(),
            path: None,
            packages_dir: PathBuf::from("."),
            source: Source::BuiltIn,
            digest: None,
        }
    }

    #[test]
    fn network_modes_and_aliases() {
        let ws = Path::new("/tmp/ws");
        let p = config("").network_policy(ws).unwrap();
        assert_eq!(p.mode, NetMode::Disabled);
        assert!(p.log.unwrap().to_string_lossy().contains("ws-"));

        assert_eq!(
            config("[sandbox]\nnetwork = \"host\"\n")
                .network_mode()
                .unwrap(),
            NetMode::Host
        );
        assert_eq!(
            config("[sandbox]\nnetwork = \"disabled\"\n")
                .network_mode()
                .unwrap(),
            NetMode::Disabled
        );
        let p = config(
            "[network]\nmode = \"allowlist\"\nallow = [\"crates.io\", \"10.0.0.5:5432\"]\nlog = \"net.jsonl\"\n",
        )
        .network_policy(ws)
        .unwrap();
        assert_eq!(p.mode, NetMode::Allowlist);
        assert_eq!(p.allow.len(), 2);
        assert_eq!(p.log.as_deref(), Some(Path::new("/tmp/ws/net.jsonl")));
        assert!(Config::network_notice(&p).contains("crates.io:443, 10.0.0.5:5432"));

        for (text, expected) in [
            (
                "[sandbox]\nnetwork = \"host\"\n[network]\nmode = \"audit\"\n",
                "not both",
            ),
            (
                "[sandbox]\nnetwork = [\"dns:allow=*.crates.io:443\"]\n",
                "no longer takes Wasmer ruleset",
            ),
            ("[sandbox]\nnetwork = \"allowlist\"\n", "not supported"),
            ("[network]\nmode = \"open\"\n", "unknown network mode"),
            (
                "[network]\nmode = \"allowlist\"\nallow = [\"bad host\"]\n",
                "invalid network entry",
            ),
        ] {
            let err = config(text).network_policy(ws).unwrap_err();
            assert!(err.to_string().contains(expected), "{text}: {err}");
        }
        assert!(toml::from_str::<FileConfig>("[network]\nallowed = []\n").is_err());
    }

    #[test]
    fn plan_ledgers_are_per_workspace() {
        let a = config("").host_plans(Path::new("/tmp/a/ws")).unwrap();
        let b = config("").host_plans(Path::new("/tmp/b/ws")).unwrap();
        assert_eq!(a.dir(), b.dir());
        assert!(a.dir().ends_with("plans"));
        assert_ne!(format!("{a:?}"), format!("{b:?}"));
        assert!(
            config("[claude]\nplans = false\n")
                .host_plans(Path::new("/tmp/a/ws"))
                .is_none()
        );
    }

    #[test]
    fn claude_args_cannot_undo_the_sandbox() {
        for bad in [
            "--tools",
            "--tools=Bash",
            "--dangerously-skip-permissions",
            "--settings",
            "--mcp-config",
            "--allowedTools",
        ] {
            let err = check_claude_args(&[bad.to_owned()]).unwrap_err();
            assert!(err.to_string().contains("not allowed"), "{bad}: {err}");
        }
        check_claude_args(&["--model".into(), "opus".into(), "--verbose".into()]).unwrap();
    }

    #[test]
    fn workspace_keys_are_stable() {
        // Pinned: a changed key would orphan logs, ledgers and caches.
        assert_eq!(
            workspace_key(Path::new("/home/u/proj")),
            format!("proj-{}", &sha256_hex(b"/home/u/proj")[..16])
        );
        assert_eq!(
            workspace_key(Path::new("/")),
            format!("root-{}", &sha256_hex(b"/")[..16])
        );
    }

    #[test]
    fn claude_settings_are_protected() {
        let ws = Path::new("/tmp/ws");
        let paths = config("").protected_paths(ws);
        assert!(paths.contains(&ws.join(".claude/settings.json")));
        assert!(paths.contains(&ws.join(".claude/settings.local.json")));

        // The project the file comes from, when the workspace lies above it.
        let tmp = tempfile::tempdir().unwrap();
        let home = claustrum_confine::resolve(tmp.path());
        let proj = home.join("proj");
        std::fs::create_dir(&proj).unwrap();
        let mut cfg = config("[sandbox]\nworkspace = \"~\"\n");
        cfg.path = Some(proj.join("claustrum.toml"));
        let paths = cfg.protected_paths(&home);
        for f in CLAUDE_SETTINGS {
            assert!(paths.contains(&proj.join(".claude").join(f)), "{paths:?}");
            assert!(paths.contains(&home.join(".claude").join(f)), "{paths:?}");
        }
        assert!(paths.contains(&proj.join("claustrum.toml")));

        // A configuration outside the workspace adds nothing.
        let dirs = cfg.claude_settings_dirs(&proj.join("sub"));
        assert_eq!(dirs, [proj.join("sub/.claude")]);
    }

    #[test]
    fn validate_actions_reports_bad_definitions() {
        let ws = tempfile::tempdir().unwrap();
        let ws = ws.path().canonicalize().unwrap();
        let file: FileConfig = toml::from_str(
            "[[actions.action]]\nname = \"x\"\ncommand = [\"/bin/echo\", \"{nope}\"]\n",
        )
        .unwrap();
        let config = Config {
            file,
            path: None,
            packages_dir: ws.clone(),
            source: Source::BuiltIn,
            digest: None,
        };
        let err = config.validate_actions(&ws).unwrap_err();
        assert!(err.to_string().contains("no matching input"), "{err}");
        assert_eq!(config.action_command(), "host");
    }
}
