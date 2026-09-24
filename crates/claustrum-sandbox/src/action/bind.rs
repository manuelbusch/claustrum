//! Binding guest-supplied inputs to an action: the only place where data from
//! the sandbox is turned into host argv.
//!
//! Every value passes the global checks (length, control characters, leading
//! `-`) and then its declared kind. Placeholders are substituted into single
//! argv elements which are handed to the program as they are, so the program
//! never sees more or fewer arguments than the definition names, and no shell
//! ever interprets them.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use super::spec::{ActionSpec, InputKind, InputSpec};
use crate::{WORKSPACE, fs::normalize_guest_path};

/// Why an invocation was not run. The message is meant for the model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal(pub String);

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Refusal {}

/// An action with all inputs validated and substituted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bound {
    pub argv: Vec<String>,
    pub env: BTreeMap<String, String>,
    /// The accepted input values, for logging.
    pub values: BTreeMap<String, String>,
}

/// Split command-line style arguments into positional values and
/// `name=value` pairs. An argument counts as named only when the part before
/// `=` is one of the action's inputs, so values may contain `=`.
pub fn parse_args(spec: &ActionSpec, args: &[String]) -> (Vec<String>, BTreeMap<String, String>) {
    let mut positional = Vec::new();
    let mut named = BTreeMap::new();
    for arg in args {
        match arg.split_once('=') {
            Some((name, value)) if spec.inputs.iter().any(|i| i.name == name) => {
                named.insert(name.to_owned(), value.to_owned());
            }
            _ => positional.push(arg.clone()),
        }
    }
    (positional, named)
}

/// Validate and substitute. `guest_cwd` resolves relative `path` inputs;
/// `workspace` is the canonical host directory behind `/workspace`.
pub fn bind(
    spec: &ActionSpec,
    positional: &[String],
    named: &BTreeMap<String, String>,
    guest_cwd: &str,
    workspace: &Path,
) -> Result<Bound, Refusal> {
    let refuse = |m: String| Refusal(format!("action `{}`: {m}", spec.name));
    if spec.inputs.is_empty() && (!positional.is_empty() || !named.is_empty()) {
        return Err(refuse("takes no inputs".into()));
    }
    if positional.len() > spec.inputs.len() {
        return Err(refuse(format!(
            "too many inputs: expected at most {}, got {}",
            spec.inputs.len(),
            positional.len()
        )));
    }
    for name in named.keys() {
        if !spec.inputs.iter().any(|i| &i.name == name) {
            return Err(refuse(format!("unknown input `{name}`")));
        }
    }

    let mut values = BTreeMap::new();
    for (index, input) in spec.inputs.iter().enumerate() {
        let from_position = positional.get(index);
        let from_name = named.get(&input.name);
        let raw = match (from_position, from_name) {
            (Some(_), Some(_)) => {
                return Err(refuse(format!(
                    "input `{}` given both positionally and by name",
                    input.name
                )));
            }
            (Some(v), None) | (None, Some(v)) => v.clone(),
            (None, None) => match &input.default {
                Some(d) => d.clone(),
                None => {
                    return Err(refuse(format!("missing required input `{}`", input.name)));
                }
            },
        };
        let value = check(input, &raw, guest_cwd, workspace, &spec.cwd)
            .map_err(|m| refuse(format!("input `{}` {m}", input.name)))?;
        values.insert(input.name.clone(), value);
    }

    let argv = spec.argv.iter().map(|t| t.render(&values)).collect();
    let env = spec
        .env
        .iter()
        .map(|(k, t)| (k.clone(), t.render(&values)))
        .collect();
    Ok(Bound { argv, env, values })
}

/// Validate one value against its input and return what is substituted.
fn check(
    input: &InputSpec,
    raw: &str,
    guest_cwd: &str,
    workspace: &Path,
    action_cwd: &Path,
) -> Result<String, String> {
    if raw.len() > input.max_len {
        return Err(format!(
            "is too long ({} bytes, limit {})",
            raw.len(),
            input.max_len
        ));
    }
    if raw.chars().any(char::is_control) {
        return Err("contains control characters".into());
    }
    if raw.starts_with('-') && !input.allow_leading_dash {
        return Err("must not start with `-`".into());
    }
    match &input.kind {
        InputKind::Pattern(re) => {
            if re.is_match(raw) {
                Ok(raw.to_owned())
            } else {
                Err(format!("does not match the pattern `{}`", pattern_source(re)))
            }
        }
        InputKind::Choices(choices) => {
            if choices.iter().any(|c| c == raw) {
                Ok(raw.to_owned())
            } else {
                Err(format!("must be one of {}", choices.join(", ")))
            }
        }
        InputKind::Integer { min, max } => match raw.parse::<i64>() {
            Ok(n) if n >= *min && n <= *max => Ok(n.to_string()),
            Ok(_) => Err(format!("must be between {min} and {max}")),
            Err(_) => Err("is not an integer".into()),
        },
        InputKind::Path { must_exist } => {
            let host = workspace_path(raw, guest_cwd, workspace)?;
            if *must_exist && std::fs::symlink_metadata(&host).is_err() {
                return Err("does not exist".into());
            }
            let shown = match host.strip_prefix(action_cwd) {
                Ok(rel) if rel.as_os_str().is_empty() => PathBuf::from("."),
                Ok(rel) => rel.to_path_buf(),
                Err(_) => host,
            };
            let mut s = shown.to_string_lossy().into_owned();
            if s.starts_with('-') {
                s.insert_str(0, "./");
            }
            Ok(s)
        }
    }
}

fn pattern_source(re: &regex::Regex) -> &str {
    let raw = re.as_str();
    raw.strip_prefix("^(?:")
        .and_then(|r| r.strip_suffix(")$"))
        .unwrap_or(raw)
}

/// Map a guest path to the host file it denotes, refusing anything that
/// leaves the workspace lexically or through symlinks.
fn workspace_path(raw: &str, guest_cwd: &str, workspace: &Path) -> Result<PathBuf, String> {
    let guest = normalize_guest_path(raw, guest_cwd).map_err(|e| e.to_string())?;
    let rel = if guest == WORKSPACE {
        ""
    } else {
        guest
            .strip_prefix(&format!("{WORKSPACE}/"))
            .ok_or_else(|| format!("must lie inside {WORKSPACE}"))?
    };
    let host = workspace.join(rel);
    let resolved = crate::protect::resolve_host_path(&host).map_err(|e| e.to_string())?;
    if !resolved.starts_with(workspace) {
        return Err("leaves the workspace through a symlink".into());
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::action::spec::{ActionDef, CompileContext, InputDef};

    fn spec(ws: &Path, command: &[&str], inputs: Vec<InputDef>) -> ActionSpec {
        ActionDef {
            name: "t".into(),
            command: command.iter().map(|s| (*s).to_owned()).collect(),
            inputs,
            ..Default::default()
        }
        .compile(&CompileContext {
            workspace: ws,
            default_timeout: Some(Duration::from_secs(1)),
            max_output_bytes: 1024,
        })
        .unwrap()
    }

    fn pattern(name: &str, pattern: &str) -> InputDef {
        InputDef {
            name: name.into(),
            pattern: Some(pattern.into()),
            ..Default::default()
        }
    }

    fn run(spec: &ActionSpec, args: &[&str], ws: &Path) -> Result<Vec<String>, String> {
        let args: Vec<String> = args.iter().map(|s| (*s).to_owned()).collect();
        let (pos, named) = parse_args(spec, &args);
        bind(spec, &pos, &named, WORKSPACE, ws)
            .map(|b| b.argv)
            .map_err(|r| r.0)
    }

    #[test]
    fn trigger_only_actions_take_no_arguments() {
        let ws = tempfile::tempdir().unwrap();
        let ws = ws.path().canonicalize().unwrap();
        let s = spec(&ws, &["/bin/echo", "fixed"], vec![]);
        assert_eq!(run(&s, &[], &ws).unwrap(), ["fixed"]);
        assert!(run(&s, &["x"], &ws).unwrap_err().contains("takes no inputs"));
    }

    #[test]
    fn positional_and_named_inputs_agree() {
        let ws = tempfile::tempdir().unwrap();
        let ws = ws.path().canonicalize().unwrap();
        let s = spec(
            &ws,
            &["/bin/echo", "-p", "{crate}", "--", "{filter}"],
            vec![
                pattern("crate", "[a-z][a-z0-9-]{0,40}"),
                InputDef {
                    default: Some("".into()),
                    ..pattern("filter", "[A-Za-z0-9_:=]{0,80}")
                },
            ],
        );
        let expect = ["-p", "claustrum-cli", "--", ""];
        assert_eq!(run(&s, &["claustrum-cli"], &ws).unwrap(), expect);
        assert_eq!(run(&s, &["crate=claustrum-cli"], &ws).unwrap(), expect);
        assert_eq!(
            run(&s, &["claustrum-cli", "filter=a=b"], &ws).unwrap(),
            ["-p", "claustrum-cli", "--", "a=b"]
        );
        assert_eq!(
            run(&s, &["claustrum-cli", "norm"], &ws).unwrap(),
            ["-p", "claustrum-cli", "--", "norm"]
        );
        // `name=value` with an unknown name is an ordinary positional value.
        assert_eq!(
            run(&s, &["claustrum-cli", "nope=1"], &ws).unwrap(),
            ["-p", "claustrum-cli", "--", "nope=1"]
        );
        let named: BTreeMap<String, String> = [("nope".to_owned(), "1".to_owned())].into();
        let e = bind(&s, &["a".to_owned()], &named, WORKSPACE, &ws).unwrap_err();
        assert!(e.0.contains("unknown input"), "{e}");
        for (args, msg) in [
            (&["a", "b", "c"][..], "too many"),
            (&[][..], "missing required"),
            (&["a", "crate=a"][..], "both"),
        ] {
            let e = run(&s, args, &ws).unwrap_err();
            assert!(e.contains(msg), "{args:?}: {e}");
        }
    }

    #[test]
    fn rejects_injection_attempts() {
        let ws = tempfile::tempdir().unwrap();
        let ws = ws.path().canonicalize().unwrap();
        let s = spec(
            &ws,
            &["/bin/echo", "{v}"],
            vec![pattern("v", "[A-Za-z0-9_./-]+")],
        );
        for bad in [
            "; rm -rf /",
            "$(id)",
            "`id`",
            "a\nb",
            "a\0b",
            "-rf",
            "--help",
            "a b",
            "",
            &"x".repeat(257),
        ] {
            assert!(run(&s, &[bad], &ws).is_err(), "{bad:?} was accepted");
        }
        // The pattern is the whole-value check: no substring matches.
        assert_eq!(run(&s, &["ok.txt"], &ws).unwrap(), ["ok.txt"]);

        let dash = spec(
            &ws,
            &["/bin/echo", "{v}"],
            vec![InputDef {
                allow_leading_dash: true,
                ..pattern("v", "-?[a-z]+")
            }],
        );
        assert_eq!(run(&dash, &["-v"], &ws).unwrap(), ["-v"]);
    }

    #[test]
    fn choices_and_integers() {
        let ws = tempfile::tempdir().unwrap();
        let ws = ws.path().canonicalize().unwrap();
        let s = spec(
            &ws,
            &["/bin/echo", "{mode}", "{n}"],
            vec![
                InputDef {
                    name: "mode".into(),
                    choices: Some(vec!["debug".into(), "release".into()]),
                    ..Default::default()
                },
                InputDef {
                    name: "n".into(),
                    kind: Some("integer".into()),
                    min: Some(1),
                    max: Some(8),
                    ..Default::default()
                },
            ],
        );
        assert_eq!(run(&s, &["release", "4"], &ws).unwrap(), ["release", "4"]);
        assert_eq!(run(&s, &["debug", "+4"], &ws).unwrap(), ["debug", "4"]);
        assert!(run(&s, &["Release", "4"], &ws).is_err());
        assert!(run(&s, &["debug", "9"], &ws).is_err());
        assert!(run(&s, &["debug", "x"], &ws).is_err());
    }

    #[test]
    fn paths_stay_inside_the_workspace() {
        let ws = tempfile::tempdir().unwrap();
        let ws = ws.path().canonicalize().unwrap();
        std::fs::create_dir_all(ws.join("src/dir")).unwrap();
        std::fs::write(ws.join("src/a.rs"), "").unwrap();
        std::fs::write(ws.join("-dash.rs"), "").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/etc/hosts", ws.join("escape")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/etc", ws.join("src/dir/etcdir")).unwrap();
        let s = spec(
            &ws,
            &["/bin/echo", "{file}"],
            vec![InputDef {
                name: "file".into(),
                kind: Some("path".into()),
                must_exist: Some(true),
                allow_leading_dash: true,
                ..Default::default()
            }],
        );
        let bind_in = |args: &[&str], cwd: &str| {
            let args: Vec<String> = args.iter().map(|s| (*s).to_owned()).collect();
            let (pos, named) = parse_args(&s, &args);
            bind(&s, &pos, &named, cwd, &ws).map(|b| b.argv).map_err(|r| r.0)
        };
        assert_eq!(bind_in(&["src/a.rs"], WORKSPACE).unwrap(), ["src/a.rs"]);
        assert_eq!(bind_in(&["a.rs"], "/workspace/src").unwrap(), ["src/a.rs"]);
        assert_eq!(
            bind_in(&["/workspace/src/../src/a.rs"], WORKSPACE).unwrap(),
            ["src/a.rs"]
        );
        assert_eq!(bind_in(&["."], WORKSPACE).unwrap(), ["."]);
        assert_eq!(bind_in(&["-dash.rs"], WORKSPACE).unwrap(), ["./-dash.rs"]);
        for bad in [
            "../../etc/passwd",
            "/etc/passwd",
            "/tmp/x",
            "src/missing.rs",
            "escape",
            "src/dir/etcdir/hosts",
        ] {
            let e = bind_in(&[bad], WORKSPACE).unwrap_err();
            assert!(!e.is_empty(), "{bad} was accepted");
        }
        let e = bind_in(&["../../x"], WORKSPACE).unwrap_err();
        assert!(e.contains("escapes") || e.contains("inside"), "{e}");
    }
}
