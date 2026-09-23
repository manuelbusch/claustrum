//! `Glob`: find files by pattern.

use std::path::Path;

use globset::{Glob, GlobBuilder};

use crate::{Error, Result, fs::GuestFs};

use super::{locate, walk::walk_files};

/// Maximum number of paths returned.
pub const MAX_RESULTS: usize = 1000;
const WALK_LIMIT: usize = 200_000;

#[derive(Clone, Debug)]
pub struct GlobOutput {
    /// Matching guest paths, most recently modified first.
    pub paths: Vec<String>,
    pub truncated: bool,
}

/// Match `pattern` (e.g. `**/*.rs`, `src/**/*.test.ts`) against files below
/// `root` (defaults to the cwd). Patterns are matched against paths relative
/// to `root`; an absolute pattern is matched against the full guest path.
pub fn glob(fs: &GuestFs, pattern: &str, root: Option<&str>, cwd: &str) -> Result<GlobOutput> {
    let loc = locate(fs, root.unwrap_or("."), cwd)?;
    let matcher = build_glob(pattern)?.compile_matcher();
    let absolute = pattern.starts_with('/');

    let mut found = walk_files(&*loc.fs, &loc.inner, WALK_LIMIT);
    found.sort_by(|a, b| {
        b.modified
            .cmp(&a.modified)
            .then_with(|| a.inner.cmp(&b.inner))
    });

    let mut paths = Vec::new();
    let mut truncated = false;
    for f in found {
        let guest = loc.to_guest(&f.inner);
        let candidate: &str = if absolute {
            &guest
        } else {
            relative_to(&f.inner, &loc.inner)
        };
        if matcher.is_match(Path::new(candidate)) {
            if paths.len() >= MAX_RESULTS {
                truncated = true;
                break;
            }
            paths.push(guest);
        }
    }
    Ok(GlobOutput { paths, truncated })
}

pub(crate) fn build_glob(pattern: &str) -> Result<Glob> {
    GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .map_err(|e| Error::Other(format!("invalid glob pattern `{pattern}`: {e}")))
}

/// `inner` relative to `root`, without a leading slash.
pub(crate) fn relative_to<'a>(inner: &'a Path, root: &Path) -> &'a str {
    let s = inner.to_str().unwrap_or("");
    let r = root.to_str().unwrap_or("/");
    let rest = if r == "/" {
        s
    } else {
        s.strip_prefix(r).unwrap_or(s)
    };
    rest.trim_start_matches('/')
}
