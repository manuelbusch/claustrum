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

/// Resource limits and capabilities for guest processes.
#[derive(Clone, Debug)]
pub struct Policy {
    pub network: NetworkPolicy,
    /// Maximum bytes retained from each of stdout and stderr per command.
    pub max_output_bytes: usize,
    /// Default wall-clock timeout for a command. `None` means no limit.
    pub default_timeout: Option<Duration>,
    /// Maximum number of guest threads per process.
    pub max_threads: Option<u32>,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            network: NetworkPolicy::disabled(),
            max_output_bytes: 1024 * 1024,
            default_timeout: Some(Duration::from_secs(120)),
            max_threads: Some(64),
        }
    }
}
