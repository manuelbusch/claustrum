//! Network access control.
//!
//! All network traffic Claustrum starts passes one policy gate:
//!
//! - guest sockets through [`FilteredNetworking`], a `VirtualNetworking`
//!   wrapper the guest cannot bypass;
//! - host actions through [`ActionProxy`], a local HTTP CONNECT proxy the
//!   actions are pointed at with `HTTP(S)_PROXY`. Confined actions cannot
//!   reach anything else; unconfined ones could ignore those variables.
//!
//! Both ask the same [`NetPolicy`] and record into the same
//! [`ConnectionLog`].

mod filter;
mod log;
mod policy;
mod proxy;

pub use filter::{FilteredNetworking, GUEST};
pub use log::{ConnectionLog, Event, LogEntry, read_log};
pub use policy::{
    AllowEntry, DEFAULT_PORT, HostSpec, NetMode, NetPolicy, Ports, Verdict, is_special,
    normalize_name,
};
pub use proxy::{ActionProxy, ProxyHandle};

/// How network access looks from Claude's side, for system prompts and
/// server instructions.
pub fn describe_for_model(mode: NetMode, allow: &[AllowEntry]) -> String {
    let list = allow
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    match mode {
        NetMode::Disabled => "There is no network access: every connection attempt is refused. \
             If a task needs the network, ask the user to allow the destination in \
             claustrum.toml ([network] mode and allow)."
            .into(),
        NetMode::Host => "Network access is unrestricted.".into(),
        NetMode::Allowlist if allow.is_empty() => {
            "Network access is limited to an empty allowlist, so nothing is reachable. If a \
             task needs a destination, ask the user to add it to [network] allow in \
             claustrum.toml."
                .into()
        }
        NetMode::Allowlist => format!(
            "Network access is limited to these destinations (host:port): {list}. Everything \
             else is refused, as are UDP, listening sockets and connections to local or private \
             addresses not listed. Refusals are reported at the end of the tool result as \
             `[network: refused ...]`; do not try to work around them, ask the user to add the \
             destination to [network] allow in claustrum.toml instead."
        ),
        NetMode::Audit => "Network access is open but every connection is logged for review \
             by the user."
            .into(),
    }
}
