//! The confined worker, tested through the real binary: a simulated WASIX
//! escape (raw host calls from the worker process) must hit the OS sandbox.

#![cfg(all(any(target_os = "macos", target_os = "linux"), debug_assertions))]

use std::{
    path::Path,
    process::{Command, Stdio},
};

/// `claustrum trust` for the project configuration in `ws`.
fn trust(ws: &Path, state: &Path) {
    let out = Command::new(env!("CARGO_BIN_EXE_claustrum"))
        .arg("trust")
        .current_dir(ws)
        .env("CLAUSTRUM_STATE_DIR", state)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn worker_escape_is_contained() {
    let ws = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let claude = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    std::fs::create_dir(cache.path().join("modules")).unwrap();
    std::fs::create_dir(claude.path().join("plans")).unwrap();
    std::fs::write(claude.path().join("plans/other.md"), "theirs\n").unwrap();
    std::fs::write(ws.path().join("claustrum.toml"), "# original\n").unwrap();
    std::fs::create_dir(ws.path().join(".claude")).unwrap();
    let home = std::env::var_os("HOME").unwrap();
    let escape = std::path::Path::new(&home).join(".claustrum-escape");
    let _ = std::fs::remove_file(&escape);
    trust(ws.path(), state.path());

    let out = Command::new(env!("CARGO_BIN_EXE_claustrum"))
        .arg("serve")
        .current_dir(ws.path())
        .env("CLAUSTRUM_ESCAPE_PROBE", "1")
        .env("CLAUSTRUM_STATE_DIR", state.path())
        .env("CLAUDE_CONFIG_DIR", claude.path())
        .env("CLAUSTRUM_CACHE_DIR", cache.path())
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
    for what in ["workspace-write", "workspace-cache-write"] {
        assert!(
            stderr.contains(&format!("probe {what}: ALLOWED")),
            "{what} was denied:\n{stderr}"
        );
    }
    let mut denied = vec![
        "shared-cache-write",
        "other-log-write",
        "claude-plans-read",
        "claude-plans-write",
        "home-write",
        "ssh-read",
        "exec",
        "network",
    ];
    // Landlock alone cannot keep a file read-only inside the writable
    // workspace; that needs bubblewrap (or the WASIX layer's own check).
    if backend != claustrum_confine::Backend::Landlock {
        denied.extend(["config-write", "claude-settings-write"]);
    }
    for what in denied {
        assert!(
            stderr.contains(&format!("probe {what}: denied")),
            "{what} was not denied:\n{stderr}"
        );
    }
    assert!(!escape.exists());
    assert!(!ws.path().join(".claude/settings.json").exists());
    assert!(!claude.path().join("plans/claustrum-probe.md").exists());
    assert!(!cache.path().join("modules/claustrum-probe").exists());
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
    let state = ws.path().join("state");
    let serve = || {
        Command::new(env!("CARGO_BIN_EXE_claustrum"))
            .arg("serve")
            .current_dir(ws.path())
            .env("CLAUSTRUM_ESCAPE_PROBE", "1")
            .env("CLAUSTRUM_STATE_DIR", &state)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    };
    // A project's configuration is not used before it is trusted.
    let out = serve();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    assert!(stderr.contains("not trusted"), "{stderr}");
    assert!(!stderr.contains("confinement: OFF"), "{stderr}");

    trust(ws.path(), &state);
    let out = serve();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("confinement: OFF"), "{stderr}");
    // No worker, so the probe never runs.
    assert!(!stderr.contains("probe "), "{stderr}");
}
