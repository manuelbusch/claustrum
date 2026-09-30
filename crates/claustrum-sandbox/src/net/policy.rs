//! The network allowlist and the decisions made from it.
//!
//! Entries are `host[:ports]`: a DNS name, `*.name` (subdomains only), an IP
//! address or a CIDR block, with ports `443`, `80,443`, `8000-8100` or `*`
//! (default 443). Names are checked when the guest resolves them; the
//! addresses the resolver returns become *grants* for exactly the ports of
//! the matching entries. A connection is allowed when an IP entry or a grant
//! covers its address and port, so naming a host never opens arbitrary IPs.
//! Loopback, private, link-local and similar addresses are refused unless an
//! IP entry names them, which defeats DNS rebinding and keeps the guest away
//! from services on the host and cloud metadata endpoints.

use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    str::FromStr,
    sync::RwLock,
};

use ipnet::IpNet;

/// How the gate behaves.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NetMode {
    /// Every socket operation fails.
    #[default]
    Disabled,
    /// Only destinations in the allowlist are reachable.
    Allowlist,
    /// Everything is reachable; what the allowlist would refuse is logged.
    Audit,
    /// Everything is reachable and logged.
    Host,
}

impl NetMode {
    pub fn as_str(self) -> &'static str {
        match self {
            NetMode::Disabled => "disabled",
            NetMode::Allowlist => "allowlist",
            NetMode::Audit => "audit",
            NetMode::Host => "host",
        }
    }
}

impl FromStr for NetMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "disabled" | "none" => Ok(NetMode::Disabled),
            "allowlist" => Ok(NetMode::Allowlist),
            "audit" => Ok(NetMode::Audit),
            "host" => Ok(NetMode::Host),
            other => Err(format!(
                "unknown network mode `{other}`; use disabled, allowlist, audit or host"
            )),
        }
    }
}

/// Port set of an entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ports {
    All,
    Ranges(Vec<(u16, u16)>),
}

impl Ports {
    pub fn contains(&self, port: u16) -> bool {
        match self {
            Ports::All => true,
            Ports::Ranges(r) => r.iter().any(|(lo, hi)| (*lo..=*hi).contains(&port)),
        }
    }

    fn parse(s: &str) -> Result<Self, String> {
        if s == "*" {
            return Ok(Ports::All);
        }
        let mut ranges = Vec::new();
        for part in s.split(',') {
            let part = part.trim();
            let (lo, hi) = match part.split_once('-') {
                Some((lo, hi)) => (lo, hi),
                None => (part, part),
            };
            let lo: u16 = lo.parse().map_err(|_| format!("invalid port `{part}`"))?;
            let hi: u16 = hi.parse().map_err(|_| format!("invalid port `{part}`"))?;
            if lo == 0 || lo > hi {
                return Err(format!("invalid port range `{part}`"));
            }
            ranges.push((lo, hi));
        }
        Ok(Ports::Ranges(ranges))
    }
}

impl std::fmt::Display for Ports {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Ports::All => f.write_str("*"),
            Ports::Ranges(r) => {
                let parts: Vec<String> = r
                    .iter()
                    .map(|(lo, hi)| {
                        if lo == hi {
                            lo.to_string()
                        } else {
                            format!("{lo}-{hi}")
                        }
                    })
                    .collect();
                f.write_str(&parts.join(","))
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostSpec {
    /// Exact DNS name, lowercase, without trailing dot.
    Name(String),
    /// `*.suffix`: any name strictly below `suffix`.
    Subdomains(String),
    /// Single address or CIDR block.
    Net(IpNet),
}

/// One `allow` entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AllowEntry {
    pub host: HostSpec,
    pub ports: Ports,
}

/// Port used when an entry does not name one.
pub const DEFAULT_PORT: u16 = 443;

impl FromStr for AllowEntry {
    type Err = String;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let s = raw.trim();
        let err = |m: &str| format!("invalid network entry `{raw}`: {m}");
        if s.is_empty() {
            return Err(err("empty"));
        }
        // Split host and ports. IPv6 needs brackets to carry a port.
        let (host, ports) = if let Some(rest) = s.strip_prefix('[') {
            let (host, after) = rest.split_once(']').ok_or_else(|| err("missing `]`"))?;
            match after {
                "" => (host, None),
                p => (
                    host,
                    Some(
                        p.strip_prefix(':')
                            .ok_or_else(|| err("expected `:` after `]`"))?,
                    ),
                ),
            }
        } else if s.matches(':').count() > 1 {
            (s, None)
        } else {
            match s.split_once(':') {
                Some((h, p)) => (h, Some(p)),
                None => (s, None),
            }
        };
        let ports = match ports {
            None => Ports::Ranges(vec![(DEFAULT_PORT, DEFAULT_PORT)]),
            Some(p) => Ports::parse(p).map_err(|m| err(&m))?,
        };
        let host = parse_host(host).map_err(|m| err(&m))?;
        Ok(AllowEntry { host, ports })
    }
}

fn parse_host(h: &str) -> Result<HostSpec, String> {
    // Addresses are compared in canonical form (see `canonical_ip`), so an
    // IPv4-mapped entry is stored as the IPv4 address or net it carries.
    if let Ok(ip) = h.parse::<IpAddr>() {
        return Ok(HostSpec::Net(IpNet::from(canonical_ip(ip))));
    }
    if let Ok(net) = h.parse::<IpNet>() {
        return Ok(HostSpec::Net(canonical_net(net.trunc())));
    }
    let lower = h.trim_end_matches('.').to_ascii_lowercase();
    let (wild, name) = match lower.strip_prefix("*.") {
        Some(rest) => (true, rest.to_owned()),
        None => (false, lower),
    };
    if !is_dns_name(&name) {
        return Err(format!("`{h}` is not a DNS name, IP address or CIDR block"));
    }
    Ok(if wild {
        HostSpec::Subdomains(name)
    } else {
        HostSpec::Name(name)
    })
}

/// Whether `s` is a DNS name: dot-separated labels of letters, digits, `-`
/// and `_`.
pub fn is_dns_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 253
        && s.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        })
}

/// Normalise a name for matching.
pub fn normalize_name(name: &str) -> String {
    name.trim_end_matches('.').to_ascii_lowercase()
}

/// Whether the guest may look `name` up at all: an IP address or a DNS
/// name. Anything else (control characters, spaces, quotes) could only
/// serve to forge lines in logs, notes and reports.
pub fn is_query_name(name: &str) -> bool {
    let name = normalize_name(name);
    name.parse::<IpAddr>().is_ok() || is_dns_name(&name)
}

impl AllowEntry {
    fn matches_name(&self, name: &str) -> bool {
        match &self.host {
            HostSpec::Name(n) => n == name,
            HostSpec::Subdomains(suffix) => name
                .strip_suffix(suffix.as_str())
                .is_some_and(|prefix| prefix.len() > 1 && prefix.ends_with('.')),
            HostSpec::Net(_) => false,
        }
    }

    fn covers_ip(&self, ip: IpAddr, port: u16) -> bool {
        match &self.host {
            HostSpec::Net(net) => net.contains(&canonical_ip(ip)) && self.ports.contains(port),
            _ => false,
        }
    }
}

impl std::fmt::Display for AllowEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.host {
            HostSpec::Name(n) => write!(f, "{n}")?,
            HostSpec::Subdomains(n) => write!(f, "*.{n}")?,
            HostSpec::Net(net) if net.prefix_len() == net.max_prefix_len() => match net.addr() {
                IpAddr::V6(a) => write!(f, "[{a}]")?,
                a => write!(f, "{a}")?,
            },
            HostSpec::Net(IpNet::V6(n)) => write!(f, "[{n}]")?,
            HostSpec::Net(n) => write!(f, "{n}")?,
        }
        write!(f, ":{}", self.ports)
    }
}

/// A net inside `::ffff:0:0/96` as the IPv4 net it maps.
fn canonical_net(net: IpNet) -> IpNet {
    match net {
        IpNet::V6(v6) if v6.prefix_len() >= 96 => match v6.addr().to_ipv4_mapped() {
            Some(v4) => ipnet::Ipv4Net::new(v4, v6.prefix_len() - 96)
                .map(IpNet::V4)
                .unwrap_or(net),
            None => net,
        },
        other => other,
    }
}

/// IPv4-mapped IPv6 addresses are treated as the IPv4 address they carry.
fn canonical_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6
            .to_ipv4_mapped()
            .map(IpAddr::V4)
            .unwrap_or(IpAddr::V6(v6)),
        v4 => v4,
    }
}

/// Addresses that must not be reached through a name: loopback, private,
/// link-local (incl. cloud metadata), shared, unspecified, multicast,
/// broadcast, documentation and reserved ranges.
pub fn is_special(ip: IpAddr) -> bool {
    match canonical_ip(ip) {
        IpAddr::V4(v4) => is_special_v4(v4),
        IpAddr::V6(v6) => is_special_v6(v6),
    }
}

fn is_special_v4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_multicast()
        || ip.is_broadcast()
        || ip.is_documentation()
        || o[0] == 0
        || (o[0] == 100 && (64..128).contains(&o[1])) // shared / CGNAT
        || (o[0] == 192 && o[1] == 0 && o[2] == 0) // IETF protocol assignments
        || (o[0] == 192 && o[1] == 88 && o[2] == 99) // 6to4 relay anycast (deprecated)
        || (o[0] == 198 && (18..20).contains(&o[1])) // benchmarking
        || o[0] >= 240 // reserved
}

fn is_special_v6(ip: Ipv6Addr) -> bool {
    let s = ip.segments();
    ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || (s[0] & 0xfe00) == 0xfc00 // unique local
        || (s[0] & 0xffc0) == 0xfe80 // link local
        || (s[0] == 0x2001 && s[1] == 0x0db8) // documentation
        || (s[0] & 0xfff0) == 0x3ff0 // documentation (3fff::/20)
        || (s[0] == 0x2001 && s[1] == 0x0002 && s[2] == 0) // benchmarking
        || (s[0] & 0xffc0) == 0xfec0 // deprecated site local
        || (s[0] == 0x0100 && s[1..4] == [0, 0, 0]) // discard-only
        // Transition ranges embed an IPv4 address and can reach private IPv4
        // space through a relay or translator.
        || (s[0] == 0x0064 && s[1] == 0xff9b) // NAT64 (also 64:ff9b:1::/48)
        || s[0] == 0x2002 // 6to4
        || (s[0] == 0x2001 && s[1] == 0x0000) // Teredo
        || s[..6] == [0; 6] // IPv4-compatible (deprecated), ::/96
        || s[..6] == [0, 0, 0, 0, 0xffff, 0] // IPv4-translated (SIIT), ::ffff:0:0:0/96
        || ((s[4] & 0xfdff) == 0 && s[5] == 0x5efe) // ISATAP interface identifier
}

/// Why something was refused, for logs and for the model.
pub type Reason = String;

/// What the gate concluded for one operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    Allowed,
    /// Allowed only because of audit mode; the allowlist would refuse it.
    Audited(Reason),
    Refused(Reason),
}

impl Verdict {
    pub fn allowed(&self) -> bool {
        !matches!(self, Verdict::Refused(_))
    }
}

/// How long an address stays granted after a name resolved to it: longer
/// than one Bash call may run (10 minutes), so a program that resolved once
/// can still connect later in the same call. Resolving again renews it.
const GRANT_TTL: std::time::Duration = std::time::Duration::from_secs(15 * 60);

/// Ports granted on an address, the name it was resolved from, and when.
type Grant = (Ports, String, std::time::Instant);

/// The allowlist plus the grants learned from resolved names.
#[derive(Debug)]
pub struct NetPolicy {
    mode: NetMode,
    entries: Vec<AllowEntry>,
    grants: RwLock<HashMap<IpAddr, Vec<Grant>>>,
    grant_ttl: std::time::Duration,
}

impl NetPolicy {
    pub fn new(mode: NetMode, entries: Vec<AllowEntry>) -> Self {
        Self {
            mode,
            entries,
            grants: RwLock::new(HashMap::new()),
            grant_ttl: GRANT_TTL,
        }
    }

    #[cfg(test)]
    fn with_grant_ttl(mut self, ttl: std::time::Duration) -> Self {
        self.grant_ttl = ttl;
        self
    }

    /// The grants for `ip` that have not expired.
    fn live_grants<'a>(
        list: &'a [Grant],
        ttl: std::time::Duration,
    ) -> impl Iterator<Item = &'a Grant> + 'a {
        list.iter().filter(move |(_, _, at)| at.elapsed() <= ttl)
    }

    /// Parse `allow` strings; the first invalid one is the error.
    pub fn parse_entries<S: AsRef<str>>(allow: &[S]) -> Result<Vec<AllowEntry>, String> {
        allow.iter().map(|s| s.as_ref().parse()).collect()
    }

    pub fn mode(&self) -> NetMode {
        self.mode
    }

    pub fn entries(&self) -> &[AllowEntry] {
        &self.entries
    }

    /// Apply the mode to what the allowlist says.
    fn apply(&self, rule: Result<(), Reason>) -> Verdict {
        match (self.mode, rule) {
            (NetMode::Disabled, _) => Verdict::Refused("network access is disabled".into()),
            (NetMode::Host, _) | (_, Ok(())) => Verdict::Allowed,
            (NetMode::Audit, Err(r)) => Verdict::Audited(r),
            (NetMode::Allowlist, Err(r)) => Verdict::Refused(r),
        }
    }

    fn name_entries<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a AllowEntry> + 'a {
        self.entries.iter().filter(move |e| e.matches_name(name))
    }

    fn ip_entry_covers(&self, ip: IpAddr, port: u16) -> bool {
        self.entries.iter().any(|e| e.covers_ip(ip, port))
    }

    /// May `name` be resolved?
    pub fn check_resolve(&self, name: &str) -> Verdict {
        let name = normalize_name(name);
        let rule = if let Ok(ip) = name.parse::<IpAddr>() {
            // Resolving a literal address grants nothing; the connect check decides.
            let _ = ip;
            Ok(())
        } else if self.name_entries(&name).next().is_some() {
            Ok(())
        } else {
            Err(format!("{name} is not in the allowlist"))
        };
        self.apply(rule)
    }

    /// Record the addresses `name` resolved to, for the ports of the entries
    /// that allowed it.
    pub fn grant(&self, name: &str, addrs: &[IpAddr]) {
        let name = normalize_name(name);
        let ports: Vec<Ports> = self.name_entries(&name).map(|e| e.ports.clone()).collect();
        if ports.is_empty() {
            return;
        }
        let now = std::time::Instant::now();
        let ttl = self.grant_ttl;
        let mut grants = self.grants.write().unwrap_or_else(|p| p.into_inner());
        // Names move; an address is granted only as long as a recent
        // resolution returned it.
        grants.retain(|_, list| {
            list.retain(|(_, _, at)| at.elapsed() <= ttl);
            !list.is_empty()
        });
        for ip in addrs {
            let list = grants.entry(canonical_ip(*ip)).or_default();
            for p in &ports {
                match list.iter_mut().find(|(lp, ln, _)| lp == p && *ln == name) {
                    Some(existing) => existing.2 = now,
                    None => list.push((p.clone(), name.clone(), now)),
                }
            }
        }
    }

    /// The name an address was resolved from, if any (for logging).
    pub fn name_of(&self, ip: IpAddr) -> Option<String> {
        let grants = self.grants.read().unwrap_or_else(|p| p.into_inner());
        grants
            .get(&canonical_ip(ip))
            .and_then(|l| Self::live_grants(l, self.grant_ttl).next())
            .map(|(_, n, _)| n.clone())
    }

    /// May a TCP connection to `ip:port` be opened?
    pub fn check_connect(&self, ip: IpAddr, port: u16) -> Verdict {
        let rule = if self.ip_entry_covers(ip, port) {
            Ok(())
        } else if is_special(ip) {
            Err(format!(
                "{} is a local or private address; only an explicit IP entry allows it",
                canonical_ip(ip)
            ))
        } else {
            let grants = self.grants.read().unwrap_or_else(|p| p.into_inner());
            let live: Vec<&Grant> = grants
                .get(&canonical_ip(ip))
                .map(|l| Self::live_grants(l, self.grant_ttl).collect())
                .unwrap_or_default();
            match live.first() {
                _ if live.iter().any(|(p, _, _)| p.contains(port)) => Ok(()),
                Some((_, name, _)) => Err(format!("port {port} of {name} is not in the allowlist")),
                None => Err(format!(
                    "{} was not resolved from an allowed name recently and is not in the \
                     allowlist",
                    canonical_ip(ip)
                )),
            }
        };
        self.apply(rule)
    }

    /// May the proxy connect to `host:port` for a host action? `host` is a
    /// name or a literal address; the proxy resolves names itself and checks
    /// every address with [`check_resolved_addr`](Self::check_resolved_addr).
    pub fn check_name_port(&self, host: &str, port: u16) -> Verdict {
        let host = normalize_name(host.trim_start_matches('[').trim_end_matches(']'));
        if let Ok(ip) = host.parse::<IpAddr>() {
            return self.check_connect(ip, port);
        }
        let rule = if self.name_entries(&host).any(|e| e.ports.contains(port)) {
            Ok(())
        } else if self.name_entries(&host).next().is_some() {
            Err(format!("port {port} of {host} is not in the allowlist"))
        } else {
            Err(format!("{host} is not in the allowlist"))
        };
        self.apply(rule)
    }

    /// After a name passed [`check_name_port`](Self::check_name_port): may
    /// this resolved address be used? Refuses special addresses (rebinding).
    pub fn check_resolved_addr(&self, ip: IpAddr, port: u16) -> Verdict {
        let rule = if !is_special(ip) || self.ip_entry_covers(ip, port) {
            Ok(())
        } else {
            Err(format!(
                "the name resolved to the local or private address {}; only an explicit IP entry allows it",
                canonical_ip(ip)
            ))
        };
        self.apply(rule)
    }

    /// Sockets whose later destinations the gate cannot see: bound TCP
    /// sockets, UDP sockets, listeners, raw and ICMP sockets.
    pub fn check_unchecked_socket(&self, what: &str) -> Verdict {
        self.apply(Err(format!(
            "{what} is not allowed; only outgoing TCP connections to allowed destinations are"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(mode: NetMode, allow: &[&str]) -> NetPolicy {
        NetPolicy::new(mode, NetPolicy::parse_entries(allow).unwrap())
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn parses_entries() {
        let cases = [
            ("crates.io", "crates.io:443"),
            ("Crates.IO.:80", "crates.io:80"),
            ("*.crates.io:*", "*.crates.io:*"),
            ("pypi.org:80,443", "pypi.org:80,443"),
            ("example.com:8000-8100", "example.com:8000-8100"),
            ("10.0.0.5:5432", "10.0.0.5:5432"),
            ("10.0.0.0/8:22", "10.0.0.0/8:22"),
            ("10.1.2.3/8:22", "10.0.0.0/8:22"),
            ("[::1]:8080", "[::1]:8080"),
            ("::1", "[::1]:443"),
            ("[fd00::/8]:*", "[fd00::/8]:*"),
            // IPv4-mapped entries are the IPv4 address or net they carry.
            ("[::ffff:10.0.0.5]:5432", "10.0.0.5:5432"),
            ("[::ffff:10.0.0.0/104]:22", "10.0.0.0/8:22"),
        ];
        for (input, shown) in cases {
            let e: AllowEntry = input.parse().unwrap_or_else(|e| panic!("{input}: {e}"));
            assert_eq!(e.to_string(), shown, "{input}");
        }
        for bad in [
            "",
            "exa mple.com",
            "example.com:0",
            "example.com:99999",
            "example.com:10-5",
            "example.com:x",
            "[::1",
            "[::1]8080",
            "*.",
            "-bad.com",
            "a..b",
            "http://x.com",
        ] {
            assert!(bad.parse::<AllowEntry>().is_err(), "{bad:?} was accepted");
        }
    }

    #[test]
    fn ipv4_mapped_entries_match_both_forms() {
        let p = policy(NetMode::Allowlist, &["[::ffff:10.0.0.5]:5432"]);
        assert!(p.check_connect(ip("10.0.0.5"), 5432).allowed());
        assert!(p.check_connect(ip("::ffff:10.0.0.5"), 5432).allowed());
        assert!(!p.check_connect(ip("10.0.0.6"), 5432).allowed());
    }

    #[test]
    fn wildcards_match_subdomains_only() {
        let p = policy(NetMode::Allowlist, &["*.crates.io"]);
        assert!(p.check_resolve("static.crates.io").allowed());
        assert!(p.check_resolve("a.b.crates.io").allowed());
        assert!(!p.check_resolve("crates.io").allowed());
        assert!(!p.check_resolve("evilcrates.io").allowed());
        assert!(!p.check_resolve("crates.io.evil.com").allowed());
    }

    #[test]
    fn classifies_special_addresses() {
        for s in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "224.0.0.1",
            "255.255.255.255",
            "::1",
            "::",
            "fe80::1",
            "fd12::1",
            "::ffff:127.0.0.1",
            "64:ff9b::a00:1",
            "64:ff9b:1::a00:1",
            "2002:a00:1::1",
            "2001:0:4136:e378::1",
            "::10.0.0.1",
            "::8.8.8.8",
            "fec0::1",
            "100::1",
            "192.88.99.1",
            "::ffff:0:a00:1",
            "2001:db8:1::5efe:a00:1",
            "2a00:1450::200:5efe:a00:1",
            "3fff::1",
            "2001:2::1",
        ] {
            assert!(is_special(ip(s)), "{s} should be special");
        }
        for s in [
            "1.1.1.1",
            "140.82.112.3",
            "2606:4700::1111",
            "::ffff:8.8.8.8",
            "2001:4860:4860::8888",
            "2a00:1450::1",
        ] {
            assert!(!is_special(ip(s)), "{s} should not be special");
        }
    }

    #[test]
    fn grants_expire_and_renew() {
        let p = policy(NetMode::Allowlist, &["github.com:443"])
            .with_grant_ttl(std::time::Duration::from_millis(200));
        p.grant("github.com", &[ip("140.82.112.3")]);
        assert!(p.check_connect(ip("140.82.112.3"), 443).allowed());
        std::thread::sleep(std::time::Duration::from_millis(300));
        let v = p.check_connect(ip("140.82.112.3"), 443);
        assert!(!v.allowed(), "{v:?}");
        assert_eq!(p.name_of(ip("140.82.112.3")), None);
        // Resolving again renews the grant.
        p.grant("github.com", &[ip("140.82.112.3")]);
        assert!(p.check_connect(ip("140.82.112.3"), 443).allowed());
    }

    #[test]
    fn resolved_names_grant_exactly_their_ports() {
        let p = policy(NetMode::Allowlist, &["github.com:443", "pypi.org:80,443"]);
        assert!(p.check_resolve("GitHub.com").allowed());
        p.grant("GitHub.com", &[ip("140.82.112.3")]);
        assert_eq!(p.check_connect(ip("140.82.112.3"), 443), Verdict::Allowed);
        assert!(!p.check_connect(ip("140.82.112.3"), 22).allowed());
        // Never resolved: a literal IP is refused.
        let v = p.check_connect(ip("140.82.112.4"), 443);
        assert!(
            matches!(&v, Verdict::Refused(r) if r.contains("not resolved")),
            "{v:?}"
        );
        assert!(!p.check_resolve("example.com").allowed());
        // Mapped IPv6 is the same address.
        assert!(p.check_connect(ip("::ffff:140.82.112.3"), 443).allowed());
        assert_eq!(p.name_of(ip("140.82.112.3")).as_deref(), Some("github.com"));
    }

    #[test]
    fn rebinding_to_local_addresses_is_refused() {
        let p = policy(NetMode::Allowlist, &["evil.example.com"]);
        p.grant(
            "evil.example.com",
            &[ip("127.0.0.1"), ip("169.254.169.254")],
        );
        assert!(!p.check_connect(ip("127.0.0.1"), 443).allowed());
        assert!(!p.check_connect(ip("169.254.169.254"), 443).allowed());
        assert!(!p.check_resolved_addr(ip("127.0.0.1"), 443).allowed());

        let p = policy(NetMode::Allowlist, &["127.0.0.1:8080", "10.0.0.0/8:5432"]);
        assert!(p.check_connect(ip("127.0.0.1"), 8080).allowed());
        assert!(!p.check_connect(ip("127.0.0.1"), 8081).allowed());
        assert!(p.check_connect(ip("10.9.9.9"), 5432).allowed());
        assert!(p.check_resolved_addr(ip("127.0.0.1"), 8080).allowed());
    }

    #[test]
    fn proxy_checks_names_and_ports() {
        let p = policy(NetMode::Allowlist, &["crates.io", "*.github.com:443"]);
        assert!(p.check_name_port("crates.io", 443).allowed());
        assert!(!p.check_name_port("crates.io", 80).allowed());
        assert!(p.check_name_port("api.github.com", 443).allowed());
        assert!(!p.check_name_port("github.com", 443).allowed());
        assert!(!p.check_name_port("127.0.0.1", 443).allowed());
        assert!(!p.check_name_port("[::1]", 443).allowed());
    }

    #[test]
    fn modes() {
        let p = policy(NetMode::Audit, &["crates.io"]);
        assert!(matches!(
            p.check_resolve("example.com"),
            Verdict::Audited(_)
        ));
        assert_eq!(p.check_resolve("crates.io"), Verdict::Allowed);
        assert!(matches!(
            p.check_unchecked_socket("udp"),
            Verdict::Audited(_)
        ));

        let p = policy(NetMode::Host, &[]);
        assert_eq!(p.check_resolve("example.com"), Verdict::Allowed);
        assert_eq!(p.check_connect(ip("127.0.0.1"), 22), Verdict::Allowed);

        let p = policy(NetMode::Disabled, &["crates.io"]);
        assert!(!p.check_resolve("crates.io").allowed());

        let p = policy(NetMode::Allowlist, &["crates.io"]);
        assert!(!p.check_unchecked_socket("udp").allowed());
        assert_eq!("audit".parse::<NetMode>(), Ok(NetMode::Audit));
        assert!("open".parse::<NetMode>().is_err());
    }
}
