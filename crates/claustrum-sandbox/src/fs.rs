//! Guest file system: mounts and path handling.
//!
//! The guest sees a small in-memory root (`/bin`, `/tmp`, ...) with the host
//! project directory mounted read/write at [`WORKSPACE`]. Native tools
//! address files by guest path and are routed to the correct mount here.

use std::{
    path::{Component, Path, PathBuf},
    sync::Arc,
};

use virtual_fs::FileSystem;

use crate::{Error, Result, WORKSPACE};

/// A file system mounted at a guest path.
#[derive(Clone)]
pub struct Mount {
    /// Absolute guest path, e.g. `/workspace`.
    pub guest: String,
    pub fs: Arc<dyn FileSystem + Send + Sync>,
}

impl std::fmt::Debug for Mount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Mount").field("guest", &self.guest).finish()
    }
}

/// The guest view of all persistent mounts, as seen by native tools.
#[derive(Clone, Debug, Default)]
pub struct GuestFs {
    mounts: Vec<Mount>,
}

impl GuestFs {
    pub fn new(mut mounts: Vec<Mount>) -> Self {
        // Longest prefix first so nested mounts win.
        mounts.sort_by_key(|m| std::cmp::Reverse(m.guest.len()));
        Self { mounts }
    }

    pub fn mounts(&self) -> &[Mount] {
        &self.mounts
    }

    /// Resolve an absolute guest path to the mount that backs it and the path
    /// inside that mount (always starting with `/`).
    pub fn resolve(&self, guest_path: &str) -> Result<(&Mount, PathBuf)> {
        let normalized = normalize_guest_path(guest_path, WORKSPACE)?;
        for mount in &self.mounts {
            if let Some(rest) = strip_mount_prefix(&normalized, &mount.guest) {
                return Ok((mount, rest));
            }
        }
        Err(Error::invalid_path(
            guest_path,
            format!(
                "path is outside the sandbox mounts ({})",
                self.mounts
                    .iter()
                    .map(|m| m.guest.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ))
    }
}

fn strip_mount_prefix(path: &str, mount: &str) -> Option<PathBuf> {
    if path == mount {
        return Some(PathBuf::from("/"));
    }
    let rest = path.strip_prefix(mount)?;
    if rest.starts_with('/') {
        Some(PathBuf::from(rest))
    } else {
        None
    }
}

/// Normalise a guest path lexically: make it absolute relative to `cwd`,
/// resolve `.` and `..`, and reject attempts to climb above `/`.
pub fn normalize_guest_path(path: &str, cwd: &str) -> Result<String> {
    if path.is_empty() {
        return Err(Error::invalid_path(path, "path is empty"));
    }
    let joined = if path.starts_with('/') {
        PathBuf::from(path)
    } else {
        Path::new(cwd).join(path)
    };
    let mut parts: Vec<String> = Vec::new();
    for component in joined.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(s) => parts.push(s.to_string_lossy().into_owned()),
            Component::ParentDir => {
                if parts.pop().is_none() {
                    return Err(Error::invalid_path(path, "path escapes the root"));
                }
            }
            Component::Prefix(_) => {
                return Err(Error::invalid_path(
                    path,
                    "Windows prefixes are not allowed",
                ));
            }
        }
    }
    Ok(format!("/{}", parts.join("/")))
}

/// Create a host-backed file system rooted at `dir`.
///
/// `protected` lists resolved host paths (see
/// [`SandboxBuilder::protect`](crate::SandboxBuilder::protect)) that must stay
/// read-only; those under `dir` are enforced by wrapping the mount.
pub fn host_dir(
    handle: tokio::runtime::Handle,
    dir: &Path,
    protected: &[PathBuf],
) -> Result<Arc<dyn FileSystem + Send + Sync>> {
    let canonical = dir
        .canonicalize()
        .map_err(|e| Error::Init(format!("cannot resolve {}: {e}", dir.display())))?;
    if !canonical.is_dir() {
        return Err(Error::Init(format!("{} is not a directory", dir.display())));
    }
    let fs = virtual_fs::host_fs::FileSystem::new(handle, &canonical)
        .map_err(|e| Error::Init(format!("cannot mount {}: {e}", canonical.display())))?;
    let inside: Vec<PathBuf> = protected
        .iter()
        .filter(|p| crate::protect::key(p).starts_with(crate::protect::key(&canonical)))
        .cloned()
        .collect();
    if inside.is_empty() {
        Ok(Arc::new(fs))
    } else {
        Ok(Arc::new(crate::protect::ProtectedFs::new(
            fs, canonical, inside,
        )))
    }
}

/// Whether `inner` (a path inside the mount `fs`) is a protected, read-only
/// file.
pub(crate) fn is_protected(fs: &(dyn FileSystem + Send + Sync), inner: &Path) -> bool {
    let fs: &dyn FileSystem = fs;
    fs.downcast_ref::<crate::protect::ProtectedFs>()
        .is_some_and(|p| p.is_protected(inner))
}

/// Create an empty in-memory file system.
pub fn mem_dir() -> Arc<dyn FileSystem + Send + Sync> {
    Arc::new(virtual_fs::mem_fs::FileSystem::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_paths() {
        assert_eq!(
            normalize_guest_path("a/b", "/workspace").unwrap(),
            "/workspace/a/b"
        );
        assert_eq!(normalize_guest_path("/x/./y/../z", "/").unwrap(), "/x/z");
        assert_eq!(normalize_guest_path("..", "/workspace").unwrap(), "/");
        assert!(normalize_guest_path("../..", "/workspace").is_err());
        assert_eq!(normalize_guest_path("/", "/").unwrap(), "/");
    }

    #[test]
    fn resolves_mounts() {
        let fs = GuestFs::new(vec![
            Mount {
                guest: "/workspace".into(),
                fs: mem_dir(),
            },
            Mount {
                guest: "/tmp".into(),
                fs: mem_dir(),
            },
        ]);
        let (m, p) = fs.resolve("/workspace/src/main.rs").unwrap();
        assert_eq!(m.guest, "/workspace");
        assert_eq!(p, PathBuf::from("/src/main.rs"));
        let (m, p) = fs.resolve("/workspace").unwrap();
        assert_eq!(m.guest, "/workspace");
        assert_eq!(p, PathBuf::from("/"));
        assert!(fs.resolve("/workspacex").is_err());
        assert!(fs.resolve("/etc/passwd").is_err());
    }
}
