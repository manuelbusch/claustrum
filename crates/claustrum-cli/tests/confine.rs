//! The confined worker, tested through the real binary: a simulated WASIX
//! escape (raw host calls from the worker process) must hit the OS sandbox.

#![cfg(all(any(target_os = "macos", target_os = "linux"), debug_assertions))]

use std::process::{Command, Stdio};

#[test]
fn worker_escape_is_contained() {
    let ws = tempfile::tempdir().unwrap();
    std::fs::write(ws.path().join("claustrum.toml"), "# original\n").unwrap();
    let home = std::env::var_os("HOME").unwrap();
    let escape = std::path::Path::new(&home).join(".claustrum-escape");
    let _ = std::fs::remove_file(&escape);

    let out = Command::new(env!("CARGO_BIN_EXE_claustrum"))
        .arg("serve")
        .current_dir(ws.path())
        .env("CLAUSTRUM_ESCAPE_PROBE", "1")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    let backend = claustrum_confine::backend().expect("a confinement backend");
    assert!(
        stderr.contains(&format!("confinement: {backend} around")),
        "{stderr}"
    );
    assert!(
        stderr.contains("probe workspace-write: ALLOWED"),
        "{stderr}"
    );
    let mut denied = vec!["home-write", "ssh-read", "exec", "network"];
    // Landlock alone cannot keep a file read-only inside the writable
    // workspace; that needs bubblewrap (or the WASIX layer's own check).
    if backend != claustrum_confine::Backend::Landlock {
        denied.push("config-write");
    }
    for what in denied {
        assert!(
            stderr.contains(&format!("probe {what}: denied")),
            "{what} was not denied:\n{stderr}"
        );
    }
    assert!(!escape.exists());
    assert_eq!(
        std::fs::read_to_string(ws.path().join("claustrum.toml")).unwrap(),
        "# original\n"
    );
}

#[test]
fn confinement_off_runs_in_process() {
    let ws = tempfile::tempdir().unwrap();
    std::fs::write(
        ws.path().join("claustrum.toml"),
        "[sandbox]\nconfinement = \"off\"\n",
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_claustrum"))
        .arg("serve")
        .current_dir(ws.path())
        .env("CLAUSTRUM_ESCAPE_PROBE", "1")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("confinement: OFF"), "{stderr}");
    // No worker, so the probe never runs.
    assert!(!stderr.contains("probe "), "{stderr}");
}
