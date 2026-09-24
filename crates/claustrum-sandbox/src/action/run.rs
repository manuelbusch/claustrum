//! Running a bound action on the host.
//!
//! The program gets exactly the argv from [`Bound`], a clean environment, a
//! closed stdin and its own process group. Output is captured with a limit,
//! the wall-clock timeout and the guest's cancel flag both end in a SIGKILL
//! of the whole group, and the protected files (the Claustrum configuration)
//! are put back if the program touched them.

use std::{
    collections::BTreeMap,
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use super::{bind::Bound, spec::ActionSpec};
use crate::hostcmd::Cancel;

/// How often the child is polled for exit, cancel and timeout.
const POLL: Duration = Duration::from_millis(50);

/// Exit code reported when the timeout or the cancel flag killed the program.
pub const KILLED_EXIT_CODE: i32 = 124;

/// Result of one action run.
#[derive(Clone, Debug, Default)]
pub struct ActionOutcome {
    pub exit_code: i32,
    /// Set when the timeout or the guest's death killed the program.
    pub killed: bool,
    pub stdout: Vec<u8>,
    pub stdout_truncated: bool,
    pub stderr: Vec<u8>,
    pub stderr_truncated: bool,
    pub duration: Duration,
    /// Protected files the program changed and Claustrum restored.
    pub restored: Vec<PathBuf>,
}

/// Run the program. Errors are host-side failures to start it at all.
pub(crate) fn execute(
    spec: &ActionSpec,
    bound: &Bound,
    protected: &[PathBuf],
    cancel: &Cancel,
) -> Result<ActionOutcome, String> {
    let started = Instant::now();
    let snapshot = snapshot(protected);

    let mut command = Command::new(&spec.program);
    command
        .args(&bound.argv)
        .current_dir(&spec.cwd)
        .env_clear()
        .envs(base_env())
        .envs(
            spec.env_passthrough
                .iter()
                .filter_map(|k| std::env::var_os(k).map(|v| (k.clone(), v))),
        )
        .envs(&bound.env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    let mut child = command
        .spawn()
        .map_err(|e| format!("cannot start `{}`: {e}", spec.program.display()))?;
    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");
    let limit = spec.max_output_bytes;

    let (status, killed, out, err) = std::thread::scope(|s| {
        let out = s.spawn(move || drain(stdout, limit));
        let err = s.spawn(move || drain(stderr, limit));
        let deadline = spec.timeout.map(|t| started + t);
        let mut killed = false;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Ok(status),
                Ok(None) => {}
                Err(e) => break Err(e),
            }
            let expired = deadline.is_some_and(|d| Instant::now() >= d);
            if expired || cancel.is_cancelled() {
                tracing::warn!(
                    action = spec.name,
                    reason = if expired { "timeout" } else { "guest killed" },
                    "killing host action"
                );
                kill_group(&mut child);
                killed = true;
                break child.wait();
            }
            std::thread::sleep(POLL);
        };
        (
            status,
            killed,
            out.join().unwrap_or_default(),
            err.join().unwrap_or_default(),
        )
    });
    let status = status.map_err(|e| format!("waiting for `{}` failed: {e}", spec.program.display()))?;

    let mut outcome = ActionOutcome {
        exit_code: if killed {
            KILLED_EXIT_CODE
        } else {
            exit_code(&status)
        },
        killed,
        stdout: out.0,
        stdout_truncated: out.1,
        stderr: err.0,
        stderr_truncated: err.1,
        duration: started.elapsed(),
        restored: restore(&snapshot),
    };
    if !outcome.restored.is_empty() {
        let names: Vec<_> = outcome
            .restored
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        tracing::warn!(action = spec.name, files = ?names, "action changed protected files; restored");
        outcome.stderr.extend_from_slice(
            format!(
                "\nclaustrum: the action modified protected file(s) {}; the original contents were restored\n",
                names.join(", ")
            )
            .as_bytes(),
        );
        if outcome.exit_code == 0 {
            outcome.exit_code = 1;
        }
    }
    Ok(outcome)
}

/// The environment every action starts from.
fn base_env() -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    for key in ["PATH", "HOME", "LANG"] {
        if let Ok(v) = std::env::var(key) {
            env.insert(key.to_owned(), v);
        }
    }
    env.insert("TERM".into(), "dumb".into());
    env.insert("NO_COLOR".into(), "1".into());
    env
}

fn exit_code(status: &std::process::ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        if let Some(sig) = status.signal() {
            return 128 + sig;
        }
    }
    1
}

fn kill_group(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        // The child is the leader of its own process group (see `execute`),
        // so this reaches everything it spawned.
        // SAFETY: killpg has no memory-safety preconditions; a stale pid can
        // at worst fail with ESRCH.
        unsafe {
            libc::killpg(child.id() as libc::pid_t, libc::SIGKILL);
        }
    }
    let _ = child.kill();
}

/// Read a stream to EOF, keeping at most `limit` bytes.
fn drain(mut reader: impl Read, limit: usize) -> (Vec<u8>, bool) {
    let mut buf = Vec::new();
    let mut truncated = false;
    let mut chunk = [0u8; 8192];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                let room = limit.saturating_sub(buf.len());
                let take = room.min(n);
                buf.extend_from_slice(&chunk[..take]);
                if take < n {
                    truncated = true;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    (buf, truncated)
}

/// Contents (or absence) of each protected file before the action.
fn snapshot(protected: &[PathBuf]) -> Vec<(PathBuf, Option<Vec<u8>>)> {
    protected
        .iter()
        .map(|p| (p.clone(), std::fs::read(p).ok()))
        .collect()
}

/// Put back every protected file whose contents changed; returns those paths.
fn restore(snapshot: &[(PathBuf, Option<Vec<u8>>)]) -> Vec<PathBuf> {
    let mut restored = Vec::new();
    for (path, before) in snapshot {
        let after = std::fs::read(path).ok();
        if &after == before {
            continue;
        }
        let result = match before {
            Some(bytes) => std::fs::write(path, bytes),
            None => remove(path),
        };
        if let Err(e) = result {
            tracing::error!(path = %path.display(), error = %e, "cannot restore protected file");
        }
        restored.push(path.clone());
    }
    restored
}

fn remove(path: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::{
        bind::bind,
        spec::{ActionDef, CompileContext},
    };

    fn spec(ws: &Path, command: &[&str], timeout: Option<u64>) -> ActionSpec {
        ActionDef {
            name: "t".into(),
            command: command.iter().map(|s| (*s).to_owned()).collect(),
            timeout_secs: timeout,
            env: [("ACTION_VAR".to_owned(), "set".to_owned())].into(),
            ..Default::default()
        }
        .compile(&CompileContext {
            workspace: ws,
            default_timeout: Some(Duration::from_secs(10)),
            max_output_bytes: 4096,
        })
        .unwrap()
    }

    fn run(spec: &ActionSpec, ws: &Path, protected: &[PathBuf]) -> ActionOutcome {
        let bound = bind(spec, &[], &BTreeMap::new(), crate::WORKSPACE, ws).unwrap();
        execute(spec, &bound, protected, &Cancel::new()).unwrap()
    }

    #[test]
    fn passes_argv_verbatim_with_a_clean_environment() {
        let ws = tempfile::tempdir().unwrap();
        let ws = ws.path().canonicalize().unwrap();
        let s = spec(&ws, &["/bin/echo", "$(id)", "a b", "-n"], None);
        let out = run(&s, &ws, &[]);
        assert_eq!(out.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&out.stdout), "$(id) a b -n\n");

        let s = spec(&ws, &["/usr/bin/env"], None);
        let out = run(&s, &ws, &[]);
        let env = String::from_utf8_lossy(&out.stdout);
        let keys: Vec<_> = env.lines().filter_map(|l| l.split('=').next()).collect();
        for k in &keys {
            assert!(
                ["PATH", "HOME", "LANG", "TERM", "NO_COLOR", "ACTION_VAR"].contains(k),
                "unexpected variable {k}"
            );
        }
        assert!(keys.contains(&"ACTION_VAR"));
        assert!(!env.contains("USER=") && !env.contains("SHELL="));
    }

    #[test]
    fn runs_in_the_action_cwd_and_limits_output() {
        let ws = tempfile::tempdir().unwrap();
        let ws = ws.path().canonicalize().unwrap();
        let s = spec(&ws, &["/bin/pwd"], None);
        let out = run(&s, &ws, &[]);
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), ws.display().to_string());

        let s = spec(&ws, &["/usr/bin/yes"], Some(1));
        let out = run(&s, &ws, &[]);
        assert!(out.killed);
        assert_eq!(out.exit_code, KILLED_EXIT_CODE);
        assert_eq!(out.stdout.len(), 4096);
        assert!(out.stdout_truncated);
    }

    #[test]
    fn timeout_kills_the_process_group() {
        let ws = tempfile::tempdir().unwrap();
        let ws = ws.path().canonicalize().unwrap();
        // The child forks a grandchild that outlives it unless the group is killed.
        let s = spec(
            &ws,
            &[
                "/bin/sh",
                "-c",
                "echo $$ > pid; /bin/sleep 30 & echo $! > gpid; wait",
            ],
            Some(1),
        );
        let started = Instant::now();
        let out = run(&s, &ws, &[]);
        assert!(out.killed, "{out:?}");
        assert!(started.elapsed() < Duration::from_secs(5));
        let gpid: i32 = std::fs::read_to_string(ws.join("gpid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        std::thread::sleep(Duration::from_millis(200));
        // SAFETY: signal 0 only checks for existence.
        let alive = unsafe { libc::kill(gpid, 0) } == 0;
        assert!(!alive, "grandchild {gpid} survived");
    }

    #[test]
    fn cancel_flag_stops_the_action() {
        let ws = tempfile::tempdir().unwrap();
        let ws = ws.path().canonicalize().unwrap();
        let s = spec(&ws, &["/bin/sleep", "30"], Some(0));
        let bound = bind(&s, &[], &BTreeMap::new(), crate::WORKSPACE, &ws).unwrap();
        let cancel = Cancel::new();
        let flag = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            flag.cancel();
        });
        let started = Instant::now();
        let out = execute(&s, &bound, &[], &cancel).unwrap();
        assert!(out.killed);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn protected_files_are_restored() {
        let ws = tempfile::tempdir().unwrap();
        let ws = ws.path().canonicalize().unwrap();
        let config = ws.join("claustrum.toml");
        let ghost = ws.join("ghost.toml");
        std::fs::write(&config, "# original\n").unwrap();
        let s = spec(
            &ws,
            &[
                "/bin/sh",
                "-c",
                "echo hacked > claustrum.toml; echo new > ghost.toml; echo ok",
            ],
            None,
        );
        let out = run(&s, &ws, &[config.clone(), ghost.clone()]);
        assert_eq!(std::fs::read_to_string(&config).unwrap(), "# original\n");
        assert!(!ghost.exists());
        assert_eq!(out.restored, vec![config, ghost]);
        assert_eq!(out.exit_code, 1);
        assert!(String::from_utf8_lossy(&out.stderr).contains("restored"));
        assert_eq!(String::from_utf8_lossy(&out.stdout), "ok\n");
    }
}
