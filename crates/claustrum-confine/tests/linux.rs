//! Linux backend, end to end: real processes under bubblewrap + Landlock +
//! seccomp, or Landlock + seccomp only when `CLAUSTRUM_NO_BWRAP` is set.

#![cfg(target_os = "linux")]

use std::{
    io::{Read, Write},
    os::unix::net::UnixListener,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Once,
};

use claustrum_confine::{Backend, Exec, Network, Profile, backend, command};

fn setup() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // SAFETY: set once before any test spawns a process.
        unsafe {
            std::env::set_var(
                "CLAUSTRUM_CONFINE_HELPER",
                env!("CARGO_BIN_EXE_claustrum-confine-helper"),
            );
        }
    });
}

fn run(profile: &Profile, script: &str) -> (bool, String) {
    setup();
    let mut p = profile.clone();
    p.exec = Exec::Any;
    let out = command(&p, Path::new("/bin/sh"))
        .unwrap()
        .arg("-c")
        .arg(script)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

fn workspace() -> (tempfile::TempDir, PathBuf) {
    // Not under /tmp: bubblewrap puts a private tmpfs there.
    // Cargo sets CARGO_TARGET_TMPDIR at build time only.
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let ws = dir.path().canonicalize().unwrap();
    std::fs::write(ws.join("claustrum.toml"), "# original\n").unwrap();
    (dir, ws)
}

fn profile(ws: &Path) -> Profile {
    let mut p = Profile::new("test", "/bin/sh");
    p.read_everything = true;
    p.write = vec![ws.to_path_buf()];
    p.deny_write = vec![ws.join("claustrum.toml")];
    p
}

#[test]
fn reports_the_backend() {
    setup();
    let b = backend().unwrap();
    eprintln!("backend: {b}");
    if std::env::var_os("CLAUSTRUM_NO_BWRAP").is_some() {
        assert_eq!(b, Backend::Landlock);
    }
}

#[test]
fn writes_stay_inside_the_writable_trees() {
    let (_d, ws) = workspace();
    let (_o, outside) = workspace();
    let p = profile(&ws);
    let (ok, out) = run(
        &p,
        &format!("cd {} && echo x > inside && cat inside", ws.display()),
    );
    assert!(ok, "{out}");
    let (ok, _) = run(&p, &format!("echo x > {}/escape", outside.display()));
    assert!(!ok);
    assert!(!outside.join("escape").exists());
}

#[test]
fn protected_file_cannot_be_changed_under_bubblewrap() {
    setup();
    if backend().unwrap() != Backend::Bubblewrap {
        eprintln!("skipped: Landlock alone cannot protect a file in a writable directory");
        return;
    }
    let (_d, ws) = workspace();
    let p = profile(&ws);
    for attempt in [
        "echo x >> claustrum.toml",
        "echo x > claustrum.toml",
        "rm claustrum.toml",
        "mv claustrum.toml moved",
        "ln claustrum.toml hard && echo x >> hard",
    ] {
        let (ok, out) = run(&p, &format!("cd {} && {attempt}", ws.display()));
        assert!(!ok, "`{attempt}` succeeded: {out}");
    }
    assert_eq!(
        std::fs::read_to_string(ws.join("claustrum.toml")).unwrap(),
        "# original\n"
    );
    let (ok, out) = run(
        &p,
        &format!("cd {} && mkdir d && echo y > d/f && rm -r d", ws.display()),
    );
    assert!(ok, "{out}");
}

#[test]
fn missing_protected_file_cannot_be_created_under_bubblewrap() {
    setup();
    if backend().unwrap() != Backend::Bubblewrap {
        eprintln!("skipped: Landlock alone cannot protect a file in a writable directory");
        return;
    }
    let (_d, ws) = workspace();
    let (_o, outside) = workspace();
    std::fs::create_dir_all(ws.join(".claude/skills")).unwrap();
    std::fs::write(ws.join(".claude/notes.md"), "n\n").unwrap();
    std::fs::create_dir(ws.join("sub")).unwrap();
    std::os::unix::fs::symlink(&outside, ws.join(".claude/link")).unwrap();
    let mut p = profile(&ws);
    p.deny_write
        .extend([ws.join(".claude/settings.json"), ws.join("sub/dir/file")]);
    for attempt in [
        "echo {} > .claude/settings.json",
        "echo x > .claude/other",
        "mkdir .claude/settings.json",
        "mv .claude .claude-moved",
        "rm -r .claude/skills",
        "mkdir -p sub/dir && echo x > sub/dir/file",
        "echo x > .claude/link/through-link",
    ] {
        let (ok, out) = run(&p, &format!("cd {} && {attempt}", ws.display()));
        assert!(!ok, "`{attempt}` succeeded: {out}");
    }
    assert!(!ws.join(".claude/settings.json").exists());
    assert!(!ws.join("sub/dir").exists());
    assert!(!outside.join("through-link").exists());
    // Existing entries of the guarded directory stay writable; a link among
    // them is not bound (the bind would follow it out of the workspace).
    let (ok, out) = run(
        &p,
        &format!(
            "cd {} && echo s > .claude/skills/s.md && echo m >> .claude/notes.md",
            ws.display()
        ),
    );
    assert!(ok, "{out}");
    assert_eq!(
        std::fs::read_to_string(ws.join(".claude/notes.md")).unwrap(),
        "n\nm\n"
    );
}

#[test]
fn missing_protected_file_in_a_writable_root_is_refused() {
    setup();
    if backend().unwrap() != Backend::Bubblewrap {
        return;
    }
    let (_d, ws) = workspace();
    let mut p = profile(&ws);
    p.deny_write.push(ws.join("new.toml"));
    p.exec = Exec::Any;
    let err = command(&p, Path::new("/bin/sh")).unwrap_err();
    assert!(err.to_string().contains("new.toml"), "{err}");
}

#[test]
fn denied_reads_and_network() {
    let (_d, ws) = workspace();
    let (_s, secret) = workspace();
    std::fs::write(secret.join("key"), "k").unwrap();
    let mut p = profile(&ws);
    p.deny_read = vec![secret.clone()];
    let (ok, out) = run(&p, &format!("cat {}/key", secret.display()));
    assert!(!ok, "{out}");
    // A readable neighbour of the denied directory stays readable.
    let (ok, out) = run(&p, &format!("cat {}/claustrum.toml", ws.display()));
    assert!(ok, "{out}");
    let (ok, out) = run(&p, "curl -sS -m 3 -o /dev/null https://1.1.1.1");
    assert!(!ok, "network should be denied: {out}");
}

#[test]
fn escapes_through_the_kernel_are_refused() {
    let (_d, ws) = workspace();
    let p = profile(&ws);
    for attempt in [
        // New namespaces (seccomp).
        "unshare -r true",
        // Host daemons over Unix sockets (seccomp).
        "python3 -c 'import socket; socket.socket(socket.AF_UNIX)' 2>/dev/null || \
         perl -e 'use Socket; socket(S, PF_UNIX, SOCK_STREAM, 0) or exit 1'",
    ] {
        let (ok, out) = run(&p, attempt);
        assert!(!ok, "`{attempt}` succeeded: {out}");
    }
}

#[test]
fn exec_is_limited_to_the_listed_programs() {
    setup();
    let p = Profile::new("test", "/bin/sh");
    let out = command(&p, Path::new("/bin/sh"))
        .unwrap()
        .arg("-c")
        .arg("/bin/echo hi")
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = command(&p, Path::new("/bin/sh"))
        .unwrap()
        .arg("-c")
        .arg("echo builtin")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn loopback_reaches_only_the_proxy() {
    setup();
    let (_d, ws) = workspace();
    let sock = ws.join("proxy.sock");
    let listener = UnixListener::bind(&sock).unwrap();
    std::thread::spawn(move || {
        for mut conn in listener.incoming().flatten() {
            let mut buf = [0u8; 1024];
            let _ = conn.read(&mut buf);
            let _ = conn.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\npong");
        }
    });
    // A TCP listener on the host stands in for the proxy in Landlock-only
    // mode, where the program connects to the host loopback directly.
    let tcp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = tcp.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for mut conn in tcp.incoming().flatten() {
            let mut buf = [0u8; 1024];
            let _ = conn.read(&mut buf);
            let _ = conn.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\npong");
        }
    });
    let mut p = profile(&ws);
    p.network = Network::Loopback(port);
    p.proxy_socket = Some(sock);
    let (ok, out) = run(&p, &format!("curl -sS -m 5 http://127.0.0.1:{port}/"));
    assert!(ok && out.contains("pong"), "{out}");
    let (ok, out) = run(&p, "curl -sS -m 3 -o /dev/null https://1.1.1.1");
    assert!(!ok, "direct network should be denied: {out}");
}

#[test]
fn loopback_refuses_other_socket_types() {
    let (_d, ws) = workspace();
    let tcp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let mut p = profile(&ws);
    p.network = Network::Loopback(tcp.local_addr().unwrap().port());
    // Prints `<name> <errno>` for each attempt, 0 when the socket was created.
    let script = r#"python3 - <<'PY'
import socket
tries = [
    ("tcp", socket.SOCK_STREAM, 0),
    ("tcp-explicit", socket.SOCK_STREAM, socket.IPPROTO_TCP),
    ("udp", socket.SOCK_DGRAM, 0),
    ("raw", socket.SOCK_RAW, socket.IPPROTO_ICMP),
    ("seqpacket", socket.SOCK_SEQPACKET, 0),
    ("sctp-stream", socket.SOCK_STREAM, 132),
    ("mptcp", socket.SOCK_STREAM, 262),
    ("udp-cloexec", socket.SOCK_DGRAM | socket.SOCK_CLOEXEC, 0),
]
for name, ty, proto in tries:
    for family in (socket.AF_INET, socket.AF_INET6):
        try:
            socket.socket(family, ty, proto).close()
            print(name, family, 0)
        except OSError as e:
            print(name, family, e.errno)
PY"#;
    let (ok, out) = run(&p, script);
    assert!(ok, "{out}");
    for line in out.lines().filter(|l| !l.trim().is_empty()) {
        let mut parts = line.split_whitespace();
        let (Some(name), Some(_family), Some(errno)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let errno: i32 = errno.parse().unwrap_or(-1);
        if name.starts_with("tcp") {
            assert_eq!(errno, 0, "{line}\n{out}");
        } else {
            assert_eq!(
                errno,
                libc::EPERM,
                "{name} was not refused by seccomp: {line}\n{out}"
            );
        }
    }
}

/// x32 system calls (`nr | 0x40000000` with the x86_64 audit arch) match
/// no seccompiler rule; they must not get around the filter.
#[cfg(target_arch = "x86_64")]
#[test]
fn x32_system_calls_are_refused() {
    let (_d, ws) = workspace();
    let p = profile(&ws);
    // socket(AF_UNIX) and unshare(CLONE_NEWUSER), both refused natively,
    // through the x32 numbers (41 and 272 with the x32 bit).
    let script = r#"python3 - <<'PY'
import ctypes
libc = ctypes.CDLL(None, use_errno=True)
for name, nr, args in (("socket", 41, (1, 1, 0)), ("unshare", 272, (0x10000000,))):
    r = libc.syscall(0x40000000 | nr, *args)
    print(name, 0 if r >= 0 else ctypes.get_errno())
PY"#;
    let (ok, out) = run(&p, script);
    assert!(ok, "{out}");
    for line in out.lines().filter(|l| !l.trim().is_empty()) {
        let errno: i32 = line
            .split_whitespace()
            .nth(1)
            .and_then(|e| e.parse().ok())
            .unwrap_or(-1);
        assert_eq!(errno, libc::ENOSYS, "{line}\n{out}");
    }
}

/// Only IP sockets, in every network mode: families that no namespace
/// confines (vsock, kernel crypto) are refused like Unix sockets.
#[test]
fn only_ip_socket_families_are_allowed() {
    let (_d, ws) = workspace();
    let script = r#"python3 - <<'PY'
import socket
for name, family, ty in (
    ("unix", socket.AF_UNIX, socket.SOCK_STREAM),
    ("netlink", socket.AF_NETLINK, socket.SOCK_RAW),
    ("alg", 38, socket.SOCK_SEQPACKET),
    ("vsock", 40, socket.SOCK_STREAM),
    ("can", 29, socket.SOCK_RAW),
):
    try:
        socket.socket(family, ty, 0).close()
        print(name, 0)
    except OSError as e:
        print(name, e.errno)
PY"#;
    for net in [Network::None, Network::Outbound, Network::Any] {
        let mut p = profile(&ws);
        p.network = net;
        let (ok, out) = run(&p, script);
        assert!(ok, "{out}");
        for line in out.lines().filter(|l| !l.trim().is_empty()) {
            let errno: i32 = line
                .split_whitespace()
                .nth(1)
                .and_then(|e| e.parse().ok())
                .unwrap_or(-1);
            assert_eq!(errno, libc::EPERM, "{net:?}: {line}\n{out}");
        }
    }
}

/// Without bubblewrap, only Landlock's TCP rules (ABI 4) keep a loopback
/// profile on its port; an older kernel must refuse it, not run it with
/// TCP open to everywhere.
#[test]
fn loopback_without_bubblewrap_needs_tcp_rules() {
    setup();
    if backend().unwrap() != Backend::Landlock {
        eprintln!("skipped: bubblewrap provides a network namespace");
        return;
    }
    // SAFETY: with a null attribute and the VERSION flag the call only
    // returns the ABI version.
    let abi = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<libc::c_void>(),
            0usize,
            1u32,
        )
    };
    let (_d, ws) = workspace();
    let mut p = profile(&ws);
    p.network = Network::Loopback(1);
    let result = command(&p, Path::new("/bin/sh"));
    if abi < 4 {
        let err = result.unwrap_err();
        assert!(err.to_string().contains("Landlock ABI"), "{err}");
    } else {
        result.unwrap();
    }
}

#[test]
fn name_service_lookups_work_without_unix_sockets() {
    // glibc's NSS modules for systemd (nss-resolve, nss-systemd) talk over
    // Unix sockets, which seccomp refuses; they must report UNAVAIL so that
    // the lookup falls through to files and DNS.
    let (_d, ws) = workspace();
    for net in [Network::None, Network::Outbound] {
        let mut p = profile(&ws);
        p.network = net;
        let (ok, out) = run(&p, "getent passwd \"$(id -u)\" && getent hosts localhost");
        assert!(ok, "{out}");
    }
}

#[test]
fn denied_paths_created_later_stay_unreadable() {
    setup();
    let (_d, ws) = workspace();
    // Inside the workspace, which bubblewrap binds (its /tmp is private).
    let secret = ws.join(".aws");
    let mut p = profile(&ws);
    p.deny_read = vec![secret.clone()];
    p.exec = Exec::Any;
    // The credential store appears only after the process started.
    let child = command(&p, Path::new("/bin/sh"))
        .unwrap()
        .arg("-c")
        .arg(format!(
            "i=0; while [ ! -e {s}/ready ] && [ $i -lt 100 ]; do sleep 0.05; i=$((i+1)); done; \
             cat {s}/credentials",
            s = secret.display()
        ))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(300));
    std::fs::create_dir(&secret).unwrap();
    std::fs::write(secret.join("credentials"), "KEY").unwrap();
    std::fs::write(secret.join("ready"), "").unwrap();
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("KEY"),
        "read a credential created later: {stdout}"
    );
}
