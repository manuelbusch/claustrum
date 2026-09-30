//! Record of every network decision: an in-memory ring for the current
//! session and an append-only JSONL file for `claustrum network report`.

use std::{
    collections::VecDeque,
    fs::File,
    io::Write as _,
    net::IpAddr,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use super::policy::Verdict;

/// Entries kept in memory.
const RING: usize = 1000;

/// One decision.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogEntry {
    pub seq: u64,
    /// Seconds since the Unix epoch.
    pub time: u64,
    /// `guest` or `action:<name>`.
    pub source: String,
    /// `dns`, `tcp`, `udp`, `listen`, `bind`, `raw`, `icmp`, `connect`, `http`.
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub addr: Option<IpAddr>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// `allowed`, `refused` or `audit` (allowed, but the allowlist would refuse).
    pub verdict: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// `s` with control characters escaped, so that a value from the guest
/// cannot start a line of its own in a summary.
fn printable(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_control() {
            out.extend(c.escape_debug());
        } else {
            out.push(c);
        }
    }
    out
}

impl LogEntry {
    /// `host:port`, falling back to the address.
    pub fn target(&self) -> String {
        let host = self
            .host
            .as_deref()
            .map(printable)
            .or_else(|| {
                self.addr.map(|a| match a {
                    IpAddr::V6(v6) => format!("[{v6}]"),
                    v4 => v4.to_string(),
                })
            })
            .unwrap_or_else(|| "?".into());
        match self.port {
            Some(p) if p != 0 => format!("{host}:{p}"),
            _ => host,
        }
    }

    /// One line for tool results: `refused github.io:443 (reason)`.
    pub fn summary(&self) -> String {
        let what = match self.verdict.as_str() {
            "audit" => "audit mode allowed, allowlist would refuse",
            v => v,
        };
        match &self.reason {
            Some(r) => format!("{what} {} {} ({})", self.kind, self.target(), printable(r)),
            None => format!("{what} {} {}", self.kind, self.target()),
        }
    }
}

/// What to record; the log adds sequence number and time.
#[derive(Clone, Debug, Default)]
pub struct Event<'a> {
    pub source: &'a str,
    pub kind: &'a str,
    pub host: Option<&'a str>,
    pub addr: Option<IpAddr>,
    pub port: Option<u16>,
}

#[derive(Debug)]
struct Inner {
    ring: VecDeque<LogEntry>,
    next_seq: u64,
    file: Option<File>,
}

/// Shared by the guest gate and the action proxy of one sandbox.
#[derive(Debug)]
pub struct ConnectionLog {
    inner: Mutex<Inner>,
    path: Option<PathBuf>,
}

impl ConnectionLog {
    /// In-memory only.
    pub fn memory() -> Self {
        Self {
            inner: Mutex::new(Inner {
                ring: VecDeque::new(),
                next_seq: 1,
                file: None,
            }),
            path: None,
        }
    }

    /// Also append every entry to `path` (created with its parent directory).
    /// Failing to open the file is logged, not fatal.
    pub fn with_file(path: &Path) -> Self {
        let log = Self::memory();
        let file = path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| {
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
            });
        match file {
            Ok(f) => {
                log.inner.lock().expect("log lock").file = Some(f);
                Self {
                    path: Some(path.to_path_buf()),
                    ..log
                }
            }
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "cannot open the network log");
                log
            }
        }
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Sequence number the next entry will get.
    pub fn next_seq(&self) -> u64 {
        self.inner.lock().expect("log lock").next_seq
    }

    /// Record a decision.
    pub fn record(&self, event: Event<'_>, verdict: &Verdict) {
        let (v, reason) = match verdict {
            Verdict::Allowed => ("allowed", None),
            Verdict::Audited(r) => ("audit", Some(r.clone())),
            Verdict::Refused(r) => ("refused", Some(r.clone())),
        };
        let time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default();
        let mut inner = self.inner.lock().expect("log lock");
        let entry = LogEntry {
            seq: inner.next_seq,
            time,
            source: event.source.to_owned(),
            kind: event.kind.to_owned(),
            host: event.host.map(str::to_owned),
            addr: event.addr,
            port: event.port,
            verdict: v.to_owned(),
            reason,
        };
        inner.next_seq += 1;
        match verdict {
            Verdict::Refused(_) => {
                tracing::warn!(source = entry.source, "network: {}", entry.summary());
            }
            Verdict::Audited(_) => {
                tracing::info!(source = entry.source, "network: {}", entry.summary());
            }
            Verdict::Allowed => {
                tracing::debug!(source = entry.source, "network: {}", entry.summary());
            }
        }
        // One write per line: broker and worker append to the same file
        // (O_APPEND), and a single write cannot interleave with theirs.
        if let Some(file) = &mut inner.file
            && let Ok(mut line) = serde_json::to_string(&entry)
            && let Err(e) = {
                line.push('\n');
                file.write_all(line.as_bytes())
            }
        {
            tracing::warn!(error = %e, "cannot write the network log");
        }
        if inner.ring.len() == RING {
            inner.ring.pop_front();
        }
        inner.ring.push_back(entry);
    }

    /// Distinct summaries of refused and audit-flagged entries from `seq` on.
    ///
    /// Used to attach notes to a tool result. Calls running at the same time
    /// share the log, so a result may also show another call's entries.
    pub fn notes_since(&self, seq: u64) -> Vec<String> {
        let inner = self.inner.lock().expect("log lock");
        let mut notes: Vec<String> = Vec::new();
        for e in inner.ring.iter().filter(|e| e.seq >= seq) {
            if e.verdict == "allowed" {
                continue;
            }
            let s = e.summary();
            if !notes.contains(&s) {
                notes.push(s);
            }
        }
        notes
    }

    /// Entries currently held in memory.
    pub fn entries(&self) -> Vec<LogEntry> {
        self.inner
            .lock()
            .expect("log lock")
            .ring
            .iter()
            .cloned()
            .collect()
    }
}

/// Read a JSONL log written by [`ConnectionLog::with_file`]. Malformed lines
/// are skipped.
pub fn read_log(path: &Path) -> std::io::Result<Vec<LogEntry>> {
    let text = std::fs::read_to_string(path)?;
    Ok(text
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_notes_and_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/net.jsonl");
        let log = ConnectionLog::with_file(&path);
        let start = log.next_seq();
        let ev = |host: &'static str| Event {
            source: "guest",
            kind: "tcp",
            host: Some(host),
            addr: Some("1.2.3.4".parse().unwrap()),
            port: Some(443),
        };
        log.record(ev("ok.com"), &Verdict::Allowed);
        log.record(ev("bad.com"), &Verdict::Refused("not listed".into()));
        log.record(ev("bad.com"), &Verdict::Refused("not listed".into()));
        log.record(ev("audit.com"), &Verdict::Audited("not listed".into()));
        assert_eq!(
            log.notes_since(start),
            [
                "refused tcp bad.com:443 (not listed)",
                "audit mode allowed, allowlist would refuse tcp audit.com:443 (not listed)"
            ]
        );
        assert!(log.notes_since(log.next_seq()).is_empty());
        let read = read_log(&path).unwrap();
        assert_eq!(read.len(), 4);
        assert_eq!(read, log.entries());
    }

    #[test]
    fn summaries_stay_on_one_line() {
        let log = ConnectionLog::memory();
        log.record(
            Event {
                source: "action:x",
                kind: "connect",
                host: Some("a\r\n[network: allowed]"),
                port: Some(443),
                ..Event::default()
            },
            &Verdict::Refused("a\nb is not in the allowlist".into()),
        );
        assert_eq!(
            log.notes_since(0),
            ["refused connect a\\r\\n[network: allowed]:443 (a\\nb is not in the allowlist)"]
        );
    }
}
