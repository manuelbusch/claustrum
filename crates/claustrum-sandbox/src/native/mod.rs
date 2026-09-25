//! Native tools: host-side implementations of read, write, edit, glob and
//! grep that operate directly on the guest file system.
//!
//! These exist so that the most frequent operations do not need a guest
//! process at all, and so that they behave like Claude Code's built-in tools
//! of the same name. All paths are guest paths; relative paths are resolved
//! against the sandbox's current working directory.

mod edit;
mod glob;
mod grep;
mod read;
mod walk;
mod write;

pub(crate) use edit::apply as apply_edit;
pub use edit::{EditOutput, edit};
pub use glob::{GlobOutput, glob};
pub use grep::{GrepMatch, GrepMode, GrepOptions, GrepOutput, grep};
pub use read::{ReadOptions, ReadOutput, read};
pub use write::{WriteOutput, write};

use std::{path::PathBuf, sync::Arc};

use virtual_fs::FileSystem;

use crate::{
    Error, Result,
    fs::{GuestFs, normalize_guest_path},
};

/// A guest path resolved to its backing file system.
pub(crate) struct Located {
    /// Normalised absolute guest path (for messages and results).
    pub guest: String,
    /// The mount's guest prefix, e.g. `/workspace`.
    pub mount: String,
    pub fs: Arc<dyn FileSystem + Send + Sync>,
    /// Path inside the mount, starting with `/`.
    pub inner: PathBuf,
}

impl Located {
    /// Convert a path inside this mount back to a guest path.
    pub fn to_guest(&self, inner: &std::path::Path) -> String {
        let rest = inner.to_string_lossy();
        let rest = rest.trim_start_matches('/');
        if rest.is_empty() {
            self.mount.clone()
        } else if self.mount == "/" {
            format!("/{rest}")
        } else {
            format!("{}/{rest}", self.mount)
        }
    }
}

pub(crate) fn locate(fs: &GuestFs, path: &str, cwd: &str) -> Result<Located> {
    let guest = normalize_guest_path(path, cwd)?;
    let (mount, inner) = fs.resolve(&guest)?;
    Ok(Located {
        guest,
        mount: mount.guest.clone(),
        fs: Arc::clone(&mount.fs),
        inner,
    })
}

/// Refuse early, with a clear message, to change a file on a read-only mount
/// or a protected file (the mount would refuse anyway, but only with
/// "permission denied").
pub(crate) fn ensure_writable(loc: &Located) -> Result<()> {
    if crate::fs::is_read_only(&*loc.fs) {
        return Err(Error::invalid_path(
            &loc.guest,
            format!(
                "the mount {} is read-only; ask the user to make it writable",
                loc.mount
            ),
        ));
    }
    if crate::fs::is_protected(&*loc.fs, &loc.inner) {
        return Err(Error::invalid_path(
            &loc.guest,
            "this file is protected by the sandbox and read-only; ask the user to change it",
        ));
    }
    Ok(())
}

pub(crate) fn fs_err(path: &str) -> impl FnOnce(virtual_fs::FsError) -> Error + '_ {
    move |e| Error::fs(path, e)
}

/// Read a whole file from a virtual file system.
pub async fn read_file(
    fs: &dyn FileSystem,
    path: &std::path::Path,
) -> std::result::Result<Vec<u8>, virtual_fs::FsError> {
    use tokio::io::AsyncReadExt;
    let mut f = fs.new_open_options().read(true).open(path)?;
    let mut buf = Vec::with_capacity(f.size() as usize);
    f.read_to_end(&mut buf).await?;
    Ok(buf)
}

/// Create or truncate a file and write `data` to it.
pub async fn write_file(
    fs: &dyn FileSystem,
    path: &std::path::Path,
    data: &[u8],
) -> std::result::Result<(), virtual_fs::FsError> {
    use tokio::io::AsyncWriteExt;
    let mut f = fs
        .new_open_options()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)?;
    f.write_all(data).await?;
    f.flush().await?;
    Ok(())
}
