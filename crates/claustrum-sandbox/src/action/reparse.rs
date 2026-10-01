//! Refusing placeholders that the host would interpret as code.
//!
//! Claustrum validates an input once; after that it must reach the program as
//! plain data. Some arguments never stay data:
//!
//! - Shells and interpreters re-parse some of their arguments as code
//!   (`sh -c`, `python -c`, `perl -e`, `awk 'program'`).
//! - Wrappers such as `env`, `timeout` or `xargs` run the program named in
//!   their arguments, which may itself be a shell.
//! - A few programs take settings that run commands (`git -c`,
//!   `cargo --config`, `make VAR=$(shell ...)`).
//!
//! A placeholder in any of those positions hands the guest code execution on
//! the host, so such a definition is refused.
//!
//! The check follows each program's option syntax only as far as it needs to
//! tell code from data, and it fails closed. In a command with placeholders it
//! also refuses:
//!
//! - options it does not know,
//! - placeholders inside option tokens,
//! - placeholders in the argument of an interpreter option.

use std::path::Path;

use super::spec::{LOADER_ENV, Template};

/// Refuse a definition whose placeholders a program would interpret as
/// code, or that lets the guest choose the program to run.
pub(super) fn refuse_reparsed_inputs(program: &Path, argv: &[Template]) -> Result<(), String> {
    if argv.iter().all(Template::is_literal) {
        return Ok(());
    }
    check_command(&program_name(program), argv)
}

/// The name a program is known by: lowercase basename without `.exe` and
/// version suffix (`/usr/bin/python3.12` → `python`).
fn program_name(program: &Path) -> String {
    let base = program
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let base = base.strip_suffix(".exe").unwrap_or(&base);
    let stem = base.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.');
    let stem = if stem.is_empty() { base } else { stem };
    match stem {
        "nodejs" => "node",
        "pypy" => "python",
        other => other,
    }
    .to_owned()
}

fn check_command(name: &str, args: &[Template]) -> Result<(), String> {
    if let Some(w) = WRAPPERS.iter().find(|w| w.names.contains(&name)) {
        return check_wrapper(name, w, args);
    }
    if let Some((_, reason)) = REFUSE_ALL.iter().find(|(n, _)| *n == name) {
        return match args.iter().find(|a| !a.is_literal()) {
            Some(a) => Err(format!(
                "`{name}` {reason}, so the placeholder in `{a}` is refused; put the command in \
                 a script and pass the input to it as an argument instead"
            )),
            None => Ok(()),
        };
    }
    match name {
        "find" => check_find(args),
        "git" => check_git(args),
        "cargo" => check_cargo(args),
        _ => match GRAMMARS.iter().find(|g| g.names.contains(&name)) {
            Some(g) => check_interpreter(name, g, args),
            None => Ok(()),
        },
    }
}

fn code_error(name: &str, arg: &Template) -> String {
    format!(
        "`{name}` would re-parse the placeholder in `{arg}` as code; put the code in a script \
         and pass the input as an argument to it instead"
    )
}

fn program_error(name: &str, arg: &Template) -> String {
    format!(
        "`{name}` would run the program or script named by the placeholder in `{arg}`; name it \
         in the definition instead"
    )
}

fn option_error(name: &str, arg: &Template) -> String {
    format!(
        "`{name}` would read the placeholder in `{arg}` as an option or an option's argument; \
         placeholders may only stand for plain arguments here"
    )
}

fn unknown_error(name: &str, option: &str) -> String {
    format!(
        "`{name}` option `{option}` is unknown to Claustrum, so it cannot tell code from data in \
         this command and refuses its placeholders; put the command in a script and pass the \
         input to it as an argument instead"
    )
}

/// The argument at `i`, which an option consumes, must not be a placeholder.
/// (Whether it exists is for the program to report.)
fn literal_arg(name: &str, args: &[Template], i: usize) -> Result<(), String> {
    match args.get(i) {
        Some(a) if !a.is_literal() => Err(option_error(name, a)),
        _ => Ok(()),
    }
}

/// Whether `arg` is an option token: starts with `-` (or `+` where that
/// introduces options too) followed by something.
fn is_option(arg: &Template, plus: bool) -> bool {
    let p = arg.literal_prefix();
    let starts = p.starts_with('-') || (plus && p.starts_with('+'));
    starts && (p.len() > 1 || !arg.is_literal())
}

/// Programs that run the program named by one of their arguments.
struct Wrapper {
    names: &'static [&'static str],
    /// Short options without an argument.
    flags: &'static str,
    /// Short options with an argument, attached or following.
    with_arg: &'static str,
    /// Short options that take the rest of their token as an optional
    /// argument, never the next one.
    attached: &'static str,
    long_flags: &'static [&'static str],
    long_with_arg: &'static [&'static str],
    /// Operands before the program (`timeout DURATION`, `chrt PRIORITY`).
    /// These may hold placeholders.
    operands: usize,
    /// `NAME=value` operands before the program (`env`).
    assignments: bool,
}

const WRAPPERS: &[Wrapper] = &[
    Wrapper {
        names: &["env"],
        flags: "i0v",
        with_arg: "uC",
        attached: "",
        long_flags: &["ignore-environment", "null", "debug"],
        long_with_arg: &["unset", "chdir"],
        operands: 0,
        assignments: true,
    },
    Wrapper {
        names: &["nice"],
        flags: "0123456789",
        with_arg: "n",
        attached: "",
        long_flags: &[],
        long_with_arg: &["adjustment"],
        operands: 0,
        assignments: false,
    },
    Wrapper {
        names: &["timeout", "gtimeout"],
        flags: "v",
        with_arg: "sk",
        attached: "",
        long_flags: &["preserve-status", "foreground", "verbose"],
        long_with_arg: &["signal", "kill-after"],
        operands: 1,
        assignments: false,
    },
    Wrapper {
        names: &["stdbuf", "gstdbuf"],
        flags: "",
        with_arg: "ioe",
        attached: "",
        long_flags: &[],
        long_with_arg: &["input", "output", "error"],
        operands: 0,
        assignments: false,
    },
    Wrapper {
        names: &["nohup"],
        flags: "",
        with_arg: "",
        attached: "",
        long_flags: &[],
        long_with_arg: &[],
        operands: 0,
        assignments: false,
    },
    Wrapper {
        names: &["setsid"],
        flags: "cfw",
        with_arg: "",
        attached: "",
        long_flags: &["ctty", "fork", "wait"],
        long_with_arg: &[],
        operands: 0,
        assignments: false,
    },
    Wrapper {
        names: &["command", "builtin", "exec"],
        flags: "p",
        with_arg: "",
        attached: "",
        long_flags: &[],
        long_with_arg: &[],
        operands: 0,
        assignments: false,
    },
    Wrapper {
        names: &["time", "gtime"],
        flags: "pvaq",
        with_arg: "fo",
        attached: "",
        long_flags: &["portability", "verbose", "append", "quiet"],
        long_with_arg: &["format", "output"],
        operands: 0,
        assignments: false,
    },
    Wrapper {
        names: &["chrt"],
        flags: "abdfiormRv",
        with_arg: "TPD",
        attached: "",
        long_flags: &[
            "all-tasks",
            "batch",
            "deadline",
            "fifo",
            "idle",
            "other",
            "rr",
            "reset-on-fork",
            "verbose",
        ],
        long_with_arg: &["sched-runtime", "sched-period", "sched-deadline"],
        operands: 1,
        assignments: false,
    },
    Wrapper {
        names: &["taskset"],
        flags: "ac",
        with_arg: "",
        attached: "",
        long_flags: &["all-tasks", "cpu-list"],
        long_with_arg: &[],
        operands: 1,
        assignments: false,
    },
    Wrapper {
        names: &["ionice"],
        flags: "t",
        with_arg: "cn",
        attached: "",
        long_flags: &["ignore"],
        long_with_arg: &["class", "classdata"],
        operands: 0,
        assignments: false,
    },
    Wrapper {
        names: &["caffeinate"],
        flags: "dimsu",
        with_arg: "tw",
        attached: "",
        long_flags: &[],
        long_with_arg: &[],
        operands: 0,
        assignments: false,
    },
    Wrapper {
        names: &["xargs", "gxargs"],
        flags: "0rtpxo",
        with_arg: "InLPsdEa",
        attached: "ile",
        long_flags: &[
            "null",
            "no-run-if-empty",
            "verbose",
            "interactive",
            "exit",
            "open-tty",
            "replace",
            "eof",
            "max-lines",
        ],
        long_with_arg: &[
            "max-args",
            "max-procs",
            "max-chars",
            "delimiter",
            "arg-file",
            "process-slot-var",
        ],
        operands: 0,
        assignments: false,
    },
    Wrapper {
        names: &["busybox", "toybox"],
        flags: "",
        with_arg: "",
        attached: "",
        long_flags: &[],
        long_with_arg: &[],
        operands: 0,
        assignments: false,
    },
];

/// Programs that hand (parts of) their arguments to a shell or otherwise
/// evaluate them, in ways too varied to follow. Any placeholder is refused.
const REFUSE_ALL: &[(&str, &str)] = &[
    ("sudo", "runs commands and has shell modes"),
    ("doas", "runs commands and has shell modes"),
    ("su", "runs its command through a shell"),
    ("runuser", "runs its command through a shell"),
    ("pkexec", "runs the program named in its arguments"),
    ("sg", "runs its command through a shell"),
    ("ssh", "passes its command to the remote shell"),
    ("watch", "runs its command through a shell"),
    ("script", "runs its command through a shell"),
    ("flock", "runs its command through a shell"),
    ("nsenter", "runs the program named in its arguments"),
    ("unshare", "runs the program named in its arguments"),
    ("chroot", "runs the program named in its arguments"),
    ("systemd-run", "runs the program named in its arguments"),
    ("sandbox-exec", "runs the program named in its arguments"),
    ("arch", "runs the program named in its arguments"),
    (
        "open",
        "opens files and URLs with whatever application handles them",
    ),
    (
        "xdg-open",
        "opens files and URLs with whatever application handles them",
    ),
    ("pwsh", "evaluates its arguments as PowerShell"),
    ("powershell", "evaluates its arguments as PowerShell"),
    (
        "make",
        "expands variables and targets given on its command line (`$(shell ...)`)",
    ),
    (
        "gmake",
        "expands variables and targets given on its command line (`$(shell ...)`)",
    ),
];

fn check_wrapper(name: &str, w: &Wrapper, args: &[Template]) -> Result<(), String> {
    let mut i = 0;
    while let Some(arg) = args.get(i) {
        // `env -` is `env -i`.
        if w.assignments && arg.as_literal() == Some("-") {
            i += 1;
            continue;
        }
        if !is_option(arg, false) {
            break;
        }
        let Some(s) = arg.as_literal() else {
            return Err(option_error(name, arg));
        };
        if s == "--" {
            i += 1;
            break;
        }
        if let Some(long) = s.strip_prefix("--") {
            let (opt, inline) = match long.split_once('=') {
                Some((o, _)) => (o, true),
                None => (long, false),
            };
            if w.long_with_arg.contains(&opt) {
                if !inline {
                    i += 1;
                    literal_arg(name, args, i)?;
                }
            } else if !w.long_flags.contains(&opt) {
                return Err(unknown_error(name, s));
            }
            i += 1;
            continue;
        }
        let body = &s[1..];
        for (k, c) in body.char_indices() {
            if w.with_arg.contains(c) {
                if k + c.len_utf8() == body.len() {
                    i += 1;
                    literal_arg(name, args, i)?;
                }
                break;
            }
            if w.attached.contains(c) {
                break;
            }
            if !w.flags.contains(c) {
                return Err(unknown_error(name, s));
            }
        }
        i += 1;
    }
    if w.assignments {
        while let Some(arg) = args.get(i) {
            // `NAME=value`, with the name written out in the definition.
            let Some((var, _)) = arg.literal_prefix().split_once('=') else {
                break;
            };
            if !arg.is_literal() && LOADER_ENV.contains(&var) {
                return Err(format!(
                    "`{name}` would set `{var}` from the placeholder in `{arg}`; it controls what \
                     the program loads or runs"
                ));
            }
            i += 1;
        }
    }
    i += w.operands;
    let Some(program) = args.get(i) else {
        return Ok(());
    };
    let Some(literal) = program.as_literal() else {
        return Err(program_error(name, program));
    };
    check_command(&program_name(Path::new(literal)), &args[i + 1..])
}

/// Option syntax of a shell or interpreter. Every one of them treats its
/// first operand as code or as the script to run, unless an option already
/// gave the code (`-c`, `-e`, `-f`); the operands after that are data.
struct Grammar {
    names: &'static [&'static str],
    /// Short options without an argument; `*` for any letter or digit.
    flags: &'static str,
    /// Short options with an argument that is not code.
    with_arg: &'static str,
    /// Short options whose argument is code, a file of code or a module.
    code: &'static str,
    /// Short options that take the rest of their token as an optional
    /// argument, never the next one.
    attached: &'static str,
    /// Short options followed by optional octal digits in the same token,
    /// after which the token goes on (`perl -l0e`).
    attached_digits: &'static str,
    /// Of `with_arg`, those whose argument may be a placeholder (`awk -v`).
    data_arg: &'static str,
    long_flags: &'static [&'static str],
    long_with_arg: &'static [&'static str],
    long_code: &'static [&'static str],
    /// Short flag after which the first operand is code (`sh -c`).
    command_mode: Option<char>,
    /// Short flags after which operands are opened in a way that runs
    /// commands (`perl -n` opens `cmd|`).
    reparsed_operands: &'static str,
    /// Whether an option's argument may follow in the same token (`-cCODE`).
    attached_values: bool,
    /// Whether `+x` introduces options too (shells).
    plus: bool,
    /// Whether options may follow operands (GNU argument permutation).
    permute: bool,
    /// Whether an option giving the code ends the options (`python -c`).
    code_ends_options: bool,
}

const SHELL_LONG_FLAGS: &[&str] = &[
    "norc",
    "noprofile",
    "posix",
    "login",
    "noediting",
    "restricted",
    "verbose",
    "version",
    "help",
    "debugger",
    "dump-strings",
    "dump-po-strings",
    "pretty-print",
];

const GRAMMARS: &[Grammar] = &[
    Grammar {
        names: &[
            "sh", "bash", "dash", "ash", "ksh", "mksh", "pdksh", "oksh", "zsh", "yash", "posh",
            "rbash",
        ],
        flags: "*",
        with_arg: "oO",
        code: "",
        attached: "",
        attached_digits: "",
        data_arg: "",
        long_flags: SHELL_LONG_FLAGS,
        long_with_arg: &[],
        long_code: &["rcfile", "init-file"],
        command_mode: Some('c'),
        reparsed_operands: "",
        attached_values: false,
        plus: true,
        permute: false,
        code_ends_options: false,
    },
    Grammar {
        names: &["fish"],
        flags: "ilNnPvh",
        with_arg: "dofp",
        code: "cC",
        attached: "",
        attached_digits: "",
        data_arg: "",
        long_flags: &[
            "interactive",
            "login",
            "no-config",
            "no-execute",
            "private",
            "print-rusage-self",
            "print-debug-categories",
            "version",
            "help",
        ],
        long_with_arg: &[
            "debug",
            "debug-output",
            "features",
            "profile",
            "profile-startup",
        ],
        long_code: &["command", "init-command"],
        command_mode: None,
        reparsed_operands: "",
        attached_values: false,
        plus: false,
        permute: false,
        code_ends_options: false,
    },
    Grammar {
        names: &["python"],
        flags: "bBdEhiIOPqsSuvVxR?",
        with_arg: "WXQ",
        code: "cm",
        attached: "",
        attached_digits: "",
        data_arg: "",
        long_flags: &["help", "help-env", "help-xoptions", "help-all", "version"],
        long_with_arg: &["check-hash-based-pycs"],
        long_code: &[],
        command_mode: None,
        reparsed_operands: "",
        attached_values: true,
        plus: false,
        permute: false,
        code_ends_options: true,
    },
    Grammar {
        names: &["perl"],
        flags: "acfnpsStTuUwWXvh",
        with_arg: "I",
        code: "eEMm",
        attached: "ixCdDF",
        attached_digits: "0l",
        data_arg: "",
        long_flags: &["help", "version"],
        long_with_arg: &[],
        long_code: &[],
        command_mode: None,
        reparsed_operands: "npF",
        attached_values: true,
        plus: false,
        permute: false,
        code_ends_options: false,
    },
    Grammar {
        names: &["node", "bun", "deno"],
        flags: "hvic",
        with_arg: "",
        code: "epr",
        attached: "",
        attached_digits: "",
        data_arg: "",
        long_flags: &["help", "version", "check", "interactive"],
        long_with_arg: &[],
        long_code: &[
            "eval",
            "print",
            "require",
            "import",
            "loader",
            "experimental-loader",
            "env-file",
        ],
        command_mode: None,
        reparsed_operands: "",
        attached_values: false,
        plus: false,
        permute: false,
        code_ends_options: false,
    },
    Grammar {
        names: &["ruby"],
        flags: "acdhlnpsSvwy",
        with_arg: "IEC",
        code: "er",
        attached: "FKWx",
        attached_digits: "0T",
        data_arg: "",
        long_flags: &["verbose", "version", "help", "copyright", "jit", "yjit"],
        long_with_arg: &[
            "encoding",
            "external-encoding",
            "internal-encoding",
            "enable",
            "disable",
            "dump",
            "backtrace-limit",
            "crash-report",
        ],
        long_code: &[],
        command_mode: None,
        reparsed_operands: "",
        attached_values: true,
        plus: false,
        permute: false,
        code_ends_options: false,
    },
    Grammar {
        names: &["php"],
        flags: "ahHinqsvlmwe",
        with_arg: "tS",
        code: "BcdEfFrRz",
        attached: "",
        attached_digits: "",
        data_arg: "",
        long_flags: &["ini", "help", "version"],
        long_with_arg: &["rf", "rc", "re", "ri", "rz"],
        long_code: &[],
        command_mode: None,
        reparsed_operands: "",
        attached_values: true,
        plus: false,
        permute: false,
        code_ends_options: false,
    },
    Grammar {
        names: &["osascript"],
        flags: "i",
        with_arg: "ls",
        code: "e",
        attached: "",
        attached_digits: "",
        data_arg: "",
        long_flags: &[],
        long_with_arg: &[],
        long_code: &[],
        command_mode: None,
        reparsed_operands: "",
        attached_values: false,
        plus: false,
        permute: false,
        code_ends_options: false,
    },
    Grammar {
        names: &["lua", "luajit"],
        flags: "ivEW",
        with_arg: "",
        code: "el",
        attached: "",
        attached_digits: "",
        data_arg: "",
        long_flags: &[],
        long_with_arg: &[],
        long_code: &[],
        command_mode: None,
        reparsed_operands: "",
        attached_values: true,
        plus: false,
        permute: false,
        code_ends_options: false,
    },
    Grammar {
        names: &["rscript"],
        flags: "",
        with_arg: "",
        code: "e",
        attached: "",
        attached_digits: "",
        data_arg: "",
        long_flags: &[
            "vanilla",
            "verbose",
            "no-environ",
            "no-site-file",
            "no-init-file",
            "restore",
            "no-restore",
            "save",
            "no-save",
            "help",
            "version",
        ],
        long_with_arg: &["default-packages"],
        long_code: &[],
        command_mode: None,
        reparsed_operands: "",
        attached_values: false,
        plus: false,
        permute: false,
        code_ends_options: false,
    },
    Grammar {
        names: &["awk", "gawk", "mawk", "nawk"],
        flags: "bcCghLMnNOPrsStV",
        with_arg: "FvW",
        code: "eEfil",
        attached: "dDop",
        attached_digits: "",
        data_arg: "v",
        long_flags: &[
            "posix",
            "traditional",
            "sandbox",
            "characters-as-bytes",
            "copyright",
            "version",
            "help",
        ],
        long_with_arg: &["field-separator", "assign"],
        long_code: &["source", "file", "exec", "include", "load"],
        command_mode: None,
        reparsed_operands: "",
        attached_values: true,
        plus: false,
        permute: true,
        code_ends_options: false,
    },
    Grammar {
        names: &["sed", "gsed"],
        flags: "nErsuz",
        with_arg: "l",
        code: "ef",
        attached: "i",
        attached_digits: "",
        data_arg: "",
        long_flags: &[
            "quiet",
            "silent",
            "debug",
            "posix",
            "regexp-extended",
            "separate",
            "unbuffered",
            "null-data",
            "sandbox",
            "follow-symlinks",
            "in-place",
            "help",
            "version",
        ],
        long_with_arg: &["line-length"],
        long_code: &["expression", "file"],
        command_mode: None,
        reparsed_operands: "",
        attached_values: true,
        plus: false,
        permute: true,
        code_ends_options: false,
    },
];

/// What the argument after an option token is.
#[derive(Clone, Copy, PartialEq)]
enum Next {
    None,
    Code,
    Arg,
    Data,
}

fn check_interpreter(name: &str, g: &Grammar, args: &[Template]) -> Result<(), String> {
    let mut code_given = false;
    let mut command_mode = false;
    let mut reparsed = false;
    // Indices of the operands, in order.
    let mut operands = Vec::new();
    let mut options_done = false;
    let mut i = 0;
    while let Some(arg) = args.get(i) {
        i += 1;
        if options_done || !is_option(arg, g.plus) {
            operands.push(i - 1);
            options_done |= !g.permute;
            continue;
        }
        let Some(s) = arg.as_literal() else {
            // `-c{x}`, `-e{x}`: the placeholder is (part of) the code.
            let letters = arg.literal_prefix().trim_start_matches(['-', '+']);
            let code = letters
                .chars()
                .any(|c| g.code.contains(c) || g.command_mode == Some(c));
            return Err(if code {
                code_error(name, arg)
            } else {
                option_error(name, arg)
            });
        };
        if s == "--" {
            options_done = true;
            continue;
        }
        let mut next = Next::None;
        if let Some(long) = s.strip_prefix("--") {
            let (opt, inline) = match long.split_once('=') {
                Some((o, _)) => (o, true),
                None => (long, false),
            };
            if g.long_code.contains(&opt) {
                code_given = true;
                next = if inline { Next::None } else { Next::Code };
            } else if g.long_with_arg.contains(&opt) {
                next = if inline { Next::None } else { Next::Arg };
            } else if !g.long_flags.contains(&opt) {
                return Err(unknown_error(name, s));
            }
        } else {
            let body: Vec<char> = s[1..].chars().collect();
            let mut k = 0;
            while let Some(&c) = body.get(k) {
                k += 1;
                reparsed |= g.reparsed_operands.contains(c);
                let takes_code = g.code.contains(c);
                if takes_code || g.with_arg.contains(c) {
                    code_given |= takes_code;
                    if g.attached_values && k < body.len() {
                        // The rest of the token is the argument.
                        break;
                    }
                    next = match (takes_code, next) {
                        // `node -pe CODE`: both take the one next argument.
                        (true, _) | (false, Next::Code) => Next::Code,
                        (false, _) if g.data_arg.contains(c) => Next::Data,
                        (false, _) => Next::Arg,
                    };
                    if g.attached_values {
                        break;
                    }
                } else if g.attached.contains(c) {
                    break;
                } else if g.attached_digits.contains(c) {
                    // Octal digits, or `x` and hex digits after `-0`.
                    let hex = c == '0' && body.get(k) == Some(&'x');
                    k += usize::from(hex);
                    while body
                        .get(k)
                        .is_some_and(|d| d.is_digit(if hex { 16 } else { 8 }))
                    {
                        k += 1;
                    }
                } else if g.command_mode == Some(c) {
                    command_mode = true;
                } else if !(g.flags == "*" && c.is_ascii_alphanumeric()) && !g.flags.contains(c) {
                    return Err(unknown_error(name, s));
                }
            }
        }
        match (next, args.get(i)) {
            (Next::Code, Some(a)) if !a.is_literal() => return Err(code_error(name, a)),
            (Next::Arg, Some(a)) if !a.is_literal() => return Err(option_error(name, a)),
            _ => {}
        }
        if next != Next::None {
            i += 1;
        }
        options_done |= code_given && g.code_ends_options;
    }
    let evaluates_first = g.names.contains(&"awk") || g.names.contains(&"sed");
    for (n, &i) in operands.iter().enumerate() {
        let arg = &args[i];
        if arg.is_literal() {
            continue;
        }
        if n == 0 && (command_mode || (!code_given && evaluates_first)) {
            return Err(code_error(name, arg));
        }
        if n == 0 && !code_given {
            return Err(program_error(name, arg));
        }
        if reparsed {
            return Err(format!(
                "`{name}` opens its file operands in a way that runs commands (`cmd|`), so the \
                 placeholder in `{arg}` is refused"
            ));
        }
    }
    Ok(())
}

/// `find`: the commands after `-exec` and friends are checked like actions
/// of their own; files written by `-fprint` must be named in the definition.
fn check_find(args: &[Template]) -> Result<(), String> {
    let mut i = 0;
    while let Some(arg) = args.get(i) {
        match arg.as_literal() {
            Some("-exec" | "-execdir" | "-ok" | "-okdir") => {
                let start = i + 1;
                let end = args[start..]
                    .iter()
                    .position(|a| matches!(a.as_literal(), Some(";" | "+")))
                    .map_or(args.len(), |p| start + p);
                if let Some((program, rest)) = args[start..end].split_first() {
                    let Some(literal) = program.as_literal() else {
                        return Err(program_error("find", program));
                    };
                    check_command(&program_name(Path::new(literal)), rest)?;
                }
                i = end + 1;
                continue;
            }
            Some("-fprint" | "-fprint0" | "-fprintf" | "-fls") => literal_arg("find", args, i + 1)?,
            Some(_) => {}
            None if is_option(arg, false) => return Err(option_error("find", arg)),
            None => {}
        }
        i += 1;
    }
    Ok(())
}

/// Global options of `git` whose argument follows as the next token.
const GIT_WITH_ARG: &[&str] = &[
    "-c",
    "-C",
    "--config-env",
    "--git-dir",
    "--work-tree",
    "--namespace",
    "--exec-path",
    "--super-prefix",
    "--attr-source",
    "--list-cmds",
];

const GIT_FLAGS: &[&str] = &[
    "-p",
    "--paginate",
    "-P",
    "--no-pager",
    "--no-replace-objects",
    "--no-lazy-fetch",
    "--no-optional-locks",
    "--no-advice",
    "--bare",
    "--literal-pathspecs",
    "--glob-pathspecs",
    "--noglob-pathspecs",
    "--icase-pathspecs",
    "-v",
    "--version",
    "-h",
    "--help",
    "--html-path",
    "--man-path",
    "--info-path",
];

/// `git`: settings given with `-c` run commands (`core.pager`,
/// `core.sshCommand`, aliases), so the global options must be written out,
/// as must the subcommand; `git config` with placeholders is refused too.
fn check_git(args: &[Template]) -> Result<(), String> {
    let mut i = 0;
    while let Some(arg) = args.get(i) {
        if !is_option(arg, false) {
            let Some(sub) = arg.as_literal() else {
                return Err(program_error("git", arg));
            };
            if sub == "config"
                && let Some(a) = args[i + 1..].iter().find(|a| !a.is_literal())
            {
                return Err(format!(
                    "`git config` would store the placeholder in `{a}` as a setting, and \
                     settings run commands"
                ));
            }
            return Ok(());
        }
        let Some(s) = arg.as_literal() else {
            return Err(option_error("git", arg));
        };
        let opt = s.split_once('=').map_or(s, |(o, _)| o);
        if GIT_WITH_ARG.contains(&s) {
            i += 1;
            literal_arg("git", args, i)?;
        } else if !(opt.starts_with("--") && GIT_WITH_ARG.contains(&opt)) && !GIT_FLAGS.contains(&s)
        {
            return Err(unknown_error("git", s));
        }
        i += 1;
    }
    Ok(())
}

/// `cargo`: `--config` sets options that run programs (`rustc-wrapper`,
/// `target.<triple>.runner`), anywhere on the command line; the subcommand
/// may be an external `cargo-<name>` program, so it must be written out.
fn check_cargo(args: &[Template]) -> Result<(), String> {
    let mut subcommand = false;
    let mut i = 0;
    while let Some(arg) = args.get(i) {
        match arg.as_literal() {
            Some("--") => break,
            Some("--config") => {
                i += 1;
                literal_arg("cargo", args, i)?;
            }
            Some("-C" | "-Z" | "--color" | "--explain") if !subcommand => {
                i += 1;
                literal_arg("cargo", args, i)?;
            }
            Some(s) if s.starts_with('-') || s.starts_with('+') => {}
            Some(_) => subcommand = true,
            None if arg.literal_prefix().starts_with("--config") => {
                return Err(option_error("cargo", arg));
            }
            None if !subcommand => {
                return Err(if is_option(arg, true) {
                    option_error("cargo", arg)
                } else {
                    program_error("cargo", arg)
                });
            }
            None => {}
        }
        i += 1;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(command: &[&str]) -> Result<(), String> {
        let (program, rest) = command.split_first().unwrap();
        let argv = rest
            .iter()
            .map(|a| Template::parse(a).unwrap())
            .collect::<Vec<_>>();
        refuse_reparsed_inputs(Path::new(program), &argv)
    }

    #[test]
    fn program_names_are_normalised() {
        for (path, name) in [
            ("/usr/bin/python3.12", "python"),
            ("python3", "python"),
            ("/opt/pypy3", "python"),
            ("perl5.36", "perl"),
            ("nodejs", "node"),
            ("BASH.EXE", "bash"),
            ("ksh93", "ksh"),
            ("sh", "sh"),
            ("7z", "7z"),
        ] {
            assert_eq!(program_name(Path::new(path)), name, "{path}");
        }
    }

    #[test]
    fn refuses_code_positions() {
        for command in [
            // The single-flag forms refused before.
            &["sh", "-c", "echo {x}"][..],
            &["bash", "-c", "{x}"],
            &["sh", "-c{x}"],
            &["zsh", "-c", "{x}"],
            // Flag clusters.
            &["sh", "-ec", "{x}"],
            &["bash", "-lc", "{x}"],
            &["bash", "-cl", "{x}"],
            &["python3", "-uc", "{x}"],
            &["python3", "-c{x}"],
            &["python3", "-uc{x}"],
            &["perl", "-le", "{x}"],
            &["perl", "-M{x}", "-e", "1"],
            &["perl", "-e", "1", "-M{x}"],
            &["node", "-pe", "{x}"],
            &["node", "--eval", "{x}"],
            &["node", "--eval={x}"],
            &["ruby", "-e", "{x}"],
            &["php", "-r", "{x}"],
            // The command string is the first operand after all options.
            &["sh", "-c", "-e", "{x}"],
            &["sh", "-c", "--", "{x}"],
            &["bash", "-c", "-o", "errexit", "{x}"],
            &["bash", "+o", "{x}", "-c", "true"],
            // Modules and scripts are code too.
            &["python3", "-m", "{x}"],
            &["python3.12", "-c", "{x}"],
            &["python3", "{x}"],
            &["bash", "{x}"],
            &["bash", "--rcfile", "{x}", "-c", "true"],
            // Programs that evaluate their first operand.
            &["awk", "{x}"],
            &["sed", "{x}", "file"],
            &["sed", "-n", "-e", "{x}"],
            &["sed", "s/a/b/", "file", "-e", "{x}"],
            &["sed", "-i", "", "-e", "{x}", "file"],
            &["osascript", "-e", "{x}"],
            &["lua", "-e", "{x}"],
            &["Rscript", "-e", "{x}"],
            // perl -n/-p open their file operands with `cmd|` semantics.
            &["perl", "-ne", "print", "{x}"],
        ] {
            let e = check(command).expect_err(&format!("{command:?} should be refused"));
            assert!(
                e.contains("re-parse")
                    || e.contains("option")
                    || e.contains("named by")
                    || e.contains("runs commands"),
                "{command:?}: {e}"
            );
        }
        let e = check(&["perl", "-ne", "print", "{x}"]).unwrap_err();
        assert!(e.contains("runs commands"), "{e}");
    }

    #[test]
    fn resolves_wrappers() {
        for command in [
            &["env", "sh", "-c", "{x}"][..],
            &["env", "-i", "A=1", "bash", "-c", "{x}"],
            &["/usr/bin/env", "--", "sh", "-c", "{x}"],
            &["env", "-", "sh", "-c", "{x}"],
            &["env", "{x}"],
            &["env", "-S", "{x}"],
            &["env", "LD_PRELOAD={x}", "cargo", "test"],
            &["nice", "-n", "5", "sh", "-c", "{x}"],
            &["nice", "-10", "sh", "-c", "{x}"],
            &["timeout", "-s", "KILL", "10", "sh", "-c", "{x}"],
            &["timeout", "10", "{x}"],
            &["stdbuf", "-oL", "sh", "-c", "{x}"],
            &["xargs", "sh", "-c", "{x}"],
            &["xargs", "-I", "{x}", "echo"],
            &["busybox", "sh", "-c", "{x}"],
            &["nohup", "env", "timeout", "5", "python3", "-c", "{x}"],
            &["find", ".", "-exec", "sh", "-c", "{x}", ";"],
            &["find", ".", "-exec", "{x}", ";"],
            &["find", ".", "-fprint", "{x}"],
        ] {
            check(command).expect_err(&format!("{command:?} should be refused"));
        }
    }

    #[test]
    fn refuses_settings_that_run_commands() {
        for command in [
            &["git", "-c", "{x}", "status"][..],
            &["git", "-c", "core.pager={x}", "log"],
            &["git", "--config-env={x}", "log"],
            &["git", "-C", "{x}", "status"],
            &["git", "{x}"],
            &["git", "config", "core.pager", "{x}"],
            &["git", "--unknown", "log", "{x}"],
            &["cargo", "--config", "{x}", "build"],
            &["cargo", "build", "--config", "{x}"],
            &["cargo", "build", "--config={x}"],
            &["cargo", "{x}"],
            &["cargo", "-C", "{x}", "build"],
            &["make", "test", "T={x}"],
            &["sudo", "ls", "{x}"],
            &["ssh", "host", "ls", "{x}"],
            &["open", "{x}"],
        ] {
            check(command).expect_err(&format!("{command:?} should be refused"));
        }
    }

    #[test]
    fn keeps_placeholders_in_data_positions() {
        for command in [
            // The documented pattern: literal code, the input as `$1`.
            &["sh", "-c", "echo \"$1\"", "sh", "{x}"][..],
            &["bash", "-ec", "echo \"$1\"", "bash", "{x}"],
            &["sh", "-c", "echo hi", "{x}"],
            &["python3", "-c", "import sys; print(sys.argv[1])", "{x}"],
            &["python3", "-u", "script.py", "{x}"],
            &["python3", "-m", "pytest", "-k", "{x}"],
            &["perl", "-e", "print @ARGV", "{x}"],
            &["node", "script.js", "{x}"],
            &["awk", "-v", "n={x}", "{{ print n }}", "file"],
            &["awk", "{{ print }}", "{x}"],
            &["sed", "-n", "s/a/b/p", "{x}"],
            &["sed", "-e", "s/a/b/", "{x}"],
            // Wrappers around ordinary programs.
            &["env", "FOO=1", "cargo", "test", "{x}"],
            &["env", "FOO={x}", "cargo", "test"],
            &["nice", "-n", "5", "cargo", "build", "-p", "{x}"],
            &["timeout", "{x}", "cargo", "test"],
            &["env", "sh", "-c", "echo \"$1\"", "sh", "{x}"],
            &[
                "find", ".", "-name", "{x}", "-exec", "grep", "-l", "foo", "{{}}", "+",
            ],
            // git and cargo with literal settings.
            &[
                "git",
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "core.fsmonitor=false",
                "diff",
                "--",
                "{x}",
            ],
            &["git", "--no-pager", "log", "-n", "{x}"],
            &["git", "commit", "-c", "{x}"],
            &["cargo", "test", "-p", "{x}"],
            &["cargo", "+nightly", "test", "{x}"],
            &["cargo", "run", "--", "--config", "{x}"],
            // Programs Claustrum knows nothing special about.
            &["/bin/echo", "{x}"],
            &["rustfmt", "--edition", "2024", "{x}"],
            // Literal commands, whatever they are.
            &["sh", "-c", "echo hi"],
            &["make", "test"],
        ] {
            check(command).unwrap_or_else(|e| panic!("{command:?} should pass: {e}"));
        }
    }
}
