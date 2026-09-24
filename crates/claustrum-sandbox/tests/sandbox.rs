//! Integration tests against the bundled bash + coreutils packages.
//!
//! Requires `packages/bash.webc` and `packages/coreutils.webc` in the repo
//! root (see README, "Packages"). Tests are skipped when they are missing.

use std::{path::PathBuf, time::Duration};

use claustrum_sandbox::{
    ExecOptions, ExitReason, Policy, Sandbox, SandboxBuilder,
    native::{GrepMode, GrepOptions, ReadOptions},
};

fn packages_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../packages")
}

async fn sandbox(workspace: &std::path::Path) -> Option<Sandbox> {
    sandbox_with(workspace, |b| b).await
}

async fn sandbox_with(
    workspace: &std::path::Path,
    customize: impl FnOnce(SandboxBuilder) -> SandboxBuilder,
) -> Option<Sandbox> {
    let dir = packages_dir();
    if !dir.join("bash.webc").exists() || !dir.join("coreutils.webc").exists() {
        eprintln!("skipping: bundled packages not found in {}", dir.display());
        return None;
    }
    let policy = Policy {
        default_timeout: Some(Duration::from_secs(60)),
        ..Policy::default()
    };
    let mut builder = Sandbox::builder()
        .workspace(workspace)
        .package_named(dir.join("bash.webc"), "wasmer/bash@1.0.25")
        .package_named(dir.join("coreutils.webc"), "wasmer/coreutils@1.0.25")
        .policy(policy);
    // Optional packages: only loaded when present.
    for (file, id) in [
        ("jq.webc", "syrusakbary/jq@0.1.0"),
        ("python.webc", "python/python@3.13.20"),
    ] {
        if dir.join(file).exists() {
            builder = builder.package_named(dir.join(file), id);
        }
    }
    Some(customize(builder).build().await.expect("sandbox builds"))
}

fn has_command(sb: &Sandbox, name: &str) -> bool {
    if sb.commands().iter().any(|c| c == name) {
        true
    } else {
        eprintln!("skipping: `{name}` package not installed");
        false
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn jq_filters_json_without_colours() {
    let ws = tempfile::tempdir().unwrap();
    std::fs::write(ws.path().join("d.json"), r#"{"a":[1,2,3],"b":"x"}"#).unwrap();
    let Some(sb) = sandbox(ws.path()).await else {
        return;
    };
    if !has_command(&sb, "jq") {
        return;
    }
    let out = sb
        .bash(
            "jq -c .a d.json; cat d.json | jq -r .b",
            ExecOptions::default(),
        )
        .await
        .unwrap();
    assert!(out.success(), "stderr: {}", out.stderr_lossy());
    assert_eq!(out.stdout_lossy(), "[1,2,3]\nx\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn python_runs_scripts() {
    let ws = tempfile::tempdir().unwrap();
    std::fs::write(
        ws.path().join("t.py"),
        "import json\nprint(json.dumps({'n': 2**70}))\n",
    )
    .unwrap();
    let Some(sb) = sandbox(ws.path()).await else {
        return;
    };
    if !has_command(&sb, "python") {
        return;
    }
    let out = sb
        .bash(
            "python t.py && python3 -c 'import sys; print(sys.platform)'",
            ExecOptions {
                // First use compiles a 60 MB module.
                timeout: Some(Duration::from_secs(300)),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(out.success(), "stderr: {}", out.stderr_lossy());
    assert_eq!(
        out.stdout_lossy(),
        "{\"n\": 1180591620717411303424}\nwasix\n"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn profile_disables_pagers_and_colours() {
    let ws = tempfile::tempdir().unwrap();
    let Some(sb) = sandbox(ws.path()).await else {
        return;
    };
    let out = sb
        .bash(
            "echo $PAGER $NO_COLOR; type jq | head -1",
            ExecOptions::default(),
        )
        .await
        .unwrap();
    assert!(
        out.stdout_lossy().starts_with("cat 1\n"),
        "{}",
        out.stdout_lossy()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn bash_runs_pipelines_and_sees_workspace() {
    let ws = tempfile::tempdir().unwrap();
    std::fs::write(ws.path().join("a.txt"), "hello\nworld\n").unwrap();
    let Some(sb) = sandbox(ws.path()).await else {
        return;
    };

    let out = sb
        .bash(
            "pwd; cat a.txt | tr a-z A-Z; echo status=$?",
            ExecOptions::default(),
        )
        .await
        .unwrap();
    assert!(out.success(), "stderr: {}", out.stderr_lossy());
    assert_eq!(out.stdout_lossy(), "/workspace\nHELLO\nWORLD\nstatus=0\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn guest_writes_reach_the_host() {
    let ws = tempfile::tempdir().unwrap();
    let Some(sb) = sandbox(ws.path()).await else {
        return;
    };

    let out = sb
        .bash(
            "mkdir -p sub && echo from-guest > sub/f.txt",
            ExecOptions::default(),
        )
        .await
        .unwrap();
    assert!(out.success(), "stderr: {}", out.stderr_lossy());
    assert_eq!(
        std::fs::read_to_string(ws.path().join("sub/f.txt")).unwrap(),
        "from-guest\n"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn host_files_outside_workspace_are_invisible() {
    let ws = tempfile::tempdir().unwrap();
    let Some(sb) = sandbox(ws.path()).await else {
        return;
    };

    let out = sb
        .bash(
            "cat /etc/passwd; ls /Users /home/manuel 2>&1; cat ../../../../etc/hosts",
            ExecOptions::default(),
        )
        .await
        .unwrap();
    assert_ne!(out.exit_code, 0);
    assert!(
        !out.stdout_lossy().contains("root:"),
        "stdout: {}",
        out.stdout_lossy()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn exit_codes_and_stderr_are_reported() {
    let ws = tempfile::tempdir().unwrap();
    let Some(sb) = sandbox(ws.path()).await else {
        return;
    };

    let out = sb
        .bash("echo oops >&2; exit 3", ExecOptions::default())
        .await
        .unwrap();
    assert_eq!(out.exit_code, 3);
    assert_eq!(out.reason, ExitReason::Exited);
    assert_eq!(out.stderr_lossy(), "oops\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn commands_time_out() {
    let ws = tempfile::tempdir().unwrap();
    let Some(sb) = sandbox(ws.path()).await else {
        return;
    };

    let out = sb
        .bash(
            "echo start; sleep 20; echo never",
            ExecOptions {
                timeout: Some(Duration::from_secs(2)),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(out.reason, ExitReason::TimedOut);
    assert_eq!(out.stdout_lossy(), "start\n");
    assert!(out.duration < Duration::from_secs(10));
}

#[tokio::test(flavor = "multi_thread")]
async fn stdin_is_delivered() {
    let ws = tempfile::tempdir().unwrap();
    let Some(sb) = sandbox(ws.path()).await else {
        return;
    };

    let out = sb
        .bash(
            "wc -c",
            ExecOptions {
                stdin: Some(b"12345".to_vec()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(out.success(), "stderr: {}", out.stderr_lossy());
    assert_eq!(out.stdout_lossy().trim(), "5");
}

#[tokio::test(flavor = "multi_thread")]
async fn cwd_persists_between_calls() {
    let ws = tempfile::tempdir().unwrap();
    std::fs::create_dir(ws.path().join("src")).unwrap();
    let Some(sb) = sandbox(ws.path()).await else {
        return;
    };

    sb.set_cwd("src").unwrap();
    let out = sb.bash("pwd", ExecOptions::default()).await.unwrap();
    assert_eq!(out.stdout_lossy(), "/workspace/src\n");
    assert!(sb.set_cwd("nope").is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn native_tools_round_trip() {
    let ws = tempfile::tempdir().unwrap();
    let Some(sb) = sandbox(ws.path()).await else {
        return;
    };

    let w = sb
        .write("src/lib.rs", "fn a() {}\nfn b() {}\nfn c() {}\n")
        .await
        .unwrap();
    assert!(w.created);
    assert_eq!(w.path, "/workspace/src/lib.rs");
    assert!(ws.path().join("src/lib.rs").exists());

    let r = sb
        .read(
            "/workspace/src/lib.rs",
            ReadOptions {
                offset: Some(2),
                limit: Some(1),
            },
        )
        .await
        .unwrap();
    assert_eq!(r.content, "     2\tfn b() {}\n");
    assert_eq!(r.total_lines, 3);
    assert!(r.truncated);

    let e = sb
        .edit("src/lib.rs", "fn b()", "fn bee()", false)
        .await
        .unwrap();
    assert_eq!(e.replacements, 1);
    assert!(e.snippet.contains("fn bee()"));
    assert!(sb.edit("src/lib.rs", "fn ", "fn  ", false).await.is_err());
    assert_eq!(
        sb.edit("src/lib.rs", "fn ", "fn  ", true)
            .await
            .unwrap()
            .replacements,
        3
    );

    sb.write("README.md", "# Title\n").await.unwrap();
    let g = sb.glob("**/*.rs", None).unwrap();
    assert_eq!(g.paths, vec!["/workspace/src/lib.rs"]);
    let g = sb.glob("*.md", Some("/workspace")).unwrap();
    assert_eq!(g.paths, vec!["/workspace/README.md"]);

    let gr = sb
        .grep(
            "fn  (a|c)",
            GrepOptions {
                mode: GrepMode::Content,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(gr.files, vec!["/workspace/src/lib.rs"]);
    assert_eq!(gr.matches.len(), 2);
    assert_eq!(gr.matches[0].line, 1);

    let gr = sb
        .grep(
            "title",
            GrepOptions {
                case_insensitive: true,
                glob: Some("*.md".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(gr.files, vec!["/workspace/README.md"]);

    // Native and guest views agree.
    let out = sb
        .bash("cat src/lib.rs | head -1", ExecOptions::default())
        .await
        .unwrap();
    assert_eq!(out.stdout_lossy(), "fn  a() {}\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn native_tools_reject_paths_outside_mounts() {
    let ws = tempfile::tempdir().unwrap();
    let Some(sb) = sandbox(ws.path()).await else {
        return;
    };
    assert!(
        sb.read("/etc/passwd", ReadOptions::default())
            .await
            .is_err()
    );
    assert!(
        sb.read("../../../etc/passwd", ReadOptions::default())
            .await
            .is_err()
    );
    assert!(sb.write("/bin/evil", "x").await.is_err());
}

/// Runs each attempt in bash and prints `ALLOWED: <cmd>` for every one that
/// succeeded.
const ATTEMPTS_PRELUDE: &str =
    "attempt() { if eval \"$1\" 2>/dev/null; then echo \"ALLOWED: $1\"; fi; }\n";

async fn allowed_attempts(sb: &Sandbox, attempts: &[&str]) -> String {
    let mut script = String::from(ATTEMPTS_PRELUDE);
    for a in attempts {
        script.push_str(&format!("attempt '{a}'\n"));
    }
    let out = sb.bash(&script, ExecOptions::default()).await.unwrap();
    out.stdout_lossy()
}

#[tokio::test(flavor = "multi_thread")]
async fn config_file_is_read_only() {
    let ws = tempfile::tempdir().unwrap();
    let config = ws.path().join("claustrum.toml");
    std::fs::write(&config, "# marker\n").unwrap();
    std::fs::write(ws.path().join("notes.txt"), "notes\n").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("claustrum.toml", ws.path().join("hostlink")).unwrap();
    let Some(sb) = sandbox_with(ws.path(), |b| {
        b.protect(ws.path().join("claustrum.toml"))
            .mount("/alt", ws.path())
    })
    .await
    else {
        return;
    };

    // Reading works everywhere.
    let out = sb
        .bash(
            "cat claustrum.toml && cat /alt/claustrum.toml",
            ExecOptions::default(),
        )
        .await
        .unwrap();
    assert!(out.success(), "stderr: {}", out.stderr_lossy());
    assert_eq!(out.stdout_lossy(), "# marker\n# marker\n");
    let read = sb
        .read("/workspace/claustrum.toml", ReadOptions::default())
        .await
        .unwrap();
    assert!(read.content.contains("marker"));

    let allowed = allowed_attempts(
        &sb,
        &[
            "echo x > claustrum.toml",
            "echo x >> claustrum.toml",
            "cp notes.txt claustrum.toml",
            "cat notes.txt > ./sub/../claustrum.toml",
            "truncate -s 0 claustrum.toml",
            "rm -f claustrum.toml",
            "mv claustrum.toml moved.toml",
            "mv notes.txt claustrum.toml",
            // WASIX keeps a guest-only alias for hard links on host mounts;
            // writing through it still hits the protected file.
            "ln claustrum.toml hard.toml && echo x > hard.toml",
            "ln -s claustrum.toml link.toml && echo x > link.toml",
            "echo x > hostlink",
            "echo x > /alt/claustrum.toml",
            "mv /alt/notes.txt /alt/claustrum.toml",
        ],
    )
    .await;
    assert_eq!(allowed, "", "these attempts were not refused");
    assert_eq!(std::fs::read_to_string(&config).unwrap(), "# marker\n");
    // `mv` falls back to copy + unlink when the rename is refused; the copy is
    // just a read of the protected file, the original stays in place.
    if let Ok(copy) = std::fs::read_to_string(ws.path().join("moved.toml")) {
        assert_eq!(copy, "# marker\n");
    }
    assert!(!ws.path().join("hard.toml").exists());

    // Native tools refuse with a clear message.
    let err = sb
        .write("/workspace/claustrum.toml", "network = \"host\"\n")
        .await
        .unwrap_err();
    assert!(err.to_string().contains("protected"), "{err}");
    let err = sb
        .edit("claustrum.toml", "marker", "changed", false)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("protected"), "{err}");
    assert_eq!(std::fs::read_to_string(&config).unwrap(), "# marker\n");

    // Ordinary files are unaffected.
    let out = sb
        .bash(
            "echo more >> notes.txt && mkdir -p d && mv notes.txt d/",
            ExecOptions::default(),
        )
        .await
        .unwrap();
    assert!(out.success(), "stderr: {}", out.stderr_lossy());
    assert_eq!(
        std::fs::read_to_string(ws.path().join("d/notes.txt")).unwrap(),
        "notes\nmore\n"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_config_file_cannot_be_created() {
    let ws = tempfile::tempdir().unwrap();
    std::fs::write(ws.path().join("notes.txt"), "network = \"host\"\n").unwrap();
    let Some(sb) = sandbox_with(ws.path(), |b| b.protect(ws.path().join("claustrum.toml"))).await
    else {
        return;
    };
    let mut attempts = vec![
        "echo x > claustrum.toml",
        "mkdir claustrum.toml",
        "cp notes.txt claustrum.toml",
        "mv notes.txt claustrum.toml",
    ];
    if cfg!(target_os = "macos") {
        attempts.push("cp notes.txt CLAUSTRUM.TOML");
    }
    let allowed = allowed_attempts(&sb, &attempts).await;
    assert_eq!(allowed, "", "these attempts were not refused");
    let err = sb
        .write("claustrum.toml", "network = \"host\"\n")
        .await
        .unwrap_err();
    assert!(err.to_string().contains("protected"), "{err}");
    let names: Vec<_> = std::fs::read_dir(ws.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["notes.txt"]);
}
