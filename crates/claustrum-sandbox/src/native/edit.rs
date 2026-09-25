//! `Edit`: exact string replacement in a file.

use crate::{Error, Result, fs::GuestFs};

use super::{fs_err, locate};

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct EditOutput {
    pub path: String,
    pub replacements: usize,
    /// A numbered excerpt of the file around the first replacement.
    pub snippet: String,
}

pub async fn edit(
    fs: &GuestFs,
    path: &str,
    cwd: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
) -> Result<EditOutput> {
    check_strings(old_string, new_string)?;
    let loc = locate(fs, path, cwd)?;
    super::ensure_writable(&loc)?;
    let bytes = super::read_for_tool(
        &loc,
        super::MAX_TOOL_FILE_BYTES,
        "edit it with Bash (`sed -i`, `python`)",
    )
    .await?;
    let text = String::from_utf8(bytes)
        .map_err(|_| Error::invalid_path(&loc.guest, "file is not valid UTF-8"))?;
    let (updated, out) = apply(&text, &loc.guest, old_string, new_string, replace_all)?;
    super::write_file(&*loc.fs, &loc.inner, updated.as_bytes())
        .await
        .map_err(fs_err(&loc.guest))?;
    Ok(out)
}

fn check_strings(old_string: &str, new_string: &str) -> Result<()> {
    if old_string.is_empty() {
        return Err(Error::Other("old_string must not be empty".into()));
    }
    if old_string == new_string {
        return Err(Error::Other(
            "old_string and new_string are identical".into(),
        ));
    }
    Ok(())
}

/// The replacement itself: the new text of the file at `path` and the
/// report for it.
pub(crate) fn apply(
    text: &str,
    path: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
) -> Result<(String, EditOutput)> {
    check_strings(old_string, new_string)?;
    let count = text.matches(old_string).count();
    if count == 0 {
        return Err(Error::Other(format!(
            "old_string not found in {path}. Make sure it matches the file contents exactly, including whitespace."
        )));
    }
    if count > 1 && !replace_all {
        return Err(Error::Other(format!(
            "old_string occurs {count} times in {path}. Provide more surrounding context to make it unique, or set replace_all."
        )));
    }

    let first = text.find(old_string).expect("count > 0");
    let updated = if replace_all {
        text.replace(old_string, new_string)
    } else {
        text.replacen(old_string, new_string, 1)
    };
    let snippet = snippet_around(&updated, first, new_string.len(), 4);
    let out = EditOutput {
        path: path.to_owned(),
        replacements: if replace_all { count } else { 1 },
        snippet,
    };
    Ok((updated, out))
}

/// Numbered lines around `[start, start + len)`, with `context` lines on
/// each side.
fn snippet_around(text: &str, start: usize, len: usize, context: usize) -> String {
    let line_of = |pos: usize| text[..pos.min(text.len())].matches('\n').count();
    let first_line = line_of(start).saturating_sub(context);
    let last_line = line_of(start + len) + context;
    text.lines()
        .enumerate()
        .skip(first_line)
        .take(last_line - first_line + 1)
        .map(|(i, l)| format!("{:>6}\t{l}", i + 1))
        .collect::<Vec<_>>()
        .join("\n")
}
