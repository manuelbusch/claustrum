//! Recursive directory walking over a virtual file system.

use std::path::{Path, PathBuf};

use virtual_fs::FileSystem;

/// Directories that are never descended into.
const SKIPPED_DIRS: &[&str] = &[".git", "node_modules", "target", ".hg", ".svn"];

/// A file found while walking.
pub(crate) struct Found {
    /// Path inside the mount.
    pub inner: PathBuf,
    pub modified: u64,
    pub len: u64,
}

/// Walk `root` depth-first and return all regular files, sorted by path,
/// and whether the walk stopped early. Symlinks are not followed. Stops
/// after `max` entries.
pub(crate) fn walk_files(fs: &dyn FileSystem, root: &Path, max: usize) -> (Vec<Found>, bool) {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs.read_dir(&dir) else {
            continue;
        };
        let mut entries: Vec<_> = entries.filter_map(|e| e.ok()).collect();
        // Deterministic order regardless of the backing file system.
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        for entry in entries {
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            let ft = meta.file_type();
            if ft.is_dir() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if !SKIPPED_DIRS.contains(&name.as_ref()) {
                    stack.push(entry.path());
                }
            } else if ft.is_file() {
                out.push(Found {
                    inner: entry.path(),
                    modified: meta.modified(),
                    len: meta.len(),
                });
                if out.len() >= max {
                    return (out, true);
                }
            }
        }
    }
    (out, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reports_an_incomplete_walk() {
        let fs = crate::fs::mem_dir();
        for name in ["a", "b", "c", "d"] {
            crate::native::write_file(&*fs, Path::new(&format!("/{name}.txt")), b"x")
                .await
                .unwrap();
        }
        let (all, incomplete) = walk_files(&*fs, Path::new("/"), 10);
        assert_eq!((all.len(), incomplete), (4, false));
        let (some, incomplete) = walk_files(&*fs, Path::new("/"), 3);
        assert_eq!((some.len(), incomplete), (3, true));
    }
}
