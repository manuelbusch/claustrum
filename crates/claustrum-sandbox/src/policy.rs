//! Sandbox policy: what the guest may reach and how much it may consume.

use std::{path::PathBuf, time::Duration};

use crate::net::{AllowEntry, NetMode};

/// Network access for guest processes and host actions. See [`crate::net`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NetworkPolicy {
    pub mode: NetMode,
    /// Destinations reachable in `allowlist` mode; the reference for `audit`.
    pub allow: Vec<AllowEntry>,
    /// Append every decision to this JSONL file as well.
    pub log: Option<PathBuf>,
}

impl NetworkPolicy {
    pub fn disabled() -> Self {
        Self::default()
    }

    pub fn host() -> Self {
        Self {
            mode: NetMode::Host,
            ..Self::default()
        }
    }

    /// Allowlist mode with entries such as `crates.io:443`.
    pub fn allowlist<S: AsRef<str>>(allow: &[S]) -> Result<Self, String> {
        Ok(Self {
            mode: NetMode::Allowlist,
            allow: crate::net::NetPolicy::parse_entries(allow)?,
            log: None,
        })
    }
}

/// How much of the operating system sandbox (the second layer around WASIX)
/// Claustrum uses for its host processes. See the `claustrum-confine` crate.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ConfinementMode {
    /// Refuse to start when the platform has no confinement backend.
    Required,
    /// Confine where possible, warn loudly where not.
    BestEffort,
    /// Never confine. Host actions run with the user's full rights.
    #[default]
    Off,
}

impl ConfinementMode {
    pub fn as_str(self) -> &'static str {
        match self {
            ConfinementMode::Required => "required",
            ConfinementMode::BestEffort => "best-effort",
            ConfinementMode::Off => "off",
        }
    }
}

impl std::str::FromStr for ConfinementMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "required" => Ok(ConfinementMode::Required),
            "best-effort" => Ok(ConfinementMode::BestEffort),
            "off" => Ok(ConfinementMode::Off),
            other => Err(format!(
                "confinement `{other}` is unknown; use \"required\", \"best-effort\" or \"off\""
            )),
        }
    }
}

/// OS confinement settings.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Confinement {
    pub mode: ConfinementMode,
    /// Host paths confined processes may never read, even where a broader
    /// rule allows reading (credentials, keychains, browser profiles).
    pub deny_read: Vec<PathBuf>,
}

impl Confinement {
    /// Whether processes are confined, after checking the platform. `Err`
    /// only in `required` mode without a backend.
    pub fn active(&self) -> Result<bool, String> {
        if self.mode == ConfinementMode::Off {
            return Ok(false);
        }
        match claustrum_confine::backend() {
            Ok(_) => Ok(true),
            Err(e) if self.mode == ConfinementMode::Required => Err(format!(
                "confinement = \"required\" but OS confinement is unavailable: {e}"
            )),
            Err(e) => {
                tracing::warn!(
                    reason = %e,
                    "OS confinement is unavailable; host processes run unconfined"
                );
                Ok(false)
            }
        }
    }
}

/// Default for [`Policy::max_memory_bytes`]: 1 GiB per guest process.
pub const DEFAULT_MAX_MEMORY_BYTES: u64 = 1024 * 1024 * 1024;

/// Resource limits and capabilities for guest processes.
#[derive(Clone, Debug)]
pub struct Policy {
    pub network: NetworkPolicy,
    /// Maximum bytes retained from each of stdout and stderr per command.
    pub max_output_bytes: usize,
    /// Default wall-clock timeout for a command. `None` means no limit.
    pub default_timeout: Option<Duration>,
    /// Maximum number of guest tasks per command: WASIX counts processes
    /// and threads of one command's process tree together.
    pub max_threads: Option<u32>,
    /// Largest linear memory a guest process may grow to (bytes). `None`
    /// allows wasm32's 4 GiB.
    pub max_memory_bytes: Option<u64>,
    /// OS confinement of host actions.
    pub confinement: Confinement,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            network: NetworkPolicy::disabled(),
            max_output_bytes: 1024 * 1024,
            default_timeout: Some(Duration::from_secs(120)),
            max_threads: Some(64),
            max_memory_bytes: Some(DEFAULT_MAX_MEMORY_BYTES),
            confinement: Confinement::default(),
        }
    }
}
