//! `Grep`: regex search across files.

use std::path::Path;

use grep_matcher::Matcher;
use grep_regex::RegexMatcherBuilder;
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkContext, SinkMatch};

use crate::{Error, Result, fs::GuestFs};

use super::{
    glob::{build_glob, relative_to},
    locate,
    walk::walk_files,
};

const WALK_LIMIT: usize = 200_000;
/// Files larger than this are skipped.
const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum GrepMode {
    /// Only the paths of files with at least one match.
    #[default]
    FilesWithMatches,
    /// Matching lines with line numbers (and optional context).
    Content,
    /// Number of matching lines per file.
    Count,
}

#[derive(Clone, Debug, Default)]
pub struct GrepOptions {
    /// Directory or file to search. Defaults to the cwd.
    pub path: Option<String>,
    /// Restrict to files matching this glob (relative to `path`).
    pub glob: Option<String>,
    pub case_insensitive: bool,
    pub mode: GrepMode,
    /// Lines of context before and after each match (content mode).
    pub context: usize,
    /// Maximum number of result entries (files, lines or counts).
    pub max_results: Option<usize>,
}

#[derive(Clone, Debug)]
pub struct GrepMatch {
    pub path: String,
    pub line: u64,
    pub text: String,
    /// False for context lines.
    pub is_match: bool,
}

#[derive(Clone, Debug, Default)]
pub struct GrepOutput {
    pub mode: GrepMode,
    /// Files with matches (all modes).
    pub files: Vec<String>,
    /// Matching lines (content mode).
    pub matches: Vec<GrepMatch>,
    /// Match counts per file (count mode).
    pub counts: Vec<(String, usize)>,
    pub truncated: bool,
}

pub async fn grep(fs: &GuestFs, pattern: &str, cwd: &str, opts: GrepOptions) -> Result<GrepOutput> {
    let matcher = RegexMatcherBuilder::new()
        .case_insensitive(opts.case_insensitive)
        .line_terminator(Some(b'\n'))
        .build(pattern)
        .map_err(|e| Error::Other(format!("invalid regex `{pattern}`: {e}")))?;
    let file_filter = opts
        .glob
        .as_deref()
        .map(|g| build_glob(g).map(|g| g.compile_matcher()))
        .transpose()?;

    let loc = locate(fs, opts.path.as_deref().unwrap_or("."), cwd)?;
    let meta = loc
        .fs
        .metadata(&loc.inner)
        .map_err(super::fs_err(&loc.guest))?;
    let mut walk_incomplete = false;
    let files = if meta.is_file() {
        vec![super::walk::Found {
            inner: loc.inner.clone(),
            modified: meta.modified(),
            len: meta.len(),
        }]
    } else {
        let (mut f, incomplete) = walk_files(&*loc.fs, &loc.inner, WALK_LIMIT);
        walk_incomplete = incomplete;
        f.sort_by(|a, b| {
            b.modified
                .cmp(&a.modified)
                .then_with(|| a.inner.cmp(&b.inner))
        });
        f
    };

    let max = opts.max_results.unwrap_or(match opts.mode {
        GrepMode::Content => 500,
        _ => 1000,
    });
    let mut searcher = SearcherBuilder::new()
        .binary_detection(BinaryDetection::quit(0))
        .line_number(true)
        .before_context(if opts.mode == GrepMode::Content {
            opts.context
        } else {
            0
        })
        .after_context(if opts.mode == GrepMode::Content {
            opts.context
        } else {
            0
        })
        .build();

    let mut out = GrepOutput {
        mode: opts.mode,
        ..Default::default()
    };
    let mut entries = 0usize;

    for f in files {
        if f.len > MAX_FILE_BYTES {
            continue;
        }
        if let Some(filter) = &file_filter {
            let rel = relative_to(&f.inner, &loc.inner);
            if !filter.is_match(Path::new(rel)) {
                continue;
            }
        }
        let Ok(Some(bytes)) = super::read_file(&*loc.fs, &f.inner, MAX_FILE_BYTES).await else {
            continue;
        };
        let guest = loc.to_guest(&f.inner);
        let mut sink = Collect {
            path: &guest,
            mode: opts.mode,
            lines: Vec::new(),
            count: 0,
        };
        if search(&mut searcher, &matcher, &bytes, &mut sink).is_err() {
            continue;
        }
        if sink.count == 0 {
            continue;
        }
        out.files.push(guest.clone());
        match opts.mode {
            GrepMode::FilesWithMatches => entries += 1,
            GrepMode::Count => {
                out.counts.push((guest.clone(), sink.count));
                entries += 1;
            }
            GrepMode::Content => {
                entries += sink.lines.len();
                out.matches.append(&mut sink.lines);
            }
        }
        if entries >= max {
            out.truncated = true;
            break;
        }
    }
    // Files the walk never reached were not searched.
    out.truncated |= walk_incomplete;
    Ok(out)
}

fn search<M: Matcher>(
    searcher: &mut Searcher,
    matcher: &M,
    bytes: &[u8],
    sink: &mut Collect<'_>,
) -> std::result::Result<(), std::io::Error> {
    searcher.search_slice(matcher, bytes, sink)
}

struct Collect<'a> {
    path: &'a str,
    mode: GrepMode,
    lines: Vec<GrepMatch>,
    count: usize,
}

impl Sink for Collect<'_> {
    type Error = std::io::Error;

    fn matched(
        &mut self,
        _s: &Searcher,
        m: &SinkMatch<'_>,
    ) -> std::result::Result<bool, Self::Error> {
        self.count += 1;
        if self.mode == GrepMode::Content {
            self.lines.push(GrepMatch {
                path: self.path.to_owned(),
                line: m.line_number().unwrap_or(0),
                text: String::from_utf8_lossy(m.bytes()).trim_end().to_owned(),
                is_match: true,
            });
        }
        Ok(true)
    }

    fn context(
        &mut self,
        _s: &Searcher,
        c: &SinkContext<'_>,
    ) -> std::result::Result<bool, Self::Error> {
        if self.mode == GrepMode::Content {
            self.lines.push(GrepMatch {
                path: self.path.to_owned(),
                line: c.line_number().unwrap_or(0),
                text: String::from_utf8_lossy(c.bytes()).trim_end().to_owned(),
                is_match: false,
            });
        }
        Ok(true)
    }
}
