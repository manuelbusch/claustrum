//! `claustrum network`: inspect the network log and derive allowlist entries.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

use anyhow::{Context, Result};
use claustrum_sandbox::net::{LogEntry, read_log};

use crate::config::Config;

#[derive(clap::Subcommand, Debug)]
pub enum Command {
    /// Summarise refused and audit-flagged connections and suggest `allow`
    /// entries for them.
    Report {
        /// Workspace whose log to read. Defaults to the current directory.
        #[arg(long)]
        workspace: Option<PathBuf>,
        /// Also list allowed connections.
        #[arg(long)]
        all: bool,
    },
}

pub fn run(config: Config, cmd: Command) -> Result<()> {
    match cmd {
        Command::Report { workspace, all } => {
            let workspace = config.workspace(workspace.as_deref())?;
            config.migrate_state(&workspace);
            let path = config.network_log_path(&workspace);
            let entries = match read_log(&path) {
                Ok(e) => e,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    println!("No network log at {} yet.", path.display());
                    return Ok(());
                }
                Err(e) => {
                    return Err(e).with_context(|| format!("cannot read {}", path.display()));
                }
            };
            print!("{}", report(&entries, all, &path.display().to_string()));
            Ok(())
        }
    }
}

/// Destination key: the host name when known, else the address.
fn host_of(e: &LogEntry) -> String {
    e.host.clone().unwrap_or_else(|| {
        e.addr
            .map(|a| match a {
                std::net::IpAddr::V6(v6) => format!("[{v6}]"),
                v4 => v4.to_string(),
            })
            .unwrap_or_else(|| "?".into())
    })
}

#[derive(Default)]
struct Row {
    count: usize,
    ports: BTreeSet<u16>,
    sources: BTreeSet<String>,
    kinds: BTreeSet<String>,
    reason: Option<String>,
}

fn report(entries: &[LogEntry], all: bool, path: &str) -> String {
    let mut out = format!("Network log: {path} ({} entries)\n", entries.len());
    let mut groups: BTreeMap<(String, String), Row> = BTreeMap::new();
    for e in entries {
        if e.verdict == "allowed" && !all {
            continue;
        }
        let row = groups.entry((e.verdict.clone(), host_of(e))).or_default();
        row.count += 1;
        if let Some(p) = e.port.filter(|p| *p != 0) {
            row.ports.insert(p);
        }
        row.sources.insert(e.source.clone());
        row.kinds.insert(e.kind.clone());
        if row.reason.is_none() {
            row.reason.clone_from(&e.reason);
        }
    }
    if groups.is_empty() {
        out.push_str("Nothing was refused or flagged.\n");
        return out;
    }
    let mut suggestions: BTreeMap<String, BTreeSet<u16>> = BTreeMap::new();
    for verdict in ["refused", "audit", "allowed"] {
        let rows: Vec<_> = groups.iter().filter(|((v, _), _)| v == verdict).collect();
        if rows.is_empty() {
            continue;
        }
        out.push_str(match verdict {
            "refused" => "\nRefused:\n",
            "audit" => "\nAllowed in audit mode, the allowlist would refuse:\n",
            _ => "\nAllowed:\n",
        });
        for ((_, host), row) in rows {
            let ports = if row.ports.is_empty() {
                String::new()
            } else {
                format!(
                    ":{}",
                    row.ports
                        .iter()
                        .map(u16::to_string)
                        .collect::<Vec<_>>()
                        .join(",")
                )
            };
            out.push_str(&format!(
                "  {host}{ports}  {}x  [{}] from {}",
                row.count,
                row.kinds.iter().cloned().collect::<Vec<_>>().join(","),
                row.sources.iter().cloned().collect::<Vec<_>>().join(", ")
            ));
            if let Some(r) = &row.reason {
                out.push_str(&format!("  ({r})"));
            }
            out.push('\n');
            // Only destinations that a name or address entry can express.
            let suggestible = row
                .kinds
                .iter()
                .any(|k| matches!(k.as_str(), "dns" | "tcp" | "connect" | "http"));
            if verdict != "allowed" && suggestible && host != "?" {
                suggestions
                    .entry(host.clone())
                    .or_default()
                    .extend(row.ports.iter().copied());
            }
        }
    }
    if !suggestions.is_empty() {
        out.push_str(
            "\nSuggested [network] allow entries (review each one; only add what the \
             project really needs):\n\nallow = [\n",
        );
        for (host, ports) in &suggestions {
            if ports.is_empty() {
                out.push_str(&format!(
                    "  \"{host}\",  # port unknown (the name was refused), 443 assumed\n"
                ));
            } else {
                let ports = ports
                    .iter()
                    .map(u16::to_string)
                    .collect::<Vec<_>>()
                    .join(",");
                out.push_str(&format!("  \"{host}:{ports}\",\n"));
            }
        }
        out.push_str("]\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(
        verdict: &str,
        kind: &str,
        host: Option<&str>,
        addr: Option<&str>,
        port: Option<u16>,
    ) -> LogEntry {
        LogEntry {
            seq: 0,
            time: 0,
            source: "guest".into(),
            kind: kind.into(),
            host: host.map(Into::into),
            addr: addr.map(|a| a.parse().unwrap()),
            port,
            verdict: verdict.into(),
            reason: Some("not in the allowlist".into()),
        }
    }

    #[test]
    fn groups_and_suggests() {
        let entries = vec![
            entry("refused", "dns", Some("pypi.org"), None, None),
            entry(
                "audit",
                "tcp",
                Some("crates.io"),
                Some("1.2.3.4"),
                Some(443),
            ),
            entry(
                "audit",
                "tcp",
                Some("crates.io"),
                Some("1.2.3.4"),
                Some(443),
            ),
            entry("refused", "tcp", None, Some("10.0.0.5"), Some(5432)),
            entry("refused", "udp", None, Some("0.0.0.0"), Some(0)),
            entry(
                "allowed",
                "tcp",
                Some("github.com"),
                Some("5.6.7.8"),
                Some(443),
            ),
        ];
        let r = report(&entries, false, "log");
        assert!(r.contains("crates.io:443  2x"), "{r}");
        assert!(!r.contains("github.com"), "{r}");
        assert!(r.contains("  \"crates.io:443\",\n"), "{r}");
        assert!(r.contains("  \"10.0.0.5:5432\",\n"), "{r}");
        assert!(r.contains("\"pypi.org\",  # port unknown"), "{r}");
        assert!(!r.contains("\"0.0.0.0"), "{r}");
        assert!(report(&entries, true, "log").contains("github.com"));
        assert!(report(&[], false, "log").contains("Nothing was refused"));
    }
}
