//! Action definitions as written in the configuration, and their compiled,
//! validated form.
//!
//! An [`ActionDef`] is what `claustrum.toml` holds. [`ActionDef::compile`]
//! turns it into an [`ActionSpec`] with the program resolved, the working
//! directory pinned inside the workspace, regexes compiled and placeholders
//! checked, so that everything that can be wrong with a definition is
//! reported when Claustrum starts, not when Claude triggers the action.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    time::Duration,
};

use regex::Regex;
use serde::Deserialize;

/// Upper bound for input values when the definition does not set `max_len`.
pub const DEFAULT_MAX_LEN: usize = 256;
/// Largest `max_len` a definition may ask for.
const MAX_MAX_LEN: usize = 64 * 1024;

/// One action as declared under `[[actions.action]]`.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ActionDef {
    /// Name used to trigger the action: `[a-z][a-z0-9-]{0,31}`.
    pub name: String,
    /// Shown to Claude in listings and tool descriptions.
    #[serde(default)]
    pub description: String,
    /// The host argv. `command[0]` is an absolute path or a bare name looked
    /// up on `PATH` at startup; the rest may contain `{input}` placeholders.
    #[serde(default)]
    pub command: Vec<String>,
    /// Working directory, relative to the workspace. Defaults to the
    /// workspace itself and must stay inside it.
    pub cwd: Option<PathBuf>,
    /// Wall-clock limit; `0` means none. Defaults to the sandbox timeout.
    pub timeout_secs: Option<u64>,
    /// Bytes kept per output stream. Defaults to the sandbox limit.
    pub max_output_bytes: Option<usize>,
    /// Environment variables set for the program (values may use
    /// placeholders). Nothing else from the host environment is inherited
    /// except `PATH`, `HOME` and `LANG`.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Host environment variables forwarded by name.
    #[serde(default)]
    pub env_passthrough: Vec<String>,
    /// Confinement of the host process: `"os"` (default) runs it in the
    /// operating system sandbox when `[sandbox] confinement` allows it,
    /// `"none"` opts this action out.
    #[serde(default)]
    pub confine: Confine,
    /// Host directories the confined program may write besides the
    /// workspace and its private temporary directory, e.g. a package cache
    /// (`~/.cargo/registry`). `~/` is expanded; relative paths are taken
    /// against the workspace.
    #[serde(default)]
    pub writable: Vec<PathBuf>,
    /// Inputs the guest may supply, in the order they are taken positionally.
    #[serde(default, rename = "input")]
    pub inputs: Vec<InputDef>,
}

/// One input as declared under `[[actions.action.input]]`.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InputDef {
    /// Placeholder name: `[a-z][a-z0-9_]{0,31}`.
    pub name: String,
    pub description: Option<String>,
    /// `"pattern"`, `"choices"`, `"path"` or `"integer"`. Inferred from
    /// `pattern` / `choices` when absent.
    pub kind: Option<String>,
    /// Regular expression the whole value must match (anchored by Claustrum).
    pub pattern: Option<String>,
    /// Allowed values.
    pub choices: Option<Vec<String>>,
    /// `path` only: refuse paths that do not exist.
    pub must_exist: Option<bool>,
    /// `integer` only: inclusive bounds.
    pub min: Option<i64>,
    pub max: Option<i64>,
    /// Makes the input optional.
    pub default: Option<String>,
    /// Maximum length in bytes (default 256).
    pub max_len: Option<usize>,
    /// Permit values starting with `-`. Off by default so that an input can
    /// never turn into an option of the program.
    #[serde(default)]
    pub allow_leading_dash: bool,
}

/// How the host process of an action is confined.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Confine {
    /// The operating system sandbox (Seatbelt on macOS; bubblewrap, Landlock
    /// and seccomp on Linux) as far as `[sandbox] confinement` enables it.
    #[default]
    #[serde(alias = "seatbelt")]
    Os,
    /// No confinement: the program runs with the user's full rights.
    None,
}

/// Sandbox-wide values a definition falls back to.
#[derive(Clone, Debug)]
pub struct CompileContext<'a> {
    /// Canonical host path of the workspace.
    pub workspace: &'a Path,
    pub default_timeout: Option<Duration>,
    pub max_output_bytes: usize,
    /// Look `command[0]` up on `PATH` and check that it exists. Off in a
    /// confined worker, which only lists the actions (the broker that runs
    /// them resolves the program itself) and may not see the host `PATH`.
    pub resolve_programs: bool,
}

/// A validated action, ready to be bound to inputs and run.
#[derive(Clone, Debug)]
pub struct ActionSpec {
    pub name: String,
    pub description: String,
    /// Resolved program path.
    pub program: PathBuf,
    /// `command[1..]` with placeholders parsed.
    pub argv: Vec<Template>,
    /// Canonical host directory inside the workspace.
    pub cwd: PathBuf,
    pub env: BTreeMap<String, Template>,
    pub env_passthrough: Vec<String>,
    pub timeout: Option<Duration>,
    pub max_output_bytes: usize,
    pub inputs: Vec<InputSpec>,
    pub confine: Confine,
    /// Resolved extra writable host directories.
    pub writable: Vec<PathBuf>,
    /// Risky but legal traits of the definition, logged at startup. See
    /// [`audit`].
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct InputSpec {
    pub name: String,
    pub description: Option<String>,
    pub kind: InputKind,
    pub default: Option<String>,
    pub max_len: usize,
    pub allow_leading_dash: bool,
}

#[derive(Clone, Debug)]
pub enum InputKind {
    Pattern(Regex),
    Choices(Vec<String>),
    Path { must_exist: bool },
    Integer { min: i64, max: i64 },
}

/// A string with `{name}` placeholders. `{{` and `}}` stand for literal
/// braces.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Template {
    segments: Vec<Segment>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Segment {
    Literal(String),
    Input(String),
}

impl Template {
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut segments = Vec::new();
        let mut literal = String::new();
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '{' if chars.peek() == Some(&'{') => {
                    chars.next();
                    literal.push('{');
                }
                '}' if chars.peek() == Some(&'}') => {
                    chars.next();
                    literal.push('}');
                }
                '{' => {
                    let mut name = String::new();
                    loop {
                        match chars.next() {
                            Some('}') => break,
                            Some(c) => name.push(c),
                            None => return Err(format!("unterminated placeholder in `{text}`")),
                        }
                    }
                    if !is_input_name(&name) {
                        return Err(format!(
                            "invalid placeholder `{{{name}}}` in `{text}` (use `{{{{` for a literal brace)"
                        ));
                    }
                    if !literal.is_empty() {
                        segments.push(Segment::Literal(std::mem::take(&mut literal)));
                    }
                    segments.push(Segment::Input(name));
                }
                '}' => {
                    return Err(format!(
                        "stray `}}` in `{text}` (use `}}}}` for a literal brace)"
                    ));
                }
                c => literal.push(c),
            }
        }
        if !literal.is_empty() || segments.is_empty() {
            segments.push(Segment::Literal(literal));
        }
        Ok(Self { segments })
    }

    /// Names of the placeholders, in order of appearance.
    pub fn inputs(&self) -> impl Iterator<Item = &str> {
        self.segments.iter().filter_map(|s| match s {
            Segment::Input(name) => Some(name.as_str()),
            Segment::Literal(_) => None,
        })
    }

    pub fn is_literal(&self) -> bool {
        self.inputs().next().is_none()
    }

    /// Substitute the placeholders. Every placeholder must have a value.
    pub fn render(&self, values: &BTreeMap<String, String>) -> String {
        let mut out = String::new();
        for s in &self.segments {
            match s {
                Segment::Literal(l) => out.push_str(l),
                Segment::Input(name) => {
                    out.push_str(values.get(name).map(String::as_str).unwrap_or_default());
                }
            }
        }
        out
    }
}

impl std::fmt::Display for Template {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for s in &self.segments {
            match s {
                Segment::Literal(l) => {
                    f.write_str(&l.replace('{', "{{").replace('}', "}}"))?;
                }
                Segment::Input(name) => write!(f, "{{{name}}}")?,
            }
        }
        Ok(())
    }
}

pub fn is_action_name(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some('a'..='z'))
        && s.len() <= 32
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

pub fn is_input_name(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some('a'..='z'))
        && s.len() <= 32
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

fn is_env_name(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some('A'..='Z' | 'a'..='z' | '_'))
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

impl ActionDef {
    /// Validate the definition and resolve everything that must not change
    /// after startup.
    pub fn compile(&self, ctx: &CompileContext<'_>) -> Result<ActionSpec, String> {
        let name = &self.name;
        let err = |m: String| format!("action `{name}`: {m}");
        if !is_action_name(name) {
            return Err(format!(
                "action name `{name}` is invalid: use [a-z][a-z0-9-]{{0,31}}"
            ));
        }

        let Some((program, rest)) = self.command.split_first() else {
            return Err(err("`command` must not be empty".into()));
        };
        let program = if ctx.resolve_programs {
            resolve_program(program).map_err(&err)?
        } else {
            PathBuf::from(program)
        };
        let argv = rest
            .iter()
            .map(|a| Template::parse(a))
            .collect::<Result<Vec<_>, _>>()
            .map_err(&err)?;
        refuse_reparsed_inputs(&program, &argv).map_err(&err)?;

        let cwd = match &self.cwd {
            None => ctx.workspace.to_path_buf(),
            Some(dir) => {
                let joined = if dir.is_absolute() {
                    dir.clone()
                } else {
                    ctx.workspace.join(dir)
                };
                let canonical = joined
                    .canonicalize()
                    .map_err(|e| err(format!("cwd `{}`: {e}", dir.display())))?;
                if !canonical.is_dir() {
                    return Err(err(format!("cwd `{}` is not a directory", dir.display())));
                }
                if !canonical.starts_with(ctx.workspace) {
                    return Err(err(format!(
                        "cwd `{}` lies outside the workspace",
                        dir.display()
                    )));
                }
                canonical
            }
        };

        let mut env = BTreeMap::new();
        for (k, v) in &self.env {
            if !is_env_name(k) {
                return Err(err(format!("invalid environment variable name `{k}`")));
            }
            env.insert(k.clone(), Template::parse(v).map_err(&err)?);
        }
        for k in self.env.keys().chain(&self.env_passthrough) {
            if super::command::PROXY_VARS
                .iter()
                .any(|p| p.eq_ignore_ascii_case(k))
            {
                return Err(err(format!(
                    "`{k}` is set by Claustrum to route the action through the network proxy"
                )));
            }
        }
        for k in &self.env_passthrough {
            if !is_env_name(k) {
                return Err(err(format!("invalid env_passthrough name `{k}`")));
            }
            if self.env.contains_key(k) {
                return Err(err(format!("`{k}` is both in env and env_passthrough")));
            }
        }

        let mut inputs = Vec::with_capacity(self.inputs.len());
        let mut seen = BTreeSet::new();
        let mut optional_seen = false;
        for def in &self.inputs {
            let input = def.compile().map_err(&err)?;
            if !seen.insert(input.name.clone()) {
                return Err(err(format!("input `{}` is declared twice", input.name)));
            }
            if input.default.is_some() {
                optional_seen = true;
            } else if optional_seen {
                return Err(err(format!(
                    "required input `{}` must come before optional inputs",
                    input.name
                )));
            }
            inputs.push(input);
        }
        let used: BTreeSet<&str> = argv
            .iter()
            .chain(env.values())
            .flat_map(Template::inputs)
            .collect();
        for u in &used {
            if !seen.contains(*u) {
                return Err(err(format!("placeholder `{{{u}}}` has no matching input")));
            }
        }
        for s in &seen {
            if !used.contains(s.as_str()) {
                return Err(err(format!("input `{s}` is never used in command or env")));
            }
        }

        let timeout = match self.timeout_secs {
            None => ctx.default_timeout,
            Some(0) => None,
            Some(secs) => Some(Duration::from_secs(secs)),
        };
        let mut spec = ActionSpec {
            name: name.clone(),
            description: self.description.clone(),
            program,
            argv,
            cwd,
            env,
            env_passthrough: self.env_passthrough.clone(),
            timeout,
            max_output_bytes: self.max_output_bytes.unwrap_or(ctx.max_output_bytes),
            inputs,
            confine: self.confine,
            writable: self
                .writable
                .iter()
                .map(|p| expand_writable(p, ctx.workspace))
                .collect::<Result<_, _>>()
                .map_err(&err)?,
            warnings: Vec::new(),
        };
        spec.warnings = audit(&spec);
        Ok(spec)
    }
}

/// Resolve one `writable` entry: `~/` is the home directory, relative paths
/// are taken against the workspace.
fn expand_writable(path: &Path, workspace: &Path) -> Result<PathBuf, String> {
    let text = path.to_string_lossy();
    let expanded = if let Some(rest) = text.strip_prefix("~/") {
        let home = std::env::var_os("HOME")
            .ok_or_else(|| format!("writable `{text}`: HOME is not set"))?;
        PathBuf::from(home).join(rest)
    } else if text.starts_with('~') {
        return Err(format!("writable `{text}`: only `~/` is expanded"));
    } else if path.is_absolute() {
        path.to_path_buf()
    } else {
        workspace.join(path)
    };
    if expanded.parent().is_none() {
        return Err(format!(
            "writable `{text}`: the root directory cannot be writable"
        ));
    }
    Ok(expanded)
}

/// Programs that re-parse an argument as code. A placeholder in the argument
/// after one of these flags would be interpreted again on the host, which
/// defeats the input validation entirely, so such definitions are refused.
const REPARSING_PROGRAMS: &[(&str, &[&str])] = &[
    ("sh", &["-c"]),
    ("bash", &["-c"]),
    ("zsh", &["-c"]),
    ("dash", &["-c"]),
    ("ksh", &["-c"]),
    ("fish", &["-c"]),
    ("python", &["-c"]),
    ("python3", &["-c"]),
    ("node", &["-e", "--eval", "-p", "--print"]),
    ("perl", &["-e", "-E"]),
    ("ruby", &["-e"]),
    ("php", &["-r"]),
    ("osascript", &["-e"]),
];

/// Programs that execute configuration or scripts found in the workspace,
/// which the guest can write. Triggering them is code execution on the host
/// even without inputs.
const WORKSPACE_CODE_PROGRAMS: &[(&str, &str)] = &[
    (
        "cargo",
        ".cargo/config.toml (rustc-wrapper, runner), build.rs and proc macros",
    ),
    ("rustc", "proc macros"),
    (
        "git",
        ".git/config (core.hooksPath, core.fsmonitor, aliases) and hooks",
    ),
    ("npm", "package.json scripts and .npmrc"),
    ("npx", "package.json scripts and .npmrc"),
    ("pnpm", "package.json scripts and .npmrc"),
    ("yarn", "package.json scripts and .yarnrc"),
    ("node", "the scripts it is given"),
    ("make", "the Makefile"),
    ("cmake", "CMakeLists.txt"),
    (
        "python",
        "the scripts it is given, sitecustomize and .pth files",
    ),
    (
        "python3",
        "the scripts it is given, sitecustomize and .pth files",
    ),
    ("pip", "setup.py of the packages it installs"),
    ("sh", "the scripts it is given"),
    ("bash", "the scripts it is given"),
    ("zsh", "the scripts it is given"),
    ("docker", "the Dockerfile and compose files"),
    ("gradle", "build.gradle"),
    ("mvn", "pom.xml plugins"),
    ("go", "go generate directives and cgo"),
];

/// Host variables that change what a process loads or runs. Forwarding them
/// hands that control to whoever set them; templating them hands it to the
/// guest.
const LOADER_ENV: &[&str] = &[
    "PATH",
    "LD_PRELOAD",
    "LD_LIBRARY_PATH",
    "DYLD_INSERT_LIBRARIES",
    "DYLD_LIBRARY_PATH",
    "DYLD_FRAMEWORK_PATH",
    "PYTHONPATH",
    "PYTHONSTARTUP",
    "NODE_OPTIONS",
    "PERL5OPT",
    "RUBYOPT",
    "RUSTFLAGS",
    "RUSTC_WRAPPER",
    "CARGO_BUILD_RUSTC_WRAPPER",
    "CARGO_HOME",
    "GIT_DIR",
    "GIT_CONFIG_GLOBAL",
    "GIT_CONFIG_SYSTEM",
    "GIT_SSH_COMMAND",
    "GIT_EXEC_PATH",
    "SSH_AUTH_SOCK",
    "BASH_ENV",
    "ENV",
    "ZDOTDIR",
    "SHELL",
    "EDITOR",
    "VISUAL",
    "PAGER",
    "GIT_PAGER",
];

fn program_basename(program: &Path) -> String {
    program
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Refuse a placeholder in an argument that the program evaluates as code.
fn refuse_reparsed_inputs(program: &Path, argv: &[Template]) -> Result<(), String> {
    let base = program_basename(program);
    let Some((_, flags)) = REPARSING_PROGRAMS.iter().find(|(p, _)| *p == base) else {
        return Ok(());
    };
    for (i, arg) in argv.iter().enumerate() {
        let text = arg.to_string();
        // `-c code` and `-ccode` / `--eval=code`.
        let flag_before = i > 0 && flags.contains(&argv[i - 1].to_string().as_str());
        let flag_inline = flags
            .iter()
            .any(|f| text.len() > f.len() && text.starts_with(f) && !arg.is_literal());
        if (flag_before && !arg.is_literal()) || flag_inline {
            return Err(format!(
                "`{base}` would re-parse the placeholder in `{text}` as code; put the code in \
                 a script and pass the input as an argument to it instead"
            ));
        }
    }
    Ok(())
}

/// Legal but risky traits of a definition, reported as warnings at startup.
pub fn audit(spec: &ActionSpec) -> Vec<String> {
    let mut warnings = Vec::new();
    let base = program_basename(&spec.program);
    if spec.confine == Confine::None
        && let Some((_, what)) = WORKSPACE_CODE_PROGRAMS.iter().find(|(p, _)| *p == base)
    {
        warnings.push(format!(
            "`{base}` executes {what} from the workspace, which the guest can write, and \
             `confine = \"none\"` lets it run unconfined: triggering this action is code \
             execution on the host"
        ));
    }
    for input in &spec.inputs {
        if let InputKind::Pattern(re) = &input.kind {
            let raw = pattern_body(re);
            if is_permissive(raw) {
                warnings.push(format!(
                    "input `{}` accepts almost anything (`{raw}`); narrow the pattern to the \
                     characters the program needs, and check how it interprets them",
                    input.name
                ));
            }
        }
    }
    for (k, t) in &spec.env {
        if !t.is_literal() {
            warnings.push(format!(
                "environment variable `{k}` contains a placeholder; environment values are \
                 validated like arguments but programs interpret them more freely"
            ));
        }
        if LOADER_ENV.contains(&k.as_str()) && !t.is_literal() {
            warnings.push(format!(
                "`{k}` controls what the program loads or runs; a guest-supplied value there \
                 is an injection path"
            ));
        }
    }
    for k in &spec.env_passthrough {
        if LOADER_ENV.contains(&k.as_str()) {
            warnings.push(format!(
                "env_passthrough forwards `{k}`, which controls what the program loads or runs"
            ));
        }
    }
    warnings
}

fn pattern_body(re: &Regex) -> &str {
    let raw = re.as_str();
    raw.strip_prefix("^(?:")
        .and_then(|r| r.strip_suffix(")$"))
        .unwrap_or(raw)
}

/// Heuristic for patterns that do not restrict the value in practice.
fn is_permissive(pattern: &str) -> bool {
    let p = pattern.trim();
    p.contains(".*")
        || p.contains(".+")
        || p.contains("[^")
        || p.contains("\\s")
        || p.contains("\\S")
        || p.contains("[[:print:]]")
        || p.contains("[[:graph:]]")
        || p.contains("\\w+") && p.len() <= 4
}

/// `command[0]`: absolute and existing, or a bare name found on `PATH`.
fn resolve_program(program: &str) -> Result<PathBuf, String> {
    if program.is_empty() {
        return Err("`command[0]` is empty".into());
    }
    if program.contains(['{', '}']) {
        return Err("`command[0]` must not contain placeholders".into());
    }
    let path = Path::new(program);
    if path.is_absolute() {
        if !path.is_file() {
            return Err(format!("program `{program}` does not exist"));
        }
        return Ok(path.to_path_buf());
    }
    if program.contains('/') {
        return Err(format!(
            "program `{program}` must be an absolute path or a bare command name"
        ));
    }
    which::which(program).map_err(|e| format!("program `{program}`: {e}"))
}

impl InputDef {
    fn compile(&self) -> Result<InputSpec, String> {
        let name = &self.name;
        if !is_input_name(name) {
            return Err(format!(
                "input name `{name}` is invalid: use [a-z][a-z0-9_]{{0,31}}"
            ));
        }
        let err = |m: String| format!("input `{name}`: {m}");
        let kind = match self.kind.as_deref() {
            Some("pattern") => "pattern",
            Some("choices") => "choices",
            Some("path") => "path",
            Some("integer") => "integer",
            Some(other) => {
                return Err(err(format!(
                    "unknown kind `{other}`; use pattern, choices, path or integer"
                )));
            }
            None if self.pattern.is_some() && self.choices.is_none() => "pattern",
            None if self.choices.is_some() && self.pattern.is_none() => "choices",
            None => return Err(err("needs `kind`, `pattern` or `choices`".into())),
        };
        let reject = |field: &str, present: bool| {
            if present {
                Err(err(format!("`{field}` does not apply to kind `{kind}`")))
            } else {
                Ok(())
            }
        };
        if kind != "pattern" {
            reject("pattern", self.pattern.is_some())?;
        }
        if kind != "choices" {
            reject("choices", self.choices.is_some())?;
        }
        if kind != "path" {
            reject("must_exist", self.must_exist.is_some())?;
        }
        if kind != "integer" {
            reject("min", self.min.is_some())?;
            reject("max", self.max.is_some())?;
        }
        let kind = match kind {
            "pattern" => {
                let pattern = self
                    .pattern
                    .as_deref()
                    .ok_or_else(|| err("kind `pattern` needs `pattern`".into()))?;
                let regex = Regex::new(&format!("^(?:{pattern})$"))
                    .map_err(|e| err(format!("invalid pattern: {e}")))?;
                InputKind::Pattern(regex)
            }
            "choices" => {
                let choices = self
                    .choices
                    .clone()
                    .ok_or_else(|| err("kind `choices` needs `choices`".into()))?;
                if choices.is_empty() {
                    return Err(err("`choices` must not be empty".into()));
                }
                InputKind::Choices(choices)
            }
            "path" => InputKind::Path {
                must_exist: self.must_exist.unwrap_or(false),
            },
            _ => {
                let min = self.min.unwrap_or(i64::MIN);
                let max = self.max.unwrap_or(i64::MAX);
                if min > max {
                    return Err(err("`min` is greater than `max`".into()));
                }
                InputKind::Integer { min, max }
            }
        };
        let max_len = self.max_len.unwrap_or(DEFAULT_MAX_LEN);
        if max_len == 0 || max_len > MAX_MAX_LEN {
            return Err(err(format!(
                "`max_len` must be between 1 and {MAX_MAX_LEN}"
            )));
        }
        Ok(InputSpec {
            name: name.clone(),
            description: self.description.clone(),
            kind,
            default: self.default.clone(),
            max_len,
            allow_leading_dash: self.allow_leading_dash,
        })
    }
}

/// Compile every definition, check that names are unique and log the
/// warnings of each one.
pub fn compile_all(
    defs: &[ActionDef],
    ctx: &CompileContext<'_>,
) -> Result<Vec<ActionSpec>, String> {
    let mut specs = Vec::with_capacity(defs.len());
    let mut names = BTreeSet::new();
    for def in defs {
        let spec = def.compile(ctx)?;
        if !names.insert(spec.name.clone()) {
            return Err(format!("action `{}` is declared twice", spec.name));
        }
        for w in &spec.warnings {
            tracing::warn!(action = spec.name, "{w}");
        }
        specs.push(spec);
    }
    Ok(specs)
}

impl InputSpec {
    /// One-line description for listings: `crate: pattern [a-z]+ (optional, default "x")`.
    pub fn describe(&self) -> String {
        let mut s = format!("{}: ", self.name);
        match &self.kind {
            InputKind::Pattern(re) => {
                let raw = re.as_str();
                let raw = raw
                    .strip_prefix("^(?:")
                    .and_then(|r| r.strip_suffix(")$"))
                    .unwrap_or(raw);
                s.push_str(&format!("matches `{raw}`"));
            }
            InputKind::Choices(c) => s.push_str(&format!("one of {}", c.join(", "))),
            InputKind::Path { must_exist } => {
                s.push_str("workspace path");
                if *must_exist {
                    s.push_str(" (must exist)");
                }
            }
            InputKind::Integer { min, max } => {
                s.push_str("integer");
                if *min != i64::MIN || *max != i64::MAX {
                    s.push_str(&format!(" {min}..={max}"));
                }
            }
        }
        if let Some(d) = &self.default {
            s.push_str(&format!(" (optional, default \"{d}\")"));
        }
        if let Some(d) = &self.description {
            s.push_str(&format!(" — {d}"));
        }
        s
    }
}

impl ActionSpec {
    /// The argv as configured, with placeholders shown.
    pub fn command_line(&self) -> String {
        std::iter::once(self.program.display().to_string())
            .chain(self.argv.iter().map(ToString::to_string))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(ws: &Path) -> CompileContext<'_> {
        CompileContext {
            workspace: ws,
            default_timeout: Some(Duration::from_secs(5)),
            max_output_bytes: 1024,
            resolve_programs: true,
        }
    }

    fn def(name: &str, command: &[&str]) -> ActionDef {
        ActionDef {
            name: name.into(),
            command: command.iter().map(|s| (*s).to_owned()).collect(),
            ..Default::default()
        }
    }

    fn input(name: &str, pattern: &str) -> InputDef {
        InputDef {
            name: name.into(),
            pattern: Some(pattern.into()),
            ..Default::default()
        }
    }

    #[test]
    fn templates_parse_and_render() {
        let t = Template::parse("--pkg={crate} {{literal}} x").unwrap();
        assert_eq!(t.inputs().collect::<Vec<_>>(), ["crate"]);
        let mut v = BTreeMap::new();
        v.insert("crate".to_owned(), "foo".to_owned());
        assert_eq!(t.render(&v), "--pkg=foo {literal} x");
        assert_eq!(t.to_string(), "--pkg={crate} {{literal}} x");
        assert!(Template::parse("{unterminated").is_err());
        assert!(Template::parse("stray }").is_err());
        assert!(Template::parse("{Bad-Name}").is_err());
        assert!(Template::parse("").unwrap().is_literal());
    }

    #[test]
    fn compiles_a_plain_action() {
        let ws = tempfile::tempdir().unwrap();
        let ws = ws.path().canonicalize().unwrap();
        let spec = def("echo", &["/bin/echo", "hi"])
            .compile(&ctx(&ws))
            .unwrap();
        assert_eq!(spec.program, PathBuf::from("/bin/echo"));
        assert_eq!(spec.cwd, ws);
        assert_eq!(spec.timeout, Some(Duration::from_secs(5)));
        assert_eq!(spec.max_output_bytes, 1024);
        assert!(spec.inputs.is_empty());

        let spec = def("bare", &["echo"]).compile(&ctx(&ws)).unwrap();
        assert!(spec.program.is_absolute());
    }

    #[test]
    fn refuses_placeholders_that_a_shell_would_reparse() {
        let ws = tempfile::tempdir().unwrap();
        let ws = ws.path().canonicalize().unwrap();
        let c = ctx(&ws);
        let with_input = |command: &[&str]| ActionDef {
            inputs: vec![input("x", "[a-z]+")],
            ..def("t", command)
        };
        for command in [
            &["/bin/sh", "-c", "echo {x}"][..],
            &["/bin/bash", "-c", "{x}"][..],
            &["/bin/sh", "-c{x}"][..],
            &["/bin/zsh", "-c", "{x}"][..],
        ] {
            let e = with_input(command).compile(&c).unwrap_err();
            assert!(e.contains("re-parse"), "{command:?}: {e}");
        }
        // Literal code with the input as a separate argument is fine.
        let spec = with_input(&["/bin/sh", "-c", "echo \"$1\"", "sh", "{x}"])
            .compile(&c)
            .unwrap();
        assert!(spec.warnings.is_empty(), "{:?}", spec.warnings);
        let spec = ActionDef {
            confine: Confine::None,
            ..with_input(&["/bin/sh", "-c", "echo \"$1\"", "sh", "{x}"])
        }
        .compile(&c)
        .unwrap();
        assert!(spec.warnings.iter().any(|w| w.contains("executes")));
        // A literal `-c` script without placeholders is fine too.
        with_input(&["/bin/sh", "-c", "echo hi", "{x}"])
            .compile(&c)
            .unwrap();
    }

    #[test]
    fn audit_flags_risky_definitions() {
        let ws = tempfile::tempdir().unwrap();
        let ws = ws.path().canonicalize().unwrap();
        let c = ctx(&ws);
        let spec = ActionDef {
            inputs: vec![input("a", ".*"), input("b", "[a-z]+")],
            env: [
                ("RUSTFLAGS".to_owned(), "{a}".to_owned()),
                ("PLAIN".to_owned(), "x".to_owned()),
            ]
            .into(),
            env_passthrough: vec!["DYLD_INSERT_LIBRARIES".into(), "CARGO_TERM_COLOR".into()],
            ..def("t", &["/bin/echo", "{b}"])
        }
        .compile(&c)
        .unwrap();
        let joined = spec.warnings.join("\n");
        assert!(
            joined.contains("input `a` accepts almost anything"),
            "{joined}"
        );
        assert!(!joined.contains("input `b`"), "{joined}");
        assert!(
            joined.contains("`RUSTFLAGS` contains a placeholder"),
            "{joined}"
        );
        assert!(joined.contains("`RUSTFLAGS` controls"), "{joined}");
        assert!(!joined.contains("PLAIN"), "{joined}");
        assert!(
            joined.contains("forwards `DYLD_INSERT_LIBRARIES`"),
            "{joined}"
        );
        assert!(!joined.contains("CARGO_TERM_COLOR"), "{joined}");

        let quiet = def("t", &["/bin/echo", "hi"]).compile(&c).unwrap();
        assert!(quiet.warnings.is_empty(), "{:?}", quiet.warnings);
    }

    #[test]
    fn rejects_bad_definitions() {
        let ws = tempfile::tempdir().unwrap();
        let ws = ws.path().canonicalize().unwrap();
        std::fs::create_dir(ws.join("sub")).unwrap();
        let c = ctx(&ws);
        let cases: Vec<(ActionDef, &str)> = vec![
            (def("Bad", &["/bin/echo"]), "invalid"),
            (def("x", &[]), "must not be empty"),
            (def("x", &["/no/such/program"]), "does not exist"),
            (def("x", &["./relative"]), "absolute path or a bare"),
            (def("x", &["definitely-not-a-command-xyz"]), "program"),
            (def("x", &["/bin/{echo}"]), "placeholders"),
            (def("x", &["/bin/echo", "{unknown}"]), "no matching input"),
            (
                ActionDef {
                    cwd: Some("..".into()),
                    ..def("x", &["/bin/echo"])
                },
                "outside the workspace",
            ),
            (
                ActionDef {
                    cwd: Some("missing".into()),
                    ..def("x", &["/bin/echo"])
                },
                "cwd",
            ),
            (
                ActionDef {
                    writable: vec!["~nobody/x".into()],
                    ..def("x", &["/bin/echo"])
                },
                "writable",
            ),
            (
                ActionDef {
                    inputs: vec![input("a", "x")],
                    ..def("x", &["/bin/echo"])
                },
                "never used",
            ),
            (
                ActionDef {
                    inputs: vec![input("a", "x"), input("a", "y")],
                    ..def("x", &["/bin/echo", "{a}"])
                },
                "declared twice",
            ),
            (
                ActionDef {
                    inputs: vec![
                        InputDef {
                            default: Some("d".into()),
                            ..input("a", "x")
                        },
                        input("b", "y"),
                    ],
                    ..def("x", &["/bin/echo", "{a}", "{b}"])
                },
                "before optional",
            ),
            (
                ActionDef {
                    inputs: vec![input("a", "(unclosed")],
                    ..def("x", &["/bin/echo", "{a}"])
                },
                "invalid pattern",
            ),
            (
                ActionDef {
                    inputs: vec![InputDef {
                        name: "a".into(),
                        ..Default::default()
                    }],
                    ..def("x", &["/bin/echo", "{a}"])
                },
                "needs `kind`",
            ),
            (
                ActionDef {
                    inputs: vec![InputDef {
                        kind: Some("integer".into()),
                        ..input("a", "x")
                    }],
                    ..def("x", &["/bin/echo", "{a}"])
                },
                "does not apply",
            ),
            (
                ActionDef {
                    env: [("https_proxy".to_owned(), "x".to_owned())].into(),
                    ..def("x", &["/bin/echo"])
                },
                "network proxy",
            ),
            (
                ActionDef {
                    env_passthrough: vec!["HTTP_PROXY".into()],
                    ..def("x", &["/bin/echo"])
                },
                "network proxy",
            ),
            (
                ActionDef {
                    env: [("1BAD".to_owned(), "x".to_owned())].into(),
                    ..def("x", &["/bin/echo"])
                },
                "environment variable name",
            ),
        ];
        for (d, expected) in cases {
            let e = d.compile(&c).expect_err(&format!("{d:?} should fail"));
            assert!(
                e.contains(expected),
                "{d:?}: got `{e}`, expected `{expected}`"
            );
        }
        let spec = ActionDef {
            cwd: Some("sub".into()),
            ..def("x", &["/bin/echo"])
        }
        .compile(&c)
        .unwrap();
        assert_eq!(spec.cwd, ws.join("sub"));

        let e = compile_all(&[def("x", &["/bin/echo"]), def("x", &["/bin/echo"])], &c).unwrap_err();
        assert!(e.contains("declared twice"));
    }
}
