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
    if crate::fs::leaves_mount(&*mount.fs, &inner) {
        return Err(Error::invalid_path(
            &guest,
            "a symbolic link on the host leads outside the sandbox mounts",
        ));
    }
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

/// Largest file `Read` and `Edit` load. Beyond it, Bash (`head`, `sed -n`,
/// `python`) works on the file in pieces.
pub const MAX_TOOL_FILE_BYTES: u64 = 64 * 1024 * 1024;

/// Read a whole file from a virtual file system, at most `max` bytes (the
/// reported size is not trusted: special files report 0).
pub async fn read_file(
    fs: &dyn FileSystem,
    path: &std::path::Path,
    max: u64,
) -> std::result::Result<Option<Vec<u8>>, virtual_fs::FsError> {
    use tokio::io::AsyncReadExt;
    let f = fs.new_open_options().read(true).open(path)?;
    let size = f.size();
    if size > max {
        return Ok(None);
    }
    let mut buf = Vec::with_capacity(size as usize);
    f.take(max + 1).read_to_end(&mut buf).await?;
    Ok((buf.len() as u64 <= max).then_some(buf))
}

/// [`read_file`] for a tool: a file above `max` is an error that says what
/// to do instead.
pub(crate) async fn read_for_tool(loc: &Located, max: u64, instead: &str) -> Result<Vec<u8>> {
    read_file(&*loc.fs, &loc.inner, max)
        .await
        .map_err(fs_err(&loc.guest))?
        .ok_or_else(|| {
            Error::invalid_path(
                &loc.guest,
                format!("is larger than {} MiB; {instead}", max / (1024 * 1024)),
            )
        })
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
