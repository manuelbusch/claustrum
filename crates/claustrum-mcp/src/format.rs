//! Rendering of tool results as text for the model.

use claustrum_sandbox::{
    ExecOutput, ExitReason,
    native::{EditOutput, GlobOutput, GrepMode, GrepOutput, ReadOutput, WriteOutput},
};

/// Maximum characters of stdout/stderr forwarded per stream.
pub const MAX_STREAM_CHARS: usize = 100_000;

pub fn exec(out: &ExecOutput) -> String {
    let mut s = String::new();
    let stdout = clip(&out.stdout_lossy(), out.stdout_truncated);
    let stderr = clip(&out.stderr_lossy(), out.stderr_truncated);
    if !stdout.is_empty() {
        s.push_str(&stdout);
        if !s.ends_with('\n') {
            s.push('\n');
        }
    }
    if !stderr.is_empty() {
        s.push_str("--- stderr ---\n");
        s.push_str(&stderr);
        if !s.ends_with('\n') {
            s.push('\n');
        }
    }
    match out.reason {
        ExitReason::TimedOut => s.push_str(&format!(
            "[command timed out after {:.1?} and was killed]\n",
            out.duration
        )),
        ExitReason::Exited if out.exit_code != 0 => {
            s.push_str(&format!("[exit code {}]\n", out.exit_code));
        }
        ExitReason::Exited => {}
    }
    if s.is_empty() {
        s.push_str("(no output)\n");
    }
    s
}

fn clip(text: &str, already_truncated: bool) -> String {
    let mut clipped: String = text.chars().take(MAX_STREAM_CHARS).collect();
    if clipped.len() < text.len() || already_truncated {
        clipped.push_str("\n… [output truncated]");
    }
    clipped
}

pub fn read(out: &ReadOutput) -> String {
    let mut s = out.content.clone();
    if out.total_lines == 0 {
        s.push_str("(empty file)\n");
    }
    if out.truncated {
        s.push_str(&format!(
            "\n[showing lines {}-{} of {}; use offset/limit to read more]\n",
            out.first_line, out.last_line, out.total_lines
        ));
    }
    s
}

pub fn write(out: &WriteOutput) -> String {
    format!(
        "{} {} ({} bytes)",
        if out.created { "Created" } else { "Updated" },
        out.path,
        out.bytes
    )
}

pub fn edit(out: &EditOutput) -> String {
    format!(
        "Applied {} replacement{} to {}. Excerpt:\n{}",
        out.replacements,
        if out.replacements == 1 { "" } else { "s" },
        out.path,
        out.snippet
    )
}

pub fn glob(out: &GlobOutput) -> String {
    if out.paths.is_empty() {
        return "No files found".into();
    }
    let mut s = out.paths.join("\n");
    if out.truncated {
        s.push_str("\n[result list truncated; narrow the pattern]");
    }
    s
}

pub fn grep(out: &GrepOutput) -> String {
    let mut s = match out.mode {
        GrepMode::FilesWithMatches => {
            if out.files.is_empty() {
                "No matches found".to_owned()
            } else {
                format!(
                    "Found {} file{}\n{}",
                    out.files.len(),
                    plural(out.files.len()),
                    out.files.join("\n")
                )
            }
        }
        GrepMode::Count => {
            if out.counts.is_empty() {
                "No matches found".to_owned()
            } else {
                out.counts
                    .iter()
                    .map(|(p, n)| format!("{p}:{n}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        }
        GrepMode::Content => {
            if out.matches.is_empty() {
                "No matches found".to_owned()
            } else {
                let mut lines = Vec::with_capacity(out.matches.len());
                let mut last_path = "";
                for m in &out.matches {
                    if m.path != last_path {
                        if !last_path.is_empty() {
                            lines.push("--".to_owned());
                        }
                        last_path = &m.path;
                    }
                    let sep = if m.is_match { ':' } else { '-' };
                    lines.push(format!("{}{sep}{}{sep}{}", m.path, m.line, m.text));
                }
                lines.join("\n")
            }
        }
    };
    if out.truncated {
        s.push_str("\n[results truncated; refine the pattern or path]");
    }
    s
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}
