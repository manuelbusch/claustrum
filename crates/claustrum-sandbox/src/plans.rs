//! Claude Code's plan files, kept on the host outside the sandbox.
//!
//! In plan mode Claude Code names a plan file in its own plan directory
//! (`~/.claude/plans/<slug>.md`) and refuses every tool that is not marked
//! read-only. The guest never sees that directory: it holds the plans of all
//! projects, and Claude Code, which runs unconfined, reads and writes the
//! files there. Plans are written through a [`PlanStore`] instead, which only
//! takes a file name and only touches files that this workspace created,
//! recorded in a ledger outside the plan directory.

use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Mutex,
};

use crate::native::{EditOutput, WriteOutput};

/// Largest plan file that is written or edited.
pub const MAX_PLAN_BYTES: usize = 256 * 1024;

/// Writes plan files. Implemented on the host by [`HostPlans`] and, from a
/// confined worker, by the broker client.
pub trait PlanStore: Send + Sync + std::fmt::Debug {
    /// Create or overwrite the plan file `name`.
    fn write(&self, name: &str, content: &str) -> Result<WriteOutput, String>;
    /// Replace `old_string` in the plan file `name`, like the Edit tool.
    fn edit(
        &self,
        name: &str,
        old_string: &str,
        new_string: &str,
        replace_all: bool,
    ) -> Result<EditOutput, String>;
}

/// Whether `name` looks like a plan file Claude Code creates: lower-case
/// words joined by `-`, ending in `.md`.
pub fn is_plan_name(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".md") else {
        return false;
    };
    !stem.is_empty()
        && stem.len() <= 128
        && stem
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !stem.starts_with('-')
}

/// The plan directory on the host, for one workspace.
#[derive(Debug)]
pub struct HostPlans {
    dir: PathBuf,
    ledger: PathBuf,
    lock: Mutex<()>,
}

impl HostPlans {
    /// `dir` is Claude Code's plan directory, `ledger` the file that records
    /// which plans this workspace created.
    pub fn new(dir: impl Into<PathBuf>, ledger: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            ledger: ledger.into(),
            lock: Mutex::new(()),
        }
    }

    /// Names of the plans this workspace created, oldest first.
    pub fn names(&self) -> std::io::Result<Vec<String>> {
        match std::fs::read_to_string(&self.ledger) {
            Ok(text) => Ok(text
                .lines()
                .filter(|l| is_plan_name(l))
                .map(str::to_owned)
                .collect()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn display(&self, name: &str) -> String {
        self.dir.join(name).display().to_string()
    }

    fn check(&self, name: &str) -> Result<(), String> {
        if is_plan_name(name) {
            Ok(())
        } else {
            Err(format!(
                "`{name}` is not a plan file name; use the file name plan mode gives you"
            ))
        }
    }

    fn owned(&self, name: &str) -> Result<bool, String> {
        self.names()
            .map(|names| names.iter().any(|n| n == name))
            .map_err(|e| format!("cannot read {}: {e}", self.ledger.display()))
    }

    fn record(&self, name: &str) -> Result<(), String> {
        let fail = |e: std::io::Error| format!("cannot record {name}: {e}");
        if let Some(parent) = self.ledger.parent() {
            std::fs::create_dir_all(parent).map_err(fail)?;
        }
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.ledger)
            .map_err(fail)?;
        writeln!(f, "{name}").map_err(fail)
    }

    /// Open an existing plan of this workspace, never following a link.
    fn open_owned(&self, name: &str, write: bool) -> Result<File, String> {
        let path = self.dir.join(name);
        let mut opts = OpenOptions::new();
        if write {
            opts.write(true).truncate(true);
        } else {
            opts.read(true);
        }
        let f = nofollow(&mut opts)
            .open(&path)
            .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
        regular(&f, &path)?;
        Ok(f)
    }

    fn write_locked(&self, name: &str, content: &str) -> Result<WriteOutput, String> {
        if content.len() > MAX_PLAN_BYTES {
            return Err(format!(
                "the plan is larger than {MAX_PLAN_BYTES} bytes; shorten it"
            ));
        }
        let path = self.dir.join(name);
        let created = match std::fs::symlink_metadata(&path) {
            Ok(_) if self.owned(name)? => false,
            Ok(_) => {
                return Err(format!(
                    "{} was not created from this workspace; plan mode names a new file for \
                     every session, write that one",
                    path.display()
                ));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
            Err(e) => return Err(format!("cannot inspect {}: {e}", path.display())),
        };
        let mut f = if created {
            create_private_dir(&self.dir)
                .map_err(|e| format!("cannot create {}: {e}", self.dir.display()))?;
            let mut opts = OpenOptions::new();
            opts.write(true).create_new(true);
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
            let f = nofollow(&mut opts)
                .open(&path)
                .map_err(|e| format!("cannot create {}: {e}", path.display()))?;
            self.record(name)?;
            f
        } else {
            self.open_owned(name, true)?
        };
        f.write_all(content.as_bytes())
            .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
        Ok(WriteOutput {
            path: self.display(name),
            created,
            bytes: content.len(),
        })
    }
}

impl PlanStore for HostPlans {
    fn write(&self, name: &str, content: &str) -> Result<WriteOutput, String> {
        self.check(name)?;
        let _guard = self.lock.lock().unwrap_or_else(|p| p.into_inner());
        self.write_locked(name, content)
    }

    fn edit(
        &self,
        name: &str,
        old_string: &str,
        new_string: &str,
        replace_all: bool,
    ) -> Result<EditOutput, String> {
        self.check(name)?;
        let _guard = self.lock.lock().unwrap_or_else(|p| p.into_inner());
        if !self.owned(name)? {
            return Err(format!(
                "{} was not created from this workspace; write the plan with WritePlan first",
                self.display(name)
            ));
        }
        let mut text = String::new();
        self.open_owned(name, false)?
            .take(MAX_PLAN_BYTES as u64 + 1)
            .read_to_string(&mut text)
            .map_err(|e| format!("cannot read {}: {e}", self.display(name)))?;
        let (updated, out) = crate::native::apply_edit(
            &text,
            &self.display(name),
            old_string,
            new_string,
            replace_all,
        )
        .map_err(|e| e.to_string())?;
        self.write_locked(name, &updated)?;
        Ok(out)
    }
}

fn nofollow(opts: &mut OpenOptions) -> &mut OpenOptions {
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::custom_flags(opts, libc::O_NOFOLLOW);
    opts
}

fn regular(f: &File, path: &Path) -> Result<(), String> {
    match f.metadata() {
        Ok(m) if m.is_file() => Ok(()),
        Ok(_) => Err(format!("{} is not a regular file", path.display())),
        Err(e) => Err(format!("cannot inspect {}: {e}", path.display())),
    }
}

fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, HostPlans) {
        let tmp = tempfile::tempdir().unwrap();
        let plans = HostPlans::new(tmp.path().join("plans"), tmp.path().join("state/ws.list"));
        (tmp, plans)
    }

    #[test]
    fn names() {
        for ok in ["a.md", "bubbly-sifakis.md", "x-agent-a1b2.md"] {
            assert!(is_plan_name(ok), "{ok}");
        }
        for bad in [
            ".md", "a", "A.md", "../a.md", "a/b.md", "-a.md", "a.md.md ", "a b.md", "a.txt",
        ] {
            assert!(!is_plan_name(bad), "{bad}");
        }
    }

    #[test]
    fn writes_and_edits_own_plans() {
        let (tmp, plans) = store();
        let out = plans.write("p.md", "# Plan\nstep one\n").unwrap();
        assert!(out.created);
        assert_eq!(
            out.path,
            tmp.path().join("plans/p.md").display().to_string()
        );
        let out = plans.edit("p.md", "one", "two", false).unwrap();
        assert_eq!(out.replacements, 1);
        let out = plans
            .write("p.md", "# Plan\nstep two\nstep three\n")
            .unwrap();
        assert!(!out.created);
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("plans/p.md")).unwrap(),
            "# Plan\nstep two\nstep three\n"
        );
        assert_eq!(plans.names().unwrap(), ["p.md"]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &str| {
                std::fs::metadata(tmp.path().join(p))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777
            };
            assert_eq!(mode("plans/p.md"), 0o600);
            assert_eq!(mode("plans"), 0o700);
        }
    }

    #[test]
    fn foreign_plans_are_untouchable() {
        let (tmp, plans) = store();
        std::fs::create_dir_all(tmp.path().join("plans")).unwrap();
        std::fs::write(tmp.path().join("plans/other.md"), "theirs\n").unwrap();
        let err = plans.write("other.md", "mine\n").unwrap_err();
        assert!(err.contains("not created from this workspace"), "{err}");
        let err = plans.edit("other.md", "theirs", "mine", false).unwrap_err();
        assert!(err.contains("not created from this workspace"), "{err}");
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("plans/other.md")).unwrap(),
            "theirs\n"
        );
        // Another workspace's ledger does not grant anything here.
        plans.write("mine.md", "x\n").unwrap();
        let other = HostPlans::new(
            tmp.path().join("plans"),
            tmp.path().join("state/other.list"),
        );
        assert!(other.write("mine.md", "y\n").is_err());
        assert!(other.edit("mine.md", "x", "y", false).is_err());
        for bad in ["../escape.md", "/etc/passwd", "p.txt"] {
            assert!(plans.write(bad, "x").is_err(), "{bad}");
        }
        assert!(!tmp.path().join("escape.md").exists());
        assert!(
            plans
                .write("big.md", &"x".repeat(MAX_PLAN_BYTES + 1))
                .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn links_are_never_followed() {
        let (tmp, plans) = store();
        let secret = tmp.path().join("secret");
        std::fs::write(&secret, "secret\n").unwrap();
        plans.write("p.md", "plan\n").unwrap();
        // Replaced by a link after it was created (by anything on the host).
        std::fs::remove_file(tmp.path().join("plans/p.md")).unwrap();
        std::os::unix::fs::symlink(&secret, tmp.path().join("plans/p.md")).unwrap();
        assert!(plans.write("p.md", "overwritten\n").is_err());
        assert!(plans.edit("p.md", "secret", "gone", false).is_err());
        // A link placed before first use.
        std::os::unix::fs::symlink(&secret, tmp.path().join("plans/q.md")).unwrap();
        assert!(plans.write("q.md", "overwritten\n").is_err());
        assert_eq!(std::fs::read_to_string(&secret).unwrap(), "secret\n");
    }
}
