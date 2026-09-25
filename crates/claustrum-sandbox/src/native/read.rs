//! `Read`: return a file's contents with line numbers.

use crate::{Error, Result, fs::GuestFs};

use super::{fs_err, locate};

/// Default number of lines returned when no limit is given.
pub const DEFAULT_LIMIT: usize = 2000;
/// Lines longer than this are cut (with a marker) to keep output bounded.
pub const MAX_LINE_CHARS: usize = 2000;

#[derive(Clone, Debug, Default)]
pub struct ReadOptions {
    /// 1-based line to start from.
    pub offset: Option<usize>,
    /// Maximum number of lines to return.
    pub limit: Option<usize>,
}

#[derive(Clone, Debug)]
pub struct ReadOutput {
    pub path: String,
    /// `cat -n` style numbered lines.
    pub content: String,
    pub total_lines: usize,
    pub first_line: usize,
    pub last_line: usize,
    pub truncated: bool,
}

pub async fn read(fs: &GuestFs, path: &str, cwd: &str, opts: ReadOptions) -> Result<ReadOutput> {
    let loc = locate(fs, path, cwd)?;
    let meta = loc.fs.metadata(&loc.inner).map_err(fs_err(&loc.guest))?;
    if meta.is_dir() {
        return Err(Error::invalid_path(
            &loc.guest,
            "is a directory; use Bash `ls` or Glob",
        ));
    }
    let bytes = super::read_for_tool(
        &loc,
        super::MAX_TOOL_FILE_BYTES,
        "read parts of it with Bash (`sed -n 'A,Bp'`, `head`, `tail`)",
    )
    .await?;
    if bytes.iter().take(8192).any(|b| *b == 0) {
        return Err(Error::invalid_path(
            &loc.guest,
            format!("appears to be a binary file ({} bytes)", bytes.len()),
        ));
    }
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let total_lines = lines.len();

    let start = opts.offset.unwrap_or(1).max(1);
    let limit = opts.limit.unwrap_or(DEFAULT_LIMIT).max(1);
    let end = (start - 1).saturating_add(limit).min(total_lines);

    let mut content = String::new();
    if start <= total_lines {
        for (idx, line) in lines[start - 1..end].iter().enumerate() {
            let n = start + idx;
            let line = line.strip_suffix('\n').unwrap_or(line);
            let line = line.strip_suffix('\r').unwrap_or(line);
            if line.chars().count() > MAX_LINE_CHARS {
                let cut: String = line.chars().take(MAX_LINE_CHARS).collect();
                content.push_str(&format!("{n:>6}\t{cut}… [line truncated]\n"));
            } else {
                content.push_str(&format!("{n:>6}\t{line}\n"));
            }
        }
    }
    Ok(ReadOutput {
        path: loc.guest,
        content,
        total_lines,
        first_line: start.min(total_lines.max(1)),
        last_line: end,
        truncated: end < total_lines,
    })
}
