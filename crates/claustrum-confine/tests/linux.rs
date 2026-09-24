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
    let base = std::env::var_os("CARGO_TARGET_TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let dir = tempfile::tempdir_in(base).unwrap();
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
