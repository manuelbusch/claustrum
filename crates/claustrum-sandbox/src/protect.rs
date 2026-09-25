//! Read-only protection for individual host files inside writable mounts,
//! and for whole mounts that were not declared writable ([`ReadOnlyFs`]).
//!
//! The Claustrum configuration usually lives in the project directory, which
//! the guest can write. [`ProtectedFs`] wraps a host-backed mount and refuses
//! every operation that would create, modify, replace, remove or shadow a
//! protected file, while reads keep working. It sits below every guest access
//! path (Bash via WASI syscalls, the native Write/Edit tools), so there is no
//! way around it from inside the sandbox.
//!
//! Paths are resolved on the host before the decision: symlinks are followed
//! (also dangling ones), non-existent tails are appended to the canonical
//! existing ancestor, names are compared case-insensitively where the host
//! file system usually is (macOS, Windows), and an existing protected file is
//! additionally matched by inode, which catches hard links.

use std::{
    io,
    path::{Component, Path, PathBuf},
    sync::Arc,
};

use futures::future::BoxFuture;
use virtual_fs::{
    FileOpener, FileSystem, FsError, Metadata, OpenOptions, OpenOptionsConfig, ReadDir, VirtualFile,
};

/// Symlink hops followed before giving up (and denying).
const MAX_SYMLINKS: usize = 40;

/// Resolve `path` on the host to the file an operation on it would affect.
///
/// Unlike [`std::fs::canonicalize`] this also works for paths that do not
/// exist yet and follows dangling symlinks to their (missing) target.
pub(crate) fn resolve_host_path(path: &Path) -> io::Result<PathBuf> {
    resolve(&lexical_normalize(path), 0)
}

fn resolve(path: &Path, depth: usize) -> io::Result<PathBuf> {
    if depth > MAX_SYMLINKS {
        return Err(io::Error::other("too many levels of symbolic links"));
    }
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            let target = std::fs::read_link(path)?;
            let base = path.parent().unwrap_or(Path::new("/"));
            let parent = resolve(base, depth + 1)?;
            resolve(&lexical_normalize(&parent.join(target)), depth + 1)
        }
        Ok(_) => std::fs::canonicalize(path),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
                return Ok(path.to_path_buf());
            };
            Ok(resolve(parent, depth)?.join(name))
        }
        Err(e) => Err(e),
    }
}

/// Resolve `.` and `..` without touching the file system. `..` at the root
/// stays at the root, like the host file system of a mount does.
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::Prefix(p) => out.push(p.as_os_str()),
            Component::RootDir => out.push("/"),
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(s) => out.push(s),
        }
    }
    out
}

/// Comparison key: case-folded where host file systems are usually
/// case-insensitive, so `CLAUSTRUM.TOML` cannot sneak past on macOS.
pub(crate) fn key(path: &Path) -> PathBuf {
    if cfg!(any(target_os = "macos", windows)) {
        PathBuf::from(path.to_string_lossy().to_lowercase())
    } else {
        path.to_path_buf()
    }
}

#[cfg(unix)]
fn file_id(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).ok().map(|m| (m.dev(), m.ino()))
}

#[cfg(not(unix))]
fn file_id(_path: &Path) -> Option<(u64, u64)> {
    None
}

/// A host-backed mount with some files made read-only.
#[derive(Debug)]
pub(crate) struct ProtectedFs {
    inner: virtual_fs::host_fs::FileSystem,
    /// Canonical host root of the mount.
    root: PathBuf,
    /// Resolved host paths of the protected files, all under `root`.
    protected: Vec<PathBuf>,
}

impl ProtectedFs {
    pub(crate) fn new(
        inner: virtual_fs::host_fs::FileSystem,
        root: PathBuf,
        protected: Vec<PathBuf>,
    ) -> Self {
        Self {
            inner,
            root,
            protected,
        }
    }

    /// Whether writing the file at `path` (inside the mount) is refused.
    pub(crate) fn is_protected(&self, path: &Path) -> bool {
        self.check_file(path).is_err()
    }

    /// Host path of a mount-relative path, resolved like the host would.
    fn host(&self, path: &Path) -> Option<PathBuf> {
        let inside = lexical_normalize(&Path::new("/").join(path));
        let rel = inside.strip_prefix("/").unwrap_or(&inside);
        resolve_host_path(&self.root.join(rel)).ok()
    }

    /// Deny writing, creating or removing the file at `path`.
    fn check_file(&self, path: &Path) -> virtual_fs::Result<()> {
        // Unresolvable paths (symlink loops, unreadable directories) are
        // denied rather than guessed at.
        let Some(host) = self.host(path) else {
            return self.deny(path);
        };
        let k = key(&host);
        if self.protected.iter().any(|p| key(p) == k) {
            return self.deny(path);
        }
        if let Some(id) = file_id(&host)
            && self.protected.iter().any(|p| file_id(p) == Some(id))
        {
            return self.deny(path);
        }
        Ok(())
    }

    /// Deny renaming or removing `path` if it is, or contains, a protected
    /// file.
    fn check_tree(&self, path: &Path) -> virtual_fs::Result<()> {
        self.check_file(path)?;
        let Some(host) = self.host(path) else {
            return self.deny(path);
        };
        let k = key(&host);
        if self.protected.iter().any(|p| key(p).starts_with(&k)) {
            return self.deny(path);
        }
        Ok(())
    }

    fn deny(&self, path: &Path) -> virtual_fs::Result<()> {
        tracing::warn!(path = %path.display(), root = %self.root.display(), "write to a protected file refused");
        Err(FsError::PermissionDenied)
    }
}

impl FileSystem for ProtectedFs {
    fn readlink(&self, path: &Path) -> virtual_fs::Result<PathBuf> {
        self.inner.readlink(path)
    }

    fn read_dir(&self, path: &Path) -> virtual_fs::Result<ReadDir> {
        self.inner.read_dir(path)
    }

    fn create_dir(&self, path: &Path) -> virtual_fs::Result<()> {
        self.check_file(path)?;
        self.inner.create_dir(path)
    }

    fn create_symlink(&self, source: &Path, target: &Path) -> virtual_fs::Result<()> {
        // `target` is the new link. A link *to* a protected file is harmless:
        // writes through it resolve to the protected path and are refused.
        self.check_file(target)?;
        self.inner.create_symlink(source, target)
    }

    fn hard_link(&self, source: &Path, target: &Path) -> virtual_fs::Result<()> {
        // A second name for a protected file would be a writable alias.
        self.check_file(source)?;
        self.check_file(target)?;
        self.inner.hard_link(source, target)
    }

    fn remove_dir(&self, path: &Path) -> virtual_fs::Result<()> {
        self.check_tree(path)?;
        self.inner.remove_dir(path)
    }

    fn rename<'a>(&'a self, from: &'a Path, to: &'a Path) -> BoxFuture<'a, virtual_fs::Result<()>> {
        Box::pin(async move {
            self.check_tree(from)?;
            self.check_tree(to)?;
            self.inner.rename(from, to).await
        })
    }

    fn metadata(&self, path: &Path) -> virtual_fs::Result<Metadata> {
        self.inner.metadata(path)
    }

    fn symlink_metadata(&self, path: &Path) -> virtual_fs::Result<Metadata> {
        self.inner.symlink_metadata(path)
    }

    fn remove_file(&self, path: &Path) -> virtual_fs::Result<()> {
        self.check_file(path)?;
        self.inner.remove_file(path)
    }

    fn new_open_options(&self) -> OpenOptions<'_> {
        OpenOptions::new(self)
    }
}

impl FileOpener for ProtectedFs {
    fn open(
        &self,
        path: &Path,
        conf: &OpenOptionsConfig,
    ) -> virtual_fs::Result<Box<dyn VirtualFile + Send + Sync + 'static>> {
        // WASIX first tries a read/write handle for every regular file and
        // falls back to the requested mode on PermissionDenied, so refusing
        // here keeps plain reads working.
        if conf.would_mutate() {
            self.check_file(path)?;
        }
        self.inner
            .new_open_options()
            .options(conf.clone())
            .open(path)
    }
}

/// A mount the guest can read but not change at all.
///
/// Wraps the host-backed file system of an extra mount that was not declared
/// writable (possibly a [`ProtectedFs`]).
#[derive(Debug)]
pub(crate) struct ReadOnlyFs {
    inner: Arc<dyn FileSystem + Send + Sync>,
}

impl ReadOnlyFs {
    pub(crate) fn new(inner: Arc<dyn FileSystem + Send + Sync>) -> Self {
        Self { inner }
    }

    fn deny(&self, path: &Path) -> virtual_fs::Result<()> {
        tracing::warn!(path = %path.display(), "write to a read-only mount refused");
        Err(FsError::PermissionDenied)
    }
}

impl FileSystem for ReadOnlyFs {
    fn readlink(&self, path: &Path) -> virtual_fs::Result<PathBuf> {
        self.inner.readlink(path)
    }

    fn read_dir(&self, path: &Path) -> virtual_fs::Result<ReadDir> {
        self.inner.read_dir(path)
    }

    fn create_dir(&self, path: &Path) -> virtual_fs::Result<()> {
        self.deny(path)
    }

    fn create_symlink(&self, _source: &Path, target: &Path) -> virtual_fs::Result<()> {
        self.deny(target)
    }

    fn hard_link(&self, _source: &Path, target: &Path) -> virtual_fs::Result<()> {
        self.deny(target)
    }

    fn remove_dir(&self, path: &Path) -> virtual_fs::Result<()> {
        self.deny(path)
    }

    fn rename<'a>(
        &'a self,
        from: &'a Path,
        _to: &'a Path,
    ) -> BoxFuture<'a, virtual_fs::Result<()>> {
        Box::pin(async move { self.deny(from) })
    }

    fn metadata(&self, path: &Path) -> virtual_fs::Result<Metadata> {
        self.inner.metadata(path)
    }

    fn symlink_metadata(&self, path: &Path) -> virtual_fs::Result<Metadata> {
        self.inner.symlink_metadata(path)
    }

    fn remove_file(&self, path: &Path) -> virtual_fs::Result<()> {
        self.deny(path)
    }

    fn new_open_options(&self) -> OpenOptions<'_> {
        OpenOptions::new(self)
    }
}

impl FileOpener for ReadOnlyFs {
    fn open(
        &self,
        path: &Path,
        conf: &OpenOptionsConfig,
    ) -> virtual_fs::Result<Box<dyn VirtualFile + Send + Sync + 'static>> {
        // See `ProtectedFs::open`: WASIX falls back to a read-only handle.
        if conf.would_mutate() {
            self.deny(path)?;
        }
        self.inner
            .new_open_options()
            .options(conf.clone())
            .open(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (tempfile::TempDir, ProtectedFs, tokio::runtime::Runtime) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(root.join("claustrum.toml"), "marker").unwrap();
        std::fs::create_dir(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/deep.toml"), "deep").unwrap();
        let inner = virtual_fs::host_fs::FileSystem::new(rt.handle().clone(), &root).unwrap();
        let protected = vec![
            resolve_host_path(&root.join("claustrum.toml")).unwrap(),
            resolve_host_path(&root.join("sub/deep.toml")).unwrap(),
            resolve_host_path(&root.join("missing.toml")).unwrap(),
        ];
        let fs = ProtectedFs::new(inner, root, protected);
        (dir, fs, rt)
    }

    fn write_open(fs: &ProtectedFs, path: &str) -> virtual_fs::Result<()> {
        fs.new_open_options()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .map(|_| ())
    }

    #[test]
    fn reads_are_allowed() {
        let (_d, fs, _rt) = setup();
        assert!(
            fs.new_open_options()
                .read(true)
                .open("/claustrum.toml")
                .is_ok()
        );
        assert!(fs.metadata(Path::new("/claustrum.toml")).is_ok());
        assert!(fs.read_dir(Path::new("/")).is_ok());
    }

    #[test]
    fn mutations_of_protected_files_are_denied() {
        let (dir, fs, rt) = setup();
        for p in [
            "/claustrum.toml",
            "/./sub/../claustrum.toml",
            "/CLAUSTRUM.toml",
            "/sub/deep.toml",
            "/missing.toml",
        ] {
            if cfg!(not(target_os = "macos")) && p == "/CLAUSTRUM.toml" {
                continue;
            }
            assert!(
                matches!(write_open(&fs, p), Err(FsError::PermissionDenied)),
                "open {p}"
            );
        }
        assert!(fs.remove_file(Path::new("/claustrum.toml")).is_err());
        assert!(fs.create_dir(Path::new("/missing.toml")).is_err());
        assert!(fs.remove_dir(Path::new("/sub")).is_err());
        let rename =
            |a: &'static str, b: &'static str| rt.block_on(fs.rename(Path::new(a), Path::new(b)));
        assert!(rename("/claustrum.toml", "/x").is_err());
        assert!(rename("/sub", "/other").is_err());
        std::fs::write(dir.path().join("evil"), "x").unwrap();
        assert!(rename("/evil", "/claustrum.toml").is_err());
        assert!(rename("/evil", "/missing.toml").is_err());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("claustrum.toml")).unwrap(),
            "marker"
        );
        assert!(!dir.path().join("missing.toml").exists());
    }

    #[cfg(unix)]
    #[test]
    fn host_links_are_followed() {
        let (dir, fs, _rt) = setup();
        let root = dir.path();
        std::os::unix::fs::symlink("claustrum.toml", root.join("link")).unwrap();
        std::os::unix::fs::symlink("missing.toml", root.join("dangling")).unwrap();
        std::os::unix::fs::symlink("sub", root.join("subdir")).unwrap();
        std::fs::hard_link(root.join("claustrum.toml"), root.join("hard")).unwrap();
        for p in ["/link", "/dangling", "/subdir/deep.toml", "/hard"] {
            assert!(
                matches!(write_open(&fs, p), Err(FsError::PermissionDenied)),
                "open {p}"
            );
        }
        assert!(!root.join("missing.toml").exists());
    }

    #[test]
    fn other_files_stay_writable() {
        let (dir, fs, rt) = setup();
        write_open(&fs, "/notes.txt").unwrap();
        write_open(&fs, "/sub/other.toml").unwrap();
        fs.create_dir(Path::new("/newdir")).unwrap();
        rt.block_on(fs.rename(Path::new("/notes.txt"), Path::new("/newdir/n.txt")))
            .unwrap();
        fs.remove_file(Path::new("/newdir/n.txt")).unwrap();
        fs.remove_dir(Path::new("/newdir")).unwrap();
        assert!(dir.path().join("sub/other.toml").exists());
    }

    #[test]
    fn read_only_mounts_refuse_every_change() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(root.join("data.txt"), "data").unwrap();
        std::fs::create_dir(root.join("sub")).unwrap();
        let inner = virtual_fs::host_fs::FileSystem::new(rt.handle().clone(), &root).unwrap();
        let fs = ReadOnlyFs::new(Arc::new(inner));

        assert!(fs.new_open_options().read(true).open("/data.txt").is_ok());
        assert!(fs.metadata(Path::new("/data.txt")).is_ok());
        assert!(fs.read_dir(Path::new("/")).is_ok());

        let denied = |r: virtual_fs::Result<()>| matches!(r, Err(FsError::PermissionDenied));
        let open = |path: &str, append: bool| {
            fs.new_open_options()
                .write(!append)
                .append(append)
                .create(true)
                .open(path)
                .map(|_| ())
        };
        assert!(denied(open("/data.txt", false)));
        assert!(denied(open("/data.txt", true)));
        assert!(denied(open("/new.txt", false)));
        assert!(denied(fs.create_dir(Path::new("/newdir"))));
        assert!(denied(fs.remove_file(Path::new("/data.txt"))));
        assert!(denied(fs.remove_dir(Path::new("/sub"))));
        assert!(denied(
            fs.create_symlink(Path::new("data.txt"), Path::new("/link"))
        ));
        assert!(denied(
            fs.hard_link(Path::new("/data.txt"), Path::new("/hard"))
        ));
        assert!(denied(rt.block_on(
            fs.rename(Path::new("/data.txt"), Path::new("/moved"))
        )));

        assert_eq!(
            std::fs::read_to_string(root.join("data.txt")).unwrap(),
            "data"
        );
        let names: Vec<_> = std::fs::read_dir(&root)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 2, "{names:?}");
    }
}
