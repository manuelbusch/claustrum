//! Linux backend: bubblewrap for namespaces and bind mounts where user
//! namespaces are available, then Landlock and seccomp applied from inside by
//! the helper stage right before the program starts.
//!
//! ```text
//! bwrap [mounts, --unshare-net ...] -- <helper> __confine-exec <json> <program> <args>
//!                                        │ bridge localhost:<port> → proxy socket
//!                                        │ Landlock (files, TCP ports, scopes)
//!                                        │ seccomp (namespaces, ptrace, io_uring, sockets)
//!                                        └ exec <program>
//! ```
//!
//! Landlock alone cannot keep one file read-only inside a writable directory
//! (removing and creating are rights of the parent directory), so the
//! read-only configuration relies on a bubblewrap bind mount. Without
//! bubblewrap (user namespaces disabled, e.g. by AppArmor on Ubuntu 24.04+)
//! the [`Backend::Landlock`] fallback applies everything else and says so.

use std::{
    ffi::OsString,
    io::{self, Read, Write},
    net::{Shutdown, TcpListener, TcpStream},
    os::unix::{net::UnixStream, process::CommandExt as _},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::OnceLock,
};

use landlock::{
    ABI, Access, AccessFs, AccessNet, BitFlags, CompatLevel, Compatible, NetPort, PathBeneath,
    PathFd, Ruleset, RulesetAttr, RulesetCreatedAttr, RulesetStatus, Scope,
};
use serde::{Deserialize, Serialize};

use crate::{Backend, Exec, HELPER_ARG, Network, Profile, Unavailable, helper_binary, resolve};

/// Newest Landlock ABI this code knows; older kernels get what they support.
const ABI_TARGET: ABI = ABI::V6;
/// Directories whose top-level system files every profile may read.
const SYSTEM_DIRS: &[&str] = &["/usr", "/bin", "/sbin", "/lib", "/lib32", "/lib64", "/etc"];
/// Pseudo file systems every profile may read. Landlock also stops a
/// confined process from reading `/proc/<pid>` of processes outside its
/// domain (ptrace access checks).
const PSEUDO_DIRS: &[&str] = &["/proc", "/sys", "/dev"];

/// What the helper receives.
#[derive(Serialize, Deserialize)]
struct Spec {
    profile: Profile,
    /// Started inside bubblewrap (own mount and network namespace).
    bwrap: bool,
}

fn landlock_abi() -> i64 {
    const LANDLOCK_CREATE_RULESET_VERSION: libc::c_uint = 1;
    // SAFETY: with a null attribute pointer and the VERSION flag the call
    // only returns the ABI version (or an error).
    unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<libc::c_void>(),
            0usize,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
    }
}

pub(crate) fn backend() -> Result<Backend, Unavailable> {
    static CACHE: OnceLock<Result<Backend, Unavailable>> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            let abi = landlock_abi();
            if abi < 1 {
                return Err(Unavailable(
                    "Landlock is not available (needs Linux 5.13+ with the landlock LSM enabled)"
                        .into(),
                ));
            }
            if std::env::var_os("CLAUSTRUM_NO_BWRAP").is_none() && bwrap_usable() {
                Ok(Backend::Bubblewrap)
            } else {
                tracing::warn!(
                    landlock_abi = abi,
                    "bubblewrap is missing or cannot create namespaces: confining with Landlock \
                     only (no read-only protection inside writable directories, no private \
                     network namespace)"
                );
                Ok(Backend::Landlock)
            }
        })
        .clone()
}

fn bwrap_path() -> Option<PathBuf> {
    let from_path = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .map(|d| d.join("bwrap"));
    ["/usr/bin/bwrap", "/bin/bwrap"]
        .into_iter()
        .map(PathBuf::from)
        .chain(from_path)
        .find(|p| p.is_file())
}

fn bwrap_usable() -> bool {
    let (Some(bwrap), Some(truth)) = (
        bwrap_path(),
        ["/usr/bin/true", "/bin/true"]
            .into_iter()
            .find(|p| Path::new(p).is_file()),
    ) else {
        return false;
    };
    Command::new(bwrap)
        .args([
            "--ro-bind",
            "/",
            "/",
            "--unshare-user-try",
            "--unshare-pid",
            "--unshare-net",
            "--proc",
            "/proc",
            "--dev",
            "/dev",
            "--",
            truth,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Resolve every path of the profile once, like the kernel will see them.
fn resolved(profile: &Profile) -> Profile {
    let all = |v: &[PathBuf]| v.iter().map(|p| resolve(p)).collect::<Vec<_>>();
    let mut p = profile.clone();
    p.read = all(&profile.read);
    p.write = all(&profile.write);
    p.deny_read = all(&profile.deny_read);
    p.deny_write = all(&profile.deny_write);
    if let Exec::Only(programs) = &profile.exec {
        p.exec = Exec::Only(all(programs));
    }
    p.proxy_socket = profile.proxy_socket.as_deref().map(resolve);
    p
}

pub(crate) fn command(
    backend: Backend,
    profile: &Profile,
    program: &Path,
) -> Result<Command, Unavailable> {
    let helper = helper_binary()?;
    let profile = resolved(profile);
    let bwrap = backend == Backend::Bubblewrap;
    let json = serde_json::to_string(&Spec {
        profile: profile.clone(),
        bwrap,
    })
    .map_err(|e| Unavailable(format!("cannot encode the profile: {e}")))?;
    let mut cmd = if bwrap {
        let path = bwrap_path().ok_or_else(|| Unavailable("bwrap disappeared".into()))?;
        let mut cmd = Command::new(path);
        cmd.args(bwrap_args(&profile, &helper, program)?);
        cmd.arg("--").arg(&helper);
        cmd
    } else {
        Command::new(&helper)
    };
    cmd.arg(HELPER_ARG).arg(json).arg(program);
    Ok(cmd)
}

fn bwrap_args(p: &Profile, helper: &Path, program: &Path) -> Result<Vec<OsString>, Unavailable> {
    let mut a: Vec<OsString> = Vec::new();
    let mut push = |parts: &[&std::ffi::OsStr]| a.extend(parts.iter().map(|s| s.to_os_string()));
    push(&[
        "--die-with-parent".as_ref(),
        "--unshare-user-try".as_ref(),
        "--unshare-pid".as_ref(),
        "--unshare-ipc".as_ref(),
        "--unshare-uts".as_ref(),
        "--unshare-cgroup-try".as_ref(),
    ]);
    if matches!(p.network, Network::None | Network::Loopback(_)) {
        push(&["--unshare-net".as_ref()]);
    }
    if p.read_everything {
        push(&["--ro-bind".as_ref(), "/".as_ref(), "/".as_ref()]);
    } else {
        for dir in SYSTEM_DIRS {
            push(&["--ro-bind-try".as_ref(), dir.as_ref(), dir.as_ref()]);
        }
        // Resolved like the profile: bwrap cannot mount on a link (`/bin/sh`).
        let own = [resolve(helper), resolve(program)];
        for r in p.read.iter().chain(&own) {
            push(&["--ro-bind-try".as_ref(), r.as_os_str(), r.as_os_str()]);
        }
        if let Exec::Only(programs) = &p.exec {
            for r in programs {
                push(&["--ro-bind-try".as_ref(), r.as_os_str(), r.as_os_str()]);
            }
        }
    }
    push(&[
        "--proc".as_ref(),
        "/proc".as_ref(),
        "--dev".as_ref(),
        "/dev".as_ref(),
        "--tmpfs".as_ref(),
        "/tmp".as_ref(),
    ]);
    for w in &p.write {
        if w.exists() {
            push(&["--bind".as_ref(), w.as_os_str(), w.as_os_str()]);
        } else {
            tracing::debug!(path = %w.display(), "writable path does not exist; skipped");
        }
    }
    if let Some(sock) = &p.proxy_socket {
        push(&["--bind".as_ref(), sock.as_os_str(), sock.as_os_str()]);
    }
    // Before the hidden paths: a new bind of an ancestor would uncover them.
    for (dir, open) in guarded_dirs(p)? {
        push(&["--ro-bind".as_ref(), dir.as_os_str(), dir.as_os_str()]);
        for c in open {
            push(&["--bind".as_ref(), c.as_os_str(), c.as_os_str()]);
        }
    }
    for d in &p.deny_read {
        match std::fs::metadata(d) {
            Ok(m) if m.is_dir() => push(&[
                "--tmpfs".as_ref(),
                d.as_os_str(),
                "--remount-ro".as_ref(),
                d.as_os_str(),
            ]),
            Ok(_) => push(&["--ro-bind".as_ref(), "/dev/null".as_ref(), d.as_os_str()]),
            Err(_) => {}
        }
    }
    // Last, so that no later bind covers them.
    for f in &p.deny_write {
        if f.exists() {
            push(&["--ro-bind".as_ref(), f.as_os_str(), f.as_os_str()]);
        }
    }
    Ok(a)
}

/// Directories to bind read-only because a protected file in them does not
/// exist yet (`.claude/settings.json`): a bind mount needs a file, and
/// creating one would change the user's project. Each comes with its
/// children, which stay writable, so only new entries directly in the
/// directory are refused. Outer directories come first.
///
/// A missing file directly in a writable tree cannot be protected this way
/// without making the whole tree read-only, so the profile is refused.
fn guarded_dirs(p: &Profile) -> Result<Vec<(PathBuf, Vec<PathBuf>)>, Unavailable> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    for f in &p.deny_write {
        if f.exists() {
            continue;
        }
        // The innermost writable tree; outside all of them nothing can be
        // created anyway.
        let Some(root) = p
            .write
            .iter()
            .filter(|w| f.starts_with(w) && w.exists())
            .max_by_key(|w| w.components().count())
        else {
            continue;
        };
        let dir = f.ancestors().skip(1).find(|a| a.exists()).unwrap_or(root);
        if dir == root {
            return Err(Unavailable(format!(
                "{}: cannot keep {} from being created in the writable {}",
                p.name,
                f.display(),
                root.display()
            )));
        }
        if !dirs.iter().any(|d| d == dir) {
            dirs.push(dir.to_path_buf());
        }
    }
    dirs.sort_by_key(|d| d.components().count());
    let guarded = |c: &Path| {
        p.deny_write
            .iter()
            .chain(&p.deny_read)
            .chain(&dirs)
            .any(|g| g.starts_with(c))
    };
    let mut out = Vec::new();
    for dir in &dirs {
        let entries = std::fs::read_dir(dir)
            .map_err(|e| Unavailable(format!("{}: cannot list {}: {e}", p.name, dir.display())))?;
        let mut open = Vec::new();
        for e in entries {
            let e = e.map_err(|e| {
                Unavailable(format!("{}: cannot list {}: {e}", p.name, dir.display()))
            })?;
            let c = e.path();
            // A bind follows links, possibly out of the writable tree.
            let is_link = e.file_type().map(|t| t.is_symlink()).unwrap_or(true);
            if !is_link && !guarded(&c) {
                open.push(c);
            }
        }
        out.push((dir.clone(), open));
    }
    Ok(out)
}

/// The helper stage: bridge, Landlock, seccomp, exec. Runs as a fresh,
/// single-threaded process.
pub(crate) fn helper(args: Vec<OsString>) -> ! {
    fn fail(msg: impl std::fmt::Display) -> ! {
        eprintln!("claustrum confinement: {msg}");
        std::process::exit(126)
    }
    let [json, program, rest @ ..] = args.as_slice() else {
        fail(format!("usage: {HELPER_ARG} <profile> <program> [args...]"));
    };
    let spec: Spec = match serde_json::from_str(&json.to_string_lossy()) {
        Ok(s) => s,
        Err(e) => fail(format!("invalid profile: {e}")),
    };
    if spec.bwrap
        && let Network::Loopback(port) = spec.profile.network
        && let Some(sock) = &spec.profile.proxy_socket
        && let Err(e) = start_bridge(port, sock)
    {
        fail(format!("cannot bridge the proxy: {e}"));
    }
    if let Err(e) = apply_landlock(&spec) {
        fail(format!("Landlock: {e}"));
    }
    if let Err(e) = apply_seccomp(&spec) {
        fail(format!("seccomp: {e}"));
    }
    let err = Command::new(program).args(rest).exec();
    fail(format!(
        "cannot run {}: {err}",
        Path::new(program).display()
    ))
}

/// Inside the private network namespace, listen on `localhost:<port>` (the
/// address in the proxy variables) and relay every connection to the proxy's
/// Unix socket, bind-mounted from the host. Runs in a forked child that dies
/// with the program.
fn start_bridge(port: u16, sock: &Path) -> io::Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", port))?;
    // SAFETY: the helper is single-threaded here; the child only uses
    // async-signal-safe calls before it starts its own threads.
    match unsafe { libc::fork() } {
        -1 => Err(io::Error::last_os_error()),
        0 => {
            // SAFETY: plain prctl/getppid calls.
            unsafe {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                if libc::getppid() == 1 {
                    libc::_exit(0);
                }
            }
            for conn in listener.incoming().flatten() {
                let sock = sock.to_path_buf();
                std::thread::spawn(move || {
                    if let Ok(upstream) = UnixStream::connect(&sock) {
                        relay(conn, upstream);
                    }
                });
            }
            std::process::exit(0)
        }
        _ => Ok(()),
    }
}

fn relay(client: TcpStream, upstream: UnixStream) {
    fn pump(mut from: impl Read, mut to: impl Write) -> io::Result<u64> {
        io::copy(&mut from, &mut to)
    }
    let (Ok(c2), Ok(u2)) = (client.try_clone(), upstream.try_clone()) else {
        return;
    };
    let up = std::thread::spawn(move || {
        let _ = pump(&c2, &u2);
        let _ = u2.shutdown(Shutdown::Write);
    });
    let _ = pump(&upstream, &client);
    let _ = client.shutdown(Shutdown::Write);
    let _ = up.join();
}

/// The subtrees of `root` that stay readable when `deny` is taken out.
/// Landlock only grants, so a denied path is carved out by granting its
/// siblings instead (recursively along the path).
fn carve(root: &Path, deny: &[PathBuf]) -> Vec<PathBuf> {
    if deny.iter().any(|d| d == root) {
        return Vec::new();
    }
    if !deny.iter().any(|d| d.starts_with(root)) {
        return vec![root.to_path_buf()];
    }
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in entries.flatten() {
        let path = e.path();
        if e.file_type().is_ok_and(|t| t.is_symlink()) {
            // A symlink rule would apply to its target; skip it.
            continue;
        }
        out.extend(carve(&path, deny));
    }
    out
}

/// Every ELF interpreter on the system, which `execve` opens for execution
/// as well.
fn loaders() -> Vec<PathBuf> {
    let mut out = Vec::new();
    for dir in ["/lib", "/lib64", "/usr/lib", "/usr/lib64"] {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with("ld-linux") || name.starts_with("ld-musl") || name == "ld.so" {
                out.push(resolve(&e.path()));
            }
        }
    }
    out
}

fn apply_landlock(spec: &Spec) -> Result<(), String> {
    let p = &spec.profile;
    let abi = ABI_TARGET;
    let exec_any = matches!(p.exec, Exec::Any);
    let read_access: BitFlags<AccessFs> = if exec_any {
        AccessFs::from_read(abi)
    } else {
        AccessFs::ReadFile | AccessFs::ReadDir
    };
    let write_access: BitFlags<AccessFs> = if exec_any {
        AccessFs::from_all(abi)
    } else {
        AccessFs::from_all(abi) & !AccessFs::Execute
    };

    let mut rules: Vec<(PathBuf, BitFlags<AccessFs>)> = Vec::new();
    let mut grant = |roots: Vec<PathBuf>, access: BitFlags<AccessFs>| {
        for r in roots {
            rules.push((r, access));
        }
    };
    let readable_roots: Vec<PathBuf> = if p.read_everything {
        vec![PathBuf::from("/")]
    } else {
        SYSTEM_DIRS
            .iter()
            .chain(PSEUDO_DIRS)
            .map(PathBuf::from)
            .chain(p.read.iter().cloned())
            .collect()
    };
    // Also inside bubblewrap: its empty mounts only hide denied paths that
    // existed at start. Landlock grants reads per existing inode, so a
    // carved-out path created later (a new `~/.aws`) is covered by no rule
    // and stays unreadable.
    let deny: &[PathBuf] = &p.deny_read;
    for root in readable_roots {
        grant(carve(&root, deny), read_access);
    }
    if p.read_everything {
        // Listing directories on the way to a carved-out path.
        grant(vec![PathBuf::from("/")], AccessFs::ReadDir.into());
    }
    for w in &p.write {
        grant(carve(w, &p.deny_read), write_access);
    }
    if spec.bwrap {
        // The private /tmp of the namespace, which may also hold bound
        // directories: carved like every other grant.
        grant(carve(Path::new("/tmp"), &p.deny_read), write_access);
    }
    if let Exec::Only(programs) = &p.exec {
        grant(programs.clone(), AccessFs::Execute | AccessFs::ReadFile);
        grant(loaders(), AccessFs::Execute | AccessFs::ReadFile);
    }
    for dev in ["/dev/null", "/dev/zero", "/dev/tty", "/dev/full"] {
        grant(
            vec![PathBuf::from(dev)],
            AccessFs::ReadFile | AccessFs::WriteFile,
        );
    }

    let handle_net: BitFlags<AccessNet> = match p.network {
        Network::None | Network::Loopback(_) => AccessNet::BindTcp | AccessNet::ConnectTcp,
        Network::Outbound => AccessNet::BindTcp.into(),
        Network::Any => BitFlags::empty(),
    };
    let mut ruleset = Ruleset::default()
        .set_compatibility(CompatLevel::BestEffort)
        .handle_access(AccessFs::from_all(abi))
        .map_err(|e| e.to_string())?;
    if !handle_net.is_empty() {
        ruleset = ruleset
            .handle_access(handle_net)
            .map_err(|e| e.to_string())?;
    }
    ruleset = ruleset
        .scope(Scope::AbstractUnixSocket | Scope::Signal)
        .map_err(|e| e.to_string())?;
    let mut created = ruleset.create().map_err(|e| e.to_string())?;
    for (path, access) in rules {
        let Ok(fd) = PathFd::new(&path) else {
            continue;
        };
        let access = if path.is_dir() {
            access
        } else {
            access & AccessFs::from_file(abi)
        };
        if access.is_empty() {
            continue;
        }
        created = created
            .add_rule(PathBeneath::new(fd, access))
            .map_err(|e| format!("{}: {e}", path.display()))?;
    }
    if let Network::Loopback(port) = p.network {
        created = created
            .add_rule(NetPort::new(port, AccessNet::ConnectTcp))
            .map_err(|e| e.to_string())?;
    }
    let status = created.restrict_self().map_err(|e| e.to_string())?;
    if status.ruleset == RulesetStatus::NotEnforced {
        return Err("the kernel did not enforce the ruleset".into());
    }
    Ok(())
}

fn apply_seccomp(spec: &Spec) -> Result<(), String> {
    use std::collections::BTreeMap;

    use seccompiler::{
        BpfProgram, SeccompAction, SeccompCmpArgLen as Len, SeccompCmpOp as Op,
        SeccompCondition as Cond, SeccompFilter, SeccompRule, TargetArch,
    };

    let arch = TargetArch::try_from(std::env::consts::ARCH).map_err(|e| e.to_string())?;
    let cond = |arg, len, op, val| Cond::new(arg, len, op, val).map_err(|e| e.to_string());
    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();

    // Kernel attack surface and ways around the other layers that no build
    // tool or the worker needs.
    for nr in [
        libc::SYS_ptrace,
        libc::SYS_process_vm_readv,
        libc::SYS_process_vm_writev,
        libc::SYS_mount,
        libc::SYS_umount2,
        libc::SYS_pivot_root,
        libc::SYS_move_mount,
        libc::SYS_open_tree,
        libc::SYS_fsopen,
        libc::SYS_fsconfig,
        libc::SYS_fsmount,
        libc::SYS_fspick,
        libc::SYS_unshare,
        libc::SYS_setns,
        libc::SYS_bpf,
        libc::SYS_keyctl,
        libc::SYS_add_key,
        libc::SYS_request_key,
        libc::SYS_userfaultfd,
        libc::SYS_perf_event_open,
        libc::SYS_io_uring_setup,
        libc::SYS_io_uring_enter,
        libc::SYS_io_uring_register,
        libc::SYS_kexec_load,
        libc::SYS_init_module,
        libc::SYS_finit_module,
        libc::SYS_delete_module,
        libc::SYS_swapon,
        libc::SYS_swapoff,
        libc::SYS_reboot,
        libc::SYS_acct,
    ] {
        rules.insert(nr, Vec::new());
    }

    // New namespaces through clone flags.
    let ns_flags = [
        libc::CLONE_NEWUSER,
        libc::CLONE_NEWNS,
        libc::CLONE_NEWNET,
        libc::CLONE_NEWPID,
        libc::CLONE_NEWIPC,
        libc::CLONE_NEWUTS,
        libc::CLONE_NEWCGROUP,
    ];
    let mut clone_rules = Vec::new();
    for flag in ns_flags {
        let flag = flag as u64;
        clone_rules.push(
            SeccompRule::new(vec![cond(0, Len::Qword, Op::MaskedEq(flag), flag)?])
                .map_err(|e| e.to_string())?,
        );
    }
    rules.insert(libc::SYS_clone, clone_rules);

    // Sockets: IP only, and only as the profile allows. Every other family
    // is refused, whether listed here or added to the kernel later: Unix
    // sockets (host daemons such as docker.sock are reachable through
    // them), netlink, packet, and families that no network namespace
    // confines, such as AF_VSOCK (the hypervisor) and AF_ALG (kernel
    // crypto). No confined process needs them; the broker socket is
    // inherited, not created.
    let mut socket_rules = vec![
        SeccompRule::new(vec![
            cond(0, Len::Dword, Op::Ne, libc::AF_INET as u64)?,
            cond(0, Len::Dword, Op::Ne, libc::AF_INET6 as u64)?,
        ])
        .map_err(|e| e.to_string())?,
    ];
    let no_ip = p_no_ip(spec);
    for domain in [libc::AF_INET, libc::AF_INET6] {
        if no_ip {
            socket_rules.push(
                SeccompRule::new(vec![cond(0, Len::Dword, Op::Eq, domain as u64)?])
                    .map_err(|e| e.to_string())?,
            );
        } else if !matches!(spec.profile.network, Network::Any | Network::Outbound) {
            // Loopback: TCP to the proxy only. Landlock filters TCP ports and
            // nothing else, so every other socket type (UDP, raw, SEQPACKET,
            // ...) and every other stream protocol (SCTP, MPTCP) is refused.
            // The low four bits of the type argument are the type; the rest
            // are flags such as SOCK_CLOEXEC.
            for ty in (0..=0xf).filter(|t| *t != libc::SOCK_STREAM) {
                socket_rules.push(
                    SeccompRule::new(vec![
                        cond(0, Len::Dword, Op::Eq, domain as u64)?,
                        cond(1, Len::Dword, Op::MaskedEq(0xf), ty as u64)?,
                    ])
                    .map_err(|e| e.to_string())?,
                );
            }
            socket_rules.push(
                SeccompRule::new(vec![
                    cond(0, Len::Dword, Op::Eq, domain as u64)?,
                    cond(2, Len::Dword, Op::Ne, 0)?,
                    cond(2, Len::Dword, Op::Ne, libc::IPPROTO_TCP as u64)?,
                ])
                .map_err(|e| e.to_string())?,
            );
        }
    }
    rules.insert(libc::SYS_socket, socket_rules);

    let deny: BpfProgram = SeccompFilter::new(
        rules,
        SeccompAction::Allow,
        SeccompAction::Errno(libc::EPERM as u32),
        arch,
    )
    .and_then(TryInto::try_into)
    .map_err(|e| e.to_string())?;
    // clone3 passes its flags in memory, out of seccomp's reach. ENOSYS makes
    // the C library fall back to clone, where the flags are checked above.
    let clone3: BpfProgram = SeccompFilter::new(
        [(libc::SYS_clone3, Vec::new())].into(),
        SeccompAction::Allow,
        SeccompAction::Errno(libc::ENOSYS as u32),
        arch,
    )
    .and_then(TryInto::try_into)
    .map_err(|e| e.to_string())?;
    // SAFETY: prctl with constant arguments.
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        return Err(io::Error::last_os_error().to_string());
    }
    #[cfg(target_arch = "x86_64")]
    {
        seccompiler::apply_filter(&x32_filter()).map_err(|e| e.to_string())?;
    }
    seccompiler::apply_filter(&deny).map_err(|e| e.to_string())?;
    seccompiler::apply_filter(&clone3).map_err(|e| e.to_string())?;
    Ok(())
}

/// Refuses the x32 ABI. On kernels built with `CONFIG_X86_X32_ABI` its
/// system calls report `AUDIT_ARCH_X86_64`, so they pass the architecture
/// check every seccompiler filter starts with, and carry
/// `__X32_SYSCALL_BIT` in their number, so they match none of its rules
/// (keyed by exact number) and would get the default action, Allow: every
/// rule above could be sidestepped. seccompiler cannot match a range of
/// numbers, hence this hand-written filter; stacked filters all apply, the
/// strictest result wins. Other architectures (i386, `int 0x80`) are
/// already killed by the architecture check.
#[cfg(target_arch = "x86_64")]
fn x32_filter() -> seccompiler::BpfProgram {
    use seccompiler::sock_filter;
    const LD_W_ABS: u16 = 0x20; // BPF_LD | BPF_W | BPF_ABS
    const JEQ_K: u16 = 0x15; // BPF_JMP | BPF_JEQ | BPF_K
    const JGE_K: u16 = 0x35; // BPF_JMP | BPF_JGE | BPF_K
    const RET_K: u16 = 0x06; // BPF_RET | BPF_K
    const AUDIT_ARCH_X86_64: u32 = 62 | 0x8000_0000 | 0x4000_0000;
    const X32_SYSCALL_BIT: u32 = 0x4000_0000;
    let op = |code, k, jt, jf| sock_filter { code, jt, jf, k };
    vec![
        // seccomp_data.arch
        op(LD_W_ABS, 4, 0, 0),
        op(JEQ_K, AUDIT_ARCH_X86_64, 1, 0),
        op(RET_K, libc::SECCOMP_RET_KILL_PROCESS, 0, 0),
        // seccomp_data.nr
        op(LD_W_ABS, 0, 0, 0),
        op(JGE_K, X32_SYSCALL_BIT, 0, 1),
        // What a kernel without x32 answers.
        op(RET_K, libc::SECCOMP_RET_ERRNO | libc::ENOSYS as u32, 0, 0),
        op(RET_K, libc::SECCOMP_RET_ALLOW, 0, 0),
    ]
}

/// No IP sockets at all: no network, and no private namespace that would
/// make them harmless.
fn p_no_ip(spec: &Spec) -> bool {
    spec.profile.network == Network::None && !spec.bwrap
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn x32_filter_is_well_formed() {
        let f = x32_filter();
        assert_eq!(f.len(), 7);
        // Both jumps land inside the program.
        for (i, ins) in f.iter().enumerate() {
            if ins.code & 0x07 == 0x05 {
                assert!(i + 1 + (ins.jt.max(ins.jf) as usize) < f.len(), "{i}");
            }
        }
        assert_eq!(f[5].k, libc::SECCOMP_RET_ERRNO | libc::ENOSYS as u32);
    }

    #[test]
    fn carve_grants_siblings_of_denied_paths() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        for d in ["a/secret", "a/open", "b"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        let mut got = carve(&root, &[root.join("a/secret")]);
        got.sort();
        assert_eq!(got, vec![root.join("a/open"), root.join("b")]);
        assert_eq!(carve(&root, &[]), vec![root.clone()]);
        assert!(carve(&root, std::slice::from_ref(&root)).is_empty());
    }
}
