//! Sandbox policy: what the guest may reach and how much it may consume.

use std::time::Duration;

/// Network access granted to guest processes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum NetworkPolicy {
    /// All socket operations fail. This is the default.
    #[default]
    Disabled,
    /// Unrestricted access to the host network.
    Host,
    /// Host network filtered by a Wasmer ruleset, e.g.
    /// `dns:allow=*.example.com:443`. See `virtual_net::ruleset`.
    Ruleset(Vec<String>),
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
            network: NetworkPolicy::Disabled,
            max_output_bytes: 1024 * 1024,
            default_timeout: Some(Duration::from_secs(120)),
            max_threads: Some(64),
        }
    }
}
