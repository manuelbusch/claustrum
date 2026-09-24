//! macOS Seatbelt backend: a generated SBPL profile applied by
//! `/usr/bin/sandbox-exec`, which then executes the program.
//!
//! `sandbox-exec` is deprecated but still shipped and used by Chromium, the
//! OpenAI Codex CLI and Anthropic's sandbox runtime. Seatbelt profiles cannot
//! be nested: a confined process cannot apply a second profile, so every
//! confined process must be started by an unconfined one.

use std::{
    fmt::Write as _,
    path::{Path, PathBuf},
    process::Command,
};

use crate::{Exec, Network, Profile, Unavailable, ancestors, resolve};

const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

pub(crate) fn available() -> Result<(), Unavailable> {
    if Path::new(SANDBOX_EXEC).is_file() {
        Ok(())
    } else {
        Err(Unavailable(format!("{SANDBOX_EXEC} is missing")))
    }
}

pub(crate) fn command(profile: &Profile, program: &Path) -> Command {
    let mut cmd = Command::new(SANDBOX_EXEC);
    cmd.arg("-p").arg(render(profile)).arg(program);
    cmd
}

/// The SBPL text for `profile`. Later rules take precedence over earlier
/// ones, so the deny rules come last.
pub(crate) fn render(profile: &Profile) -> String {
    let mut s = String::new();
    let _ = writeln!(
        s,
        ";; Claustrum profile: {}",
        profile.name.replace('\n', " ")
    );
    s.push_str(BASE);

    match &profile.exec {
        Exec::Only(programs) => {
            rule(&mut s, "allow process-exec", "literal", programs);
        }
        Exec::Any => {
            s.push_str("(allow process-exec)\n(allow process-fork)\n");
            s.push_str("(allow signal (target same-sandbox))\n");
            s.push_str("(allow process-info* (target same-sandbox))\n");
        }
    }

    if profile.read_everything {
        s.push_str("(allow file-read*)\n");
    }
    rule(&mut s, "allow file-read*", "subpath", &profile.read);
    rule(
        &mut s,
        "allow file-read* file-write*",
        "subpath",
        &profile.write,
    );

    match profile.network {
        Network::None => {}
        Network::Loopback(port) => {
            let _ = writeln!(
                s,
                "(allow network-outbound (remote ip \"localhost:{port}\"))"
            );
        }
        Network::Outbound | Network::Any => {
            s.push_str(NETWORK);
            if profile.network == Network::Any {
                s.push_str("(allow network-bind network-inbound)\n");
            }
        }
    }

    rule(&mut s, "deny file-read*", "subpath", &profile.deny_read);
    let mut frozen: Vec<PathBuf> = Vec::new();
    for p in &profile.deny_write {
        let p = resolve(p);
        for a in ancestors(&p) {
            if !frozen.iter().any(|f| f == a) {
                frozen.push(a.to_path_buf());
            }
        }
        frozen.push(p);
    }
    // Resolved already; `rule` resolves again, which is a no-op.
    rule(&mut s, "deny file-write*", "literal", &frozen);
    s
}

fn rule(s: &mut String, action: &str, filter: &str, paths: &[PathBuf]) {
    if paths.is_empty() {
        return;
    }
    let _ = write!(s, "({action}");
    for p in paths {
        let _ = write!(s, "\n  ({filter} {})", quote(&resolve(p)));
    }
    s.push_str(")\n");
}

fn quote(path: &Path) -> String {
    let raw = path.to_string_lossy();
    let mut out = String::with_capacity(raw.len() + 2);
    out.push('"');
    for c in raw.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Rules every profile gets: the system base profile, process basics and the
/// user database lookup that `getpwuid` (home directory) needs.
const BASE: &str = r#"(version 1)
(deny default)
(import "system.sb")
(allow process-info* (target self))
(allow signal (target self))
(allow sysctl-read)
(allow file-read-metadata)
(allow file-read* file-write-data file-ioctl
  (literal "/dev/tty")
  (literal "/dev/null"))
(allow mach-lookup
  (global-name "com.apple.system.opendirectoryd.libinfo")
  (global-name "com.apple.system.notification_center"))
"#;

/// Outgoing network plus name resolution.
const NETWORK: &str = r#"(allow network-outbound)
(allow system-socket)
(allow mach-lookup
  (global-name "com.apple.dnssd.service")
  (global-name "com.apple.SystemConfiguration.configd")
  (global-name "com.apple.trustd.agent")
  (global-name "com.apple.networkd"))
"#;

#[cfg(test)]
mod tests {
    use std::process::Stdio;

    use super::*;

    fn run(profile: &Profile, script: &str) -> (bool, String) {
        let mut p = profile.clone();
        p.exec = Exec::Any;
        let out = command(&p, Path::new("/bin/sh"))
            .arg("-c")
            .arg(script)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        (out.status.success(), text)
    }

    fn workspace() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().canonicalize().unwrap();
        std::fs::write(ws.join("claustrum.toml"), "# original\n").unwrap();
        (dir, ws)
    }

    fn profile(ws: &Path) -> Profile {
        let mut p = Profile::new("test", "/bin/sh");
        p.read_everything = true;
        p.write = vec![ws.to_path_buf()];
        p.deny_write = vec![ws.join("claustrum.toml")];
        p
    }

    #[test]
    fn quotes_paths() {
        assert_eq!(quote(Path::new("/a \"b\"\\c")), "\"/a \\\"b\\\"\\\\c\"");
    }

    #[test]
    fn writes_stay_inside_the_writable_trees() {
        let (_d, ws) = workspace();
        let outside = tempfile::tempdir().unwrap();
        let outside = outside.path().canonicalize().unwrap();
        let p = profile(&ws);
        let (ok, out) = run(
            &p,
            &format!("cd {} && echo x > inside && cat inside", ws.display()),
        );
        assert!(ok, "{out}");
        let (ok, _) = run(&p, &format!("echo x > {}/escape", outside.display()));
        assert!(!ok);
        assert!(!outside.join("escape").exists());
    }

    #[test]
    fn protected_file_cannot_be_changed_or_moved() {
        let (_d, ws) = workspace();
        let p = profile(&ws);
        let parent = ws.parent().unwrap();
        let name = ws.file_name().unwrap().to_string_lossy();
        for attempt in [
            "echo x >> claustrum.toml".to_owned(),
            "rm claustrum.toml".to_owned(),
            "mv claustrum.toml moved".to_owned(),
            "ln claustrum.toml hard && echo x >> hard".to_owned(),
            format!("cd {} && mv {name} renamed", parent.display()),
        ] {
            let (ok, out) = run(&p, &format!("cd {} && {attempt}", ws.display()));
            assert!(!ok, "`{attempt}` succeeded: {out}");
        }
        assert_eq!(
            std::fs::read_to_string(ws.join("claustrum.toml")).unwrap(),
            "# original\n"
        );
        // The workspace itself stays usable.
        let (ok, out) = run(
            &p,
            &format!("cd {} && mkdir d && echo y > d/f && rm -r d", ws.display()),
        );
        assert!(ok, "{out}");
    }

    #[test]
    fn denied_reads_and_network() {
        let (_d, ws) = workspace();
        std::fs::create_dir(ws.join("secret")).unwrap();
        std::fs::write(ws.join("secret/key"), "k").unwrap();
        let mut p = profile(&ws);
        p.deny_read = vec![ws.join("secret")];
        let (ok, _) = run(&p, &format!("cat {}/secret/key", ws.display()));
        assert!(!ok);
        let (ok, _) = run(&p, "/usr/bin/nc -z -G 2 1.1.1.1 443");
        assert!(!ok, "network should be denied");
    }

    #[test]
    fn exec_is_limited_to_the_listed_programs() {
        let p = Profile::new("test", "/bin/sh");
        let out = command(&p, Path::new("/bin/sh"))
            .arg("-c")
            .arg("/bin/echo hi")
            .output()
            .unwrap();
        assert!(!out.status.success());
    }
}
