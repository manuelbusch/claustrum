//! Running a bound action on the host.
//!
//! The program gets exactly the argv from [`Bound`], a clean environment, a
//! closed stdin, its own process group and, when a [`Profile`] is given, the
//! OS sandbox. Output is captured with a limit,
//! the wall-clock timeout and the guest's cancel flag both end in a SIGKILL
//! of the whole group, as does the program's own exit for whatever it left
//! running there. The protected files (the Claustrum configuration) are put
//! back if the program touched them.

use std::{
    collections::BTreeMap,
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant},
};

use claustrum_confine::Profile;
use serde::{Deserialize, Serialize};

use super::{bind::Bound, spec::ActionSpec};
use crate::hostcmd::Cancel;

/// How often the child is polled for exit, cancel and timeout.
const POLL: Duration = Duration::from_millis(50);

/// Exit code reported when the timeout or the cancel flag killed the program.
pub const KILLED_EXIT_CODE: i32 = 124;

/// Result of one action run. Serialisable because a confined worker gets it
/// from the broker.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ActionOutcome {
    pub exit_code: i32,
    /// Set when the timeout or the guest's death killed the program.
    pub killed: bool,
    #[serde(with = "base64_bytes")]
    pub stdout: Vec<u8>,
    pub stdout_truncated: bool,
    #[serde(with = "base64_bytes")]
    pub stderr: Vec<u8>,
    pub stderr_truncated: bool,
    pub duration: Duration,
    /// Protected files the program changed and Claustrum restored.
    pub restored: Vec<PathBuf>,
    /// Protected files the program changed and Claustrum could not restore.
    #[serde(default)]
    pub restore_failed: Vec<PathBuf>,
    /// Network refusals (through the proxy) during the run, for the model.
    pub network_notes: Vec<String>,
}

mod base64_bytes {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(d)?;
        STANDARD.decode(text).map_err(serde::de::Error::custom)
    }
}

/// Run the program. Errors are host-side failures to start it at all.
pub(crate) fn execute(
    spec: &ActionSpec,
    bound: &Bound,
    protected: &[PathBuf],
    extra_env: &BTreeMap<String, String>,
    cancel: &Cancel,
    profile: Option<&Profile>,
) -> Result<ActionOutcome, String> {
    let started = Instant::now();
    let snapshot = snapshot(protected);

    let mut command = match profile {
        Some(p) => claustrum_confine::command(p, &spec.program).map_err(|e| e.to_string())?,
        None => Command::new(&spec.program),
    };
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
        // Last, so that nothing in the definition can point around the proxy
        // or the private temporary directory.
        .envs(extra_env)
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
    let limit = spec.max_output_bytes;
    let out = Drain::start(child.stdout.take().expect("stdout is piped"), limit);
    let err = Drain::start(child.stderr.take().expect("stderr is piped"), limit);

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
    // An action is one job: whatever it left running in its process group
    // goes with it, instead of holding the output pipes open.
    kill_process_group(&child);
    // Output still in the pipes arrives at once. A descendant that left the
    // group (`setsid`, a daemon) may keep them open for good; it must not
    // hold up the action, and with it every later one.
    let until = Instant::now() + DRAIN_GRACE;
    let (out, out_done) = out.finish(until);
    let (mut err, err_done) = err.finish(until);
    if !(out_done && err_done) {
        tracing::warn!(
            action = spec.name,
            "a process started by the action still holds its output open; stopped reading"
        );
        err.0.extend_from_slice(
            b"\nclaustrum: a background process started by the action still holds its output \
              open; stopped reading\n",
        );
    }
    let status =
        status.map_err(|e| format!("waiting for `{}` failed: {e}", spec.program.display()))?;

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
        restored: Vec::new(),
        restore_failed: Vec::new(),
        network_notes: Vec::new(),
    };
    (outcome.restored, outcome.restore_failed) = restore(&snapshot);
    let names = |paths: &[PathBuf]| {
        paths
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
    };
    if !outcome.restored.is_empty() {
        let names = names(&outcome.restored);
        tracing::warn!(action = spec.name, files = ?names, "action changed protected files; restored");
        outcome.stderr.extend_from_slice(
            format!(
                "\nclaustrum: the action modified protected file(s) {}; the original contents were restored\n",
                names.join(", ")
            )
            .as_bytes(),
        );
    }
    if !outcome.restore_failed.is_empty() {
        let names = names(&outcome.restore_failed);
        tracing::error!(action = spec.name, files = ?names, "action changed protected files; restoring them failed");
        outcome.stderr.extend_from_slice(
            format!(
                "\nclaustrum: the action modified protected file(s) {} and they could NOT be restored; inspect them before the next run\n",
                names.join(", ")
            )
            .as_bytes(),
        );
    }
    if (!outcome.restored.is_empty() || !outcome.restore_failed.is_empty())
        && outcome.exit_code == 0
    {
        outcome.exit_code = 1;
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
    kill_process_group(child);
    let _ = child.kill();
}

/// SIGKILL the child's process group. The child is its leader (see
/// `execute`), so this reaches everything it spawned that did not start a
/// group or session of its own. After the child exited, the id stays
/// reserved for the group while any member is alive; without members
/// killpg fails with ESRCH (short of the pid space wrapping around first).
fn kill_process_group(child: &std::process::Child) {
    #[cfg(unix)]
    {
        // SAFETY: killpg has no memory-safety preconditions; a group
        // without members fails with ESRCH.
        unsafe {
            libc::killpg(child.id() as libc::pid_t, libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    let _ = child;
}

/// How long the output is still read after the action ended.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// Output captured by a reader thread of its own. The thread is detached:
/// if a leftover process keeps the pipe open it stays blocked in `read`
/// (holding at most `limit` bytes) until that process exits, but the action
/// does not wait for it.
struct Drain {
    captured: Arc<Mutex<(Vec<u8>, bool)>>,
    done: mpsc::Receiver<()>,
}

impl Drain {
    fn start(reader: impl Read + Send + 'static, limit: usize) -> Drain {
        let captured = Arc::new(Mutex::new((Vec::new(), false)));
        let (tx, done) = mpsc::channel();
        let buf = Arc::clone(&captured);
        std::thread::spawn(move || {
            drain(reader, limit, &buf);
            drop(tx);
        });
        Drain { captured, done }
    }

    /// What was read so far, and whether the stream reached its end, waiting
    /// for that at most until `until`.
    fn finish(self, until: Instant) -> ((Vec<u8>, bool), bool) {
        let wait = until.saturating_duration_since(Instant::now());
        // The sender is only dropped, never used: disconnected means done.
        let done = matches!(
            self.done.recv_timeout(wait),
            Err(mpsc::RecvTimeoutError::Disconnected)
        );
        let mut captured = self.captured.lock().unwrap_or_else(|e| e.into_inner());
        (std::mem::take(&mut *captured), done)
    }
}

/// Read a stream to EOF, keeping at most `limit` bytes in `captured`.
fn drain(mut reader: impl Read, limit: usize, captured: &Mutex<(Vec<u8>, bool)>) {
    let mut chunk = [0u8; 8192];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                let mut c = captured.lock().unwrap_or_else(|e| e.into_inner());
                let (buf, truncated) = &mut *c;
                let room = limit.saturating_sub(buf.len());
                let take = room.min(n);
                buf.extend_from_slice(&chunk[..take]);
                if take < n {
                    *truncated = true;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
}

/// State of a protected path, observed without following links.
#[derive(Debug, PartialEq)]
enum Entry {
    Absent,
    /// A regular file: contents, permissions and identity.
    File {
        bytes: Vec<u8>,
        mode: Option<u32>,
        id: Option<(u64, u64)>,
    },
    /// A link, directory or anything else; never read or written through.
    Other,
}

impl Entry {
    fn observe(path: &Path) -> Entry {
        let meta = match std::fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Entry::Absent,
            Err(_) => return Entry::Other,
        };
        if !meta.is_file() {
            return Entry::Other;
        }
        let mut opts = std::fs::OpenOptions::new();
        opts.read(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::custom_flags(&mut opts, libc::O_NOFOLLOW);
        let mut bytes = Vec::new();
        match opts.open(path).and_then(|mut f| f.read_to_end(&mut bytes)) {
            Ok(_) => Entry::File {
                bytes,
                mode: mode(&meta),
                id: file_id(&meta),
            },
            Err(_) => Entry::Other,
        }
    }

    /// Same contents and permissions in the same file (a hard link to
    /// another file with equal bytes still counts as changed).
    fn unchanged(&self, before: &Entry) -> bool {
        match (self, before) {
            (Entry::Absent, Entry::Absent) => true,
            (
                Entry::File {
                    bytes: a,
                    mode: ma,
                    id: ia,
                },
                Entry::File {
                    bytes: b,
                    mode: mb,
                    id: ib,
                },
            ) => a == b && ma == mb && ia == ib,
            _ => false,
        }
    }
}

#[cfg(unix)]
fn mode(meta: &std::fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    Some(meta.permissions().mode() & 0o7777)
}

#[cfg(not(unix))]
fn mode(_meta: &std::fs::Metadata) -> Option<u32> {
    None
}

#[cfg(unix)]
fn file_id(meta: &std::fs::Metadata) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    Some((meta.dev(), meta.ino()))
}

#[cfg(not(unix))]
fn file_id(_meta: &std::fs::Metadata) -> Option<(u64, u64)> {
    None
}

/// State of each protected file before the action. The paths are resolved
/// (see `protect::resolve_host_path`), so neither they nor their parents are
/// links at this point.
fn snapshot(protected: &[PathBuf]) -> Vec<(PathBuf, Entry)> {
    protected
        .iter()
        .map(|p| (p.clone(), Entry::observe(p)))
        .collect()
}

/// Put back every protected file the action changed; returns the paths
/// restored and those that could not be.
///
/// This runs outside any OS sandbox, so it never writes through what the
/// action left behind: links in place of the file or of a parent directory
/// are removed, directories are moved aside instead of deleted, and the
/// original contents go to a new file that is renamed into place.
fn restore(snapshot: &[(PathBuf, Entry)]) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let mut restored = Vec::new();
    let mut failed = Vec::new();
    for (path, before) in snapshot {
        if ancestors_intact(path) && Entry::observe(path).unchanged(before) {
            continue;
        }
        match restore_one(path, before) {
            Ok(()) => restored.push(path.clone()),
            Err(e) => {
                tracing::error!(path = %path.display(), error = %e, "cannot restore protected file");
                failed.push(path.clone());
            }
        }
    }
    (restored, failed)
}

fn restore_one(path: &Path, before: &Entry) -> std::io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("protected path has no parent"))?;
    unlink_ancestors(parent)?;
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => move_aside(path)?,
        Ok(_) if !matches!(before, Entry::File { .. }) => std::fs::remove_file(path)?,
        Ok(_) | Err(_) => {}
    }
    let Entry::File { bytes, mode, .. } = before else {
        return Ok(());
    };
    if !parent.is_dir() {
        std::fs::create_dir_all(parent)?;
    }
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let tmp = parent.join(format!(".{name}.claustrum-restore-{}", std::process::id()));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NOFOLLOW);
        opts.mode(mode.unwrap_or(0o644));
    }
    #[cfg(not(unix))]
    let _ = mode;
    let written = opts.open(&tmp).and_then(|mut f| {
        use std::io::Write;
        f.write_all(bytes)?;
        // The mode given to open is masked by the umask; set it exactly.
        #[cfg(unix)]
        if let Some(m) = mode {
            use std::os::unix::fs::PermissionsExt;
            f.set_permissions(std::fs::Permissions::from_mode(*m))?;
        }
        f.sync_all()
    });
    // rename replaces a file or link at `path` without following it.
    let result = written.and_then(|()| std::fs::rename(&tmp, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Replace every ancestor of `dir` that the action turned into a link with a
/// plain directory again (at snapshot time none was a link).
fn unlink_ancestors(dir: &Path) -> std::io::Result<()> {
    let mut current = PathBuf::new();
    for c in dir.components() {
        current.push(c);
        match std::fs::symlink_metadata(&current) {
            Ok(m) if m.file_type().is_symlink() => {
                tracing::warn!(path = %current.display(), "action replaced a directory above a protected file with a link; removing the link");
                std::fs::remove_file(&current)?;
                std::fs::create_dir(&current)?;
            }
            Ok(m) if !m.is_dir() => {
                move_aside(&current).and_then(|()| std::fs::create_dir(&current))?
            }
            Ok(_) => {}
            // Nothing below can exist; the file is recreated with its
            // parents only if it existed before.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Whether every existing ancestor of `path` is still a plain directory.
fn ancestors_intact(path: &Path) -> bool {
    let mut current = PathBuf::new();
    let Some(parent) = path.parent() else {
        return true;
    };
    for c in parent.components() {
        current.push(c);
        match std::fs::symlink_metadata(&current) {
            Ok(m) if m.is_dir() => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return true,
            _ => return false,
        }
    }
    true
}

/// Rename `path` to a free `<name>.claustrum-moved[-N]` next to it, so that
/// nothing the action placed there is deleted (or recursed into).
fn move_aside(path: &Path) -> std::io::Result<()> {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    for n in 0..100 {
        let suffix = if n == 0 {
            String::new()
        } else {
            format!("-{n}")
        };
        let target = path.with_file_name(format!("{name}.claustrum-moved{suffix}"));
        if std::fs::symlink_metadata(&target).is_err() {
            tracing::warn!(from = %path.display(), to = %target.display(), "moved aside what an action put in place of a protected file");
            return std::fs::rename(path, target);
        }
    }
    Err(std::io::Error::other(
        "no free name to move the entry aside",
    ))
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
            resolve_programs: true,
        })
        .unwrap()
    }

    fn run(spec: &ActionSpec, ws: &Path, protected: &[PathBuf]) -> ActionOutcome {
        let bound = bind(spec, &[], &BTreeMap::new(), crate::WORKSPACE, ws).unwrap();
        execute(
            spec,
            &bound,
            protected,
            &BTreeMap::new(),
            &Cancel::new(),
            None,
        )
        .unwrap()
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
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            ws.display().to_string()
        );

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

    fn pid_in(file: &Path) -> i32 {
        std::fs::read_to_string(file)
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    }

    fn alive(pid: i32) -> bool {
        // SAFETY: signal 0 only checks for existence.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    #[test]
    fn background_children_do_not_hold_up_a_finished_action() {
        let ws = tempfile::tempdir().unwrap();
        let ws = ws.path().canonicalize().unwrap();
        // The background sleep inherits stdout and would keep it open.
        let s = spec(
            &ws,
            &[
                "/bin/sh",
                "-c",
                "/bin/sleep 30 & echo $! > gpid; echo started",
            ],
            None,
        );
        let started = Instant::now();
        let out = run(&s, &ws, &[]);
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(out.exit_code, 0);
        assert!(!out.killed);
        assert_eq!(String::from_utf8_lossy(&out.stdout), "started\n");
        assert!(out.stderr.is_empty(), "{out:?}");
        std::thread::sleep(Duration::from_millis(200));
        let gpid = pid_in(&ws.join("gpid"));
        assert!(!alive(gpid), "background child {gpid} survived");
    }

    #[test]
    fn a_descendant_in_its_own_session_does_not_hold_up_the_action() {
        if !Path::new("/usr/bin/perl").exists() {
            eprintln!("skipped: needs /usr/bin/perl for setsid");
            return;
        }
        let ws = tempfile::tempdir().unwrap();
        let ws = ws.path().canonicalize().unwrap();
        // Out of reach of killpg, holding stdout and stderr open.
        let s = spec(
            &ws,
            &[
                "/bin/sh",
                "-c",
                "/usr/bin/perl -MPOSIX -e 'setsid(); open(my $f, \">\", \"ready\"); \
                 close($f); exec \"/bin/sleep\", \"30\"' & echo $! > spid; \
                 while [ ! -e ready ]; do /bin/sleep 0.05; done; echo started",
            ],
            Some(10),
        );
        let started = Instant::now();
        let out = run(&s, &ws, &[]);
        let spid = pid_in(&ws.join("spid"));
        // SAFETY: plain kill of the test's own leftover.
        unsafe { libc::kill(spid, libc::SIGKILL) };
        assert!(
            started.elapsed() < DRAIN_GRACE + Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(out.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&out.stdout), "started\n");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("stopped reading"),
            "{out:?}"
        );
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
        let out = execute(&s, &bound, &[], &BTreeMap::new(), &cancel, None).unwrap();
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

    #[cfg(unix)]
    #[test]
    fn changed_permissions_are_restored() {
        use std::os::unix::fs::PermissionsExt;
        let ws = tempfile::tempdir().unwrap();
        let ws = ws.path().canonicalize().unwrap();
        let config = ws.join("claustrum.toml");
        std::fs::write(&config, "# original\n").unwrap();
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o640)).unwrap();
        let s = spec(&ws, &["/bin/sh", "-c", "chmod 666 claustrum.toml"], None);
        let out = run(&s, &ws, std::slice::from_ref(&config));
        assert_eq!(out.restored, [config.clone()]);
        assert!(out.restore_failed.is_empty());
        let mode = std::fs::metadata(&config).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o640);
    }

    #[cfg(unix)]
    #[test]
    fn failed_restores_are_reported() {
        use std::os::unix::fs::PermissionsExt;
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } == 0 {
            eprintln!("skipped: root ignores the directory permissions");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().canonicalize().unwrap();
        let config = ws.join("claustrum.toml");
        std::fs::write(&config, "# original\n").unwrap();
        // The restore needs a new file next to the config, which the
        // read-only directory refuses.
        let s = spec(
            &ws,
            &["/bin/sh", "-c", "echo hacked > claustrum.toml; chmod 555 ."],
            None,
        );
        let out = run(&s, &ws, std::slice::from_ref(&config));
        std::fs::set_permissions(&ws, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(out.restored.is_empty(), "{out:?}");
        assert_eq!(out.restore_failed, [config]);
        assert_eq!(out.exit_code, 1);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("could NOT be restored"), "{stderr}");
        assert!(!stderr.contains("were restored"), "{stderr}");
    }

    /// Runs `script` as an action in a fresh workspace with a protected
    /// `claustrum.toml` and a missing protected `.claude/settings.json`,
    /// next to an outside file `victim`. Returns the workspace, the victim
    /// and the outcome.
    fn attack(script: &str) -> (tempfile::TempDir, PathBuf, PathBuf, ActionOutcome) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let ws = root.join("ws");
        std::fs::create_dir(&ws).unwrap();
        let victim = root.join("victim");
        std::fs::write(&victim, "victim\n").unwrap();
        std::fs::write(ws.join("claustrum.toml"), "# original\n").unwrap();
        let script = script.replace("VICTIM", &victim.display().to_string());
        let s = spec(&ws, &["/bin/sh", "-c", &script], None);
        let protected = [ws.join("claustrum.toml"), ws.join(".claude/settings.json")];
        let out = run(&s, &ws, &protected);
        (tmp, ws, victim, out)
    }

    fn assert_intact(ws: &Path, victim: &Path) {
        let config = ws.join("claustrum.toml");
        assert!(std::fs::symlink_metadata(&config).unwrap().is_file());
        assert_eq!(std::fs::read_to_string(&config).unwrap(), "# original\n");
        assert_eq!(std::fs::read_to_string(victim).unwrap(), "victim\n");
        assert!(!ws.join(".claude/settings.json").exists());
        if let Ok(m) = std::fs::symlink_metadata(ws.join(".claude")) {
            assert!(m.is_dir(), ".claude is not a directory");
        }
    }

    #[cfg(unix)]
    #[test]
    fn restore_never_writes_through_links() {
        for script in [
            // The file itself replaced by a link to an outside file.
            "rm claustrum.toml && ln -s VICTIM claustrum.toml",
            // A hard link: the same bytes in place are not enough.
            "rm claustrum.toml && ln VICTIM claustrum.toml",
            // A directory with contents in place of the file.
            "rm claustrum.toml && mkdir -p claustrum.toml/keep && echo x > claustrum.toml/keep/f",
            // The parent of the missing settings file redirected, with and
            // without a file behind it.
            "mkdir fake && echo hooks > fake/settings.json && ln -s fake .claude",
            "mkdir empty && ln -s empty .claude",
            "ln -s \"$(dirname VICTIM)\" .claude",
        ] {
            let (_tmp, ws, victim, out) = attack(script);
            assert_intact(&ws, &victim);
            assert!(!out.restored.is_empty(), "{script}: nothing restored");
        }
    }

    #[cfg(unix)]
    #[test]
    fn restore_moves_directories_aside() {
        let (_tmp, ws, _victim, _out) = attack(
            "rm claustrum.toml && mkdir -p claustrum.toml/keep && echo x > claustrum.toml/keep/f",
        );
        assert_eq!(
            std::fs::read_to_string(ws.join("claustrum.toml.claustrum-moved/keep/f")).unwrap(),
            "x\n"
        );
    }
}
