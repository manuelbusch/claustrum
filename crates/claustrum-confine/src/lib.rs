//! OS-level confinement for the host processes Claustrum starts.
//!
//! Claustrum's first isolation layer is WASIX. This crate is the second: an
//! operating-system sandbox around the processes that run natively on the
//! host, i.e. the worker that hosts the Wasmer runtime and every host action.
//! A [`Profile`] describes what such a process may touch; [`command`] turns a
//! program into a [`Command`] that starts it confined by the backend of the
//! current platform (see [`backend`]).
//!
//! Profiles are allow-lists: whatever a profile does not grant is denied,
//! except reads when [`Profile::read_everything`] is set. Deny rules
//! ([`Profile::deny_read`], [`Profile::deny_write`]) win over allow rules.

use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    process::Command,
};

#[cfg(target_os = "macos")]
mod seatbelt;

/// Network access granted to a confined process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Network {
    /// No sockets to anywhere (local IPC through inherited descriptors still
    /// works).
    None,
    /// TCP to `localhost:<port>` only, e.g. Claustrum's action proxy.
    Loopback(u16),
    /// Outgoing connections to anywhere, no listening.
    Outbound,
    /// Everything, including listening sockets.
    Any,
}

/// Which programs a confined process may execute.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Exec {
    /// Only these files (the process's own binary must be listed).
    Only(Vec<PathBuf>),
    /// Any program, including forking.
    Any,
}

/// What a confined process may access. Paths should be absolute; they are
/// resolved (symlinks followed) when the profile is applied, because the
/// backends match resolved paths.
#[derive(Clone, Debug)]
pub struct Profile {
    /// Short label for logs and errors, e.g. `worker` or `action build`.
    pub name: String,
    /// Allow reading the whole file system, minus [`Profile::deny_read`].
    /// Without it, only system paths, [`Profile::read`] and
    /// [`Profile::write`] are readable.
    pub read_everything: bool,
    /// Directory trees (or files) the process may read.
    pub read: Vec<PathBuf>,
    /// Directory trees (or files) the process may read and write.
    pub write: Vec<PathBuf>,
    /// Trees that stay unreadable even when a broader rule allows them.
    pub deny_read: Vec<PathBuf>,
    /// Files that stay read-only even when they lie in a writable tree. Their
    /// ancestors cannot be renamed or removed either, so that the file cannot
    /// be moved away with its directory.
    pub deny_write: Vec<PathBuf>,
    pub exec: Exec,
    pub network: Network,
}

impl Profile {
    /// An empty profile: system libraries only, no writes, no network, and
    /// only `binary` may be executed.
    pub fn new(name: impl Into<String>, binary: impl Into<PathBuf>) -> Self {
        Self {
            name: name.into(),
            read_everything: false,
            read: Vec::new(),
            write: Vec::new(),
            deny_read: Vec::new(),
            deny_write: Vec::new(),
            exec: Exec::Only(vec![binary.into()]),
            network: Network::None,
        }
    }
}

/// The confinement mechanism of this platform.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    /// macOS Seatbelt through `/usr/bin/sandbox-exec`.
    Seatbelt,
}

impl Backend {
    pub fn as_str(self) -> &'static str {
        match self {
            Backend::Seatbelt => "seatbelt",
        }
    }
}

impl std::fmt::Display for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why confinement is not possible here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unavailable(pub String);

impl std::fmt::Display for Unavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Unavailable {}

/// The backend usable on this machine, or why there is none.
pub fn backend() -> Result<Backend, Unavailable> {
    #[cfg(target_os = "macos")]
    {
        seatbelt::available().map(|()| Backend::Seatbelt)
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err(Unavailable(format!(
            "no OS confinement backend is implemented for {} yet",
            std::env::consts::OS
        )))
    }
}

/// A command that runs `program` confined by `profile`. The caller adds
/// arguments, environment, working directory and stdio as usual. `program`
/// must be an absolute path.
pub fn command(profile: &Profile, program: impl AsRef<OsStr>) -> Result<Command, Unavailable> {
    let program = Path::new(program.as_ref());
    if !program.is_absolute() {
        return Err(Unavailable(format!(
            "{}: the confined program must be an absolute path, got {}",
            profile.name,
            program.display()
        )));
    }
    let backend = backend()?;
    tracing::debug!(profile = %profile.name, %backend, program = %program.display(), "confining");
    match backend {
        #[cfg(target_os = "macos")]
        Backend::Seatbelt => Ok(seatbelt::command(profile, program)),
        #[allow(unreachable_patterns)]
        _ => Err(Unavailable(format!("{backend} is not supported here"))),
    }
}

/// Places under the home directory that hold credentials or private data and
/// that no confined process needs to read: SSH, cloud and registry
/// credentials, keychains, browser profiles, mail and messages.
pub fn secret_paths(home: &Path) -> Vec<PathBuf> {
    const RELATIVE: &[&str] = &[
        ".ssh",
        ".gnupg",
        ".aws",
        ".azure",
        ".config/gcloud",
        ".kube",
        ".docker/config.json",
        ".netrc",
        ".git-credentials",
        ".config/gh",
        ".config/git/credentials",
        ".npmrc",
        ".pypirc",
        ".cargo/credentials",
        ".cargo/credentials.toml",
        ".claude",
        ".claude.json",
        ".password-store",
        "Library/Keychains",
        "Library/Cookies",
        "Library/Mail",
        "Library/Messages",
        "Library/Safari",
        "Library/Application Support/Google/Chrome",
        "Library/Application Support/Firefox",
        "Library/Application Support/Arc",
        "Library/Application Support/BraveSoftware",
        "Library/Application Support/Microsoft Edge",
        "Library/Group Containers",
        ".mozilla",
        ".config/google-chrome",
        ".config/chromium",
        ".local/share/keyrings",
    ];
    RELATIVE.iter().map(|r| home.join(r)).collect()
}

/// Resolve `path` like the kernel will see it: follow symlinks in the
/// existing part and keep a missing tail as written. Relative paths are taken
/// against the current directory.
pub fn resolve(path: &Path) -> PathBuf {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|d| d.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    if let Ok(p) = path.canonicalize() {
        return p;
    }
    let mut tail = Vec::new();
    let mut cur = path.as_path();
    while let Some(parent) = cur.parent() {
        if let Some(name) = cur.file_name() {
            tail.push(name.to_owned());
        }
        if let Ok(base) = parent.canonicalize() {
            let mut out = base;
            for name in tail.iter().rev() {
                out.push(name);
            }
            return out;
        }
        cur = parent;
    }
    path
}

/// Directories that must not be renamed or removed so that `path` stays
/// where it is: every ancestor except the root.
pub(crate) fn ancestors(path: &Path) -> impl Iterator<Item = &Path> {
    path.ancestors().skip(1).filter(|a| a.parent().is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_keeps_a_missing_tail() {
        let dir = tempfile::tempdir().unwrap();
        let canonical = dir.path().canonicalize().unwrap();
        assert_eq!(resolve(dir.path()), canonical);
        assert_eq!(
            resolve(&dir.path().join("a/b.toml")),
            canonical.join("a/b.toml")
        );
    }

    #[test]
    fn ancestors_skip_the_root() {
        let got: Vec<_> = ancestors(Path::new("/a/b/c.toml")).collect();
        assert_eq!(got, vec![Path::new("/a/b"), Path::new("/a")]);
    }
}
