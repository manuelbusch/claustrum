//! Integration tests against the bundled bash + coreutils packages.
//!
//! Requires `packages/bash.webc` and `packages/coreutils.webc` in the repo
//! root (see README, "Packages"). Tests are skipped when they are missing.

use std::{path::PathBuf, sync::Arc, time::Duration};

use claustrum_sandbox::{
    ExecOptions, ExitReason, Policy, Sandbox, SandboxBuilder,
    native::{GrepMode, GrepOptions, ReadOptions},
    plans::HostPlans,
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

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn host_links_cannot_leave_the_mount() {
    let ws = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let secret = outside.path().join("secret.txt");
    std::fs::write(&secret, "SECRET\n").unwrap();
    std::fs::write(ws.path().join("inside.txt"), "inside\n").unwrap();
    // As a cloned repository or a host action could place them.
    let link = |target: &std::path::Path, name: &str| {
        std::os::unix::fs::symlink(target, ws.path().join(name)).unwrap()
    };
    link(&secret, "abs");
    link(outside.path(), "absdir");
    let depth = ws.path().canonicalize().unwrap().components().count();
    let up = "../".repeat(depth);
    link(
        std::path::Path::new(&format!("{up}{}", secret.canonicalize().unwrap().display())),
        "rel",
    );
    link(std::path::Path::new("inside.txt"), "ok");
    link(&outside.path().join("new.txt"), "dangling");
    let Some(sb) = sandbox(ws.path()).await else {
        return;
    };

    for path in ["abs", "absdir/secret.txt", "rel"] {
        let err = sb.read(path, ReadOptions::default()).await.unwrap_err();
        assert!(
            err.to_string().contains("outside the sandbox"),
            "{path}: {err}"
        );
        let err = sb.edit(path, "SECRET", "x", false).await.unwrap_err();
        assert!(
            err.to_string().contains("outside the sandbox"),
            "{path}: {err}"
        );
    }
    for path in ["absdir/planted.txt", "dangling", "abs"] {
        let err = sb.write(path, "x\n").await.unwrap_err();
        assert!(
            err.to_string().contains("outside the sandbox"),
            "{path}: {err}"
        );
    }
    let err = sb
        .grep(
            "SECRET",
            GrepOptions {
                path: Some("absdir".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("outside the sandbox"), "{err}");
    assert!(sb.glob("*", Some("absdir")).is_err());
    let gr = sb.grep("SECRET", GrepOptions::default()).await.unwrap();
    assert!(gr.files.is_empty(), "{:?}", gr.files);

    let allowed = allowed_attempts(
        &sb,
        &[
            "grep -q SECRET abs",
            "grep -q SECRET rel",
            "grep -q SECRET absdir/secret.txt",
            "echo x > absdir/bash.txt",
            "echo x > dangling",
            "ls absdir/secret.txt",
        ],
    )
    .await;
    assert_eq!(allowed, "", "these attempts were not refused");

    assert_eq!(std::fs::read_to_string(&secret).unwrap(), "SECRET\n");
    let mut names: Vec<_> = std::fs::read_dir(outside.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(names, ["secret.txt"]);

    // Links inside the mount keep working, and outward links can be removed.
    assert!(
        sb.read("ok", ReadOptions::default())
            .await
            .unwrap()
            .content
            .contains("inside")
    );
    sb.write("ok", "changed\n").await.unwrap();
    assert_eq!(
        std::fs::read_to_string(ws.path().join("inside.txt")).unwrap(),
        "changed\n"
    );
    let out = sb
        .bash("rm abs absdir", ExecOptions::default())
        .await
        .unwrap();
    assert!(out.success(), "stderr: {}", out.stderr_lossy());
    assert!(std::fs::symlink_metadata(ws.path().join("abs")).is_err());
    assert!(std::fs::symlink_metadata(ws.path().join("absdir")).is_err());
    assert!(secret.exists());
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
async fn guest_memory_is_limited() {
    let ws = tempfile::tempdir().unwrap();
    let Some(sb) = sandbox_with(ws.path(), |b| {
        b.policy(Policy {
            default_timeout: Some(Duration::from_secs(60)),
            max_memory_bytes: Some(256 * 1024 * 1024),
            ..Policy::default()
        })
    })
    .await
    else {
        return;
    };
    if !has_command(&sb, "python") {
        return;
    }
    let out = sb
        .bash(
            "python -c 'b = bytearray(50 * 1024 * 1024); print(len(b))'",
            ExecOptions::default(),
        )
        .await
        .unwrap();
    assert!(out.success(), "stderr: {}", out.stderr_lossy());
    assert_eq!(out.stdout_lossy().trim(), "52428800");
    let out = sb
        .bash(
            "python -c 'b = bytearray(600 * 1024 * 1024); print(len(b))'",
            ExecOptions::default(),
        )
        .await
        .unwrap();
    assert!(!out.success(), "600 MiB fit into a 256 MiB limit");
    assert!(
        out.stderr_lossy().contains("MemoryError"),
        "stderr: {}",
        out.stderr_lossy()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn native_tools_refuse_huge_files() {
    let ws = tempfile::tempdir().unwrap();
    // Sparse, so the test does not write 65 MiB.
    let big = std::fs::File::create(ws.path().join("big.log")).unwrap();
    big.set_len(65 * 1024 * 1024).unwrap();
    std::fs::write(ws.path().join("small.txt"), "a\nb\n").unwrap();
    let Some(sb) = sandbox(ws.path()).await else {
        return;
    };
    let err = sb
        .read("big.log", ReadOptions::default())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("larger than 64 MiB"), "{err}");
    let err = sb.edit("big.log", "x", "y", false).await.unwrap_err();
    assert!(err.to_string().contains("larger than 64 MiB"), "{err}");
    let out = sb
        .read(
            "small.txt",
            ReadOptions {
                offset: Some(2),
                limit: Some(usize::MAX),
            },
        )
        .await
        .unwrap();
    assert_eq!(out.content, "     2\tb\n");
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
            .mount_writable("/alt", ws.path())
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
async fn read_only_mounts_refuse_writes() {
    let ws = tempfile::tempdir().unwrap();
    let ro = tempfile::tempdir().unwrap();
    let rw = tempfile::tempdir().unwrap();
    for dir in [ro.path(), rw.path()] {
        std::fs::write(dir.join("data.txt"), "data\n").unwrap();
        std::fs::write(dir.join("other.txt"), "other\n").unwrap();
        std::fs::create_dir(dir.join("sub")).unwrap();
    }
    let Some(sb) = sandbox_with(ws.path(), |b| {
        b.mount("/ro", ro.path()).mount_writable("/rw", rw.path())
    })
    .await
    else {
        return;
    };

    // Reading works through Bash and the native tools.
    let out = sb
        .bash("cat /ro/data.txt && ls /ro", ExecOptions::default())
        .await
        .unwrap();
    assert!(out.success(), "stderr: {}", out.stderr_lossy());
    assert_eq!(out.stdout_lossy(), "data\ndata.txt\nother.txt\nsub\n");
    let read = sb
        .read("/ro/data.txt", ReadOptions::default())
        .await
        .unwrap();
    assert!(read.content.contains("data"));
    assert_eq!(
        sb.glob("d*.txt", Some("/ro")).unwrap().paths,
        vec!["/ro/data.txt"]
    );
    let gr = sb
        .grep(
            "data",
            GrepOptions {
                path: Some("/ro".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(gr.files, vec!["/ro/data.txt"]);

    let attempts = |root: &str| {
        [
            "echo x > ROOT/data.txt",
            "echo x >> ROOT/data.txt",
            "echo x > ROOT/new.txt",
            "touch ROOT/touched",
            "truncate -s 0 ROOT/data.txt",
            "mkdir ROOT/newdir",
            "rmdir ROOT/sub",
            "ln -s data.txt ROOT/link",
            "mv ROOT/data.txt ROOT/moved.txt",
            "rm ROOT/other.txt",
        ]
        .map(|a| a.replace("ROOT", root))
    };
    let ro_attempts = attempts("/ro");
    let ro_refs: Vec<&str> = ro_attempts.iter().map(String::as_str).collect();
    let allowed = allowed_attempts(&sb, &ro_refs).await;
    assert_eq!(allowed, "", "these attempts were not refused");
    // Copying out of a read-only mount is just a read.
    let out = sb
        .bash("cp /ro/data.txt copy.txt", ExecOptions::default())
        .await
        .unwrap();
    assert!(out.success(), "stderr: {}", out.stderr_lossy());

    let err = sb.write("/ro/data.txt", "changed\n").await.unwrap_err();
    assert!(err.to_string().contains("read-only"), "{err}");
    let err = sb
        .edit("/ro/data.txt", "data", "changed", false)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("read-only"), "{err}");

    assert_eq!(
        std::fs::read_to_string(ro.path().join("data.txt")).unwrap(),
        "data\n"
    );
    let mut names: Vec<_> = std::fs::read_dir(ro.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(names, ["data.txt", "other.txt", "sub"]);

    // The same operations succeed on a writable mount.
    let rw_attempts = attempts("/rw");
    let rw_refs: Vec<&str> = rw_attempts.iter().map(String::as_str).collect();
    let allowed = allowed_attempts(&sb, &rw_refs).await;
    // `rmdir` on extra mounts fails in WASIX with ENOENT independently of
    // the read-only wrapper, so it is not expected to succeed here.
    for a in rw_attempts.iter().filter(|a| !a.starts_with("rmdir")) {
        assert!(allowed.contains(&format!("ALLOWED: {a}\n")), "refused: {a}");
    }
    sb.write("/rw/written.txt", "w\n").await.unwrap();
    assert_eq!(
        std::fs::read_to_string(rw.path().join("written.txt")).unwrap(),
        "w\n"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn plans_are_written_outside_the_guest() {
    let ws = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join("plans");
    let store = Arc::new(HostPlans::new(&dir, home.path().join("ws.list")));
    let guest = dir.to_str().unwrap().to_owned();
    let Some(sb) = sandbox_with(ws.path(), |b| b.plans(&guest, store)).await else {
        return;
    };
    assert_eq!(sb.plan_dir(), Some(guest.as_str()));

    let out = sb
        .write_plan(&format!("{guest}/bubbly-plan.md"), "# Plan\none\n")
        .await
        .unwrap();
    assert!(out.created);
    sb.edit_plan(&format!("{guest}/bubbly-plan.md"), "one", "two", false)
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.join("bubbly-plan.md")).unwrap(),
        "# Plan\ntwo\n"
    );

    for bad in [
        "/workspace/plan.md".to_owned(),
        format!("{guest}/../escape.md"),
        format!("{guest}/sub/plan.md"),
        format!("{guest}/plan.txt"),
        "plan.md".to_owned(),
    ] {
        let err = sb.write_plan(&bad, "x").await.unwrap_err();
        assert!(err.to_string().contains("plan file path"), "{bad}: {err}");
    }
    assert!(!ws.path().join("plan.md").exists());
    assert!(!home.path().join("escape.md").exists());

    // The plan directory is not part of the guest file system.
    assert!(
        sb.read(&format!("{guest}/bubbly-plan.md"), ReadOptions::default())
            .await
            .is_err()
    );
    assert!(sb.write(&format!("{guest}/other.md"), "x").await.is_err());
    let out = sb
        .bash(
            &format!("cat '{guest}/bubbly-plan.md'"),
            ExecOptions::default(),
        )
        .await
        .unwrap();
    assert!(!out.success());
}

#[tokio::test(flavor = "multi_thread")]
async fn claude_settings_cannot_be_planted() {
    let ws = tempfile::tempdir().unwrap();
    let settings = ws.path().join(".claude/settings.json");
    let local = ws.path().join(".claude/settings.local.json");
    std::fs::write(ws.path().join("hooks.json"), "{\"hooks\": {}}\n").unwrap();
    let Some(sb) = sandbox_with(ws.path(), |b| b.protect(&settings).protect(&local)).await else {
        return;
    };
    let allowed = allowed_attempts(
        &sb,
        &[
            "mkdir -p .claude && cp hooks.json .claude/settings.json",
            "cp hooks.json .claude/settings.local.json",
            "rm -rf .claude",
            "mv .claude gone",
            "ln -s fake .claude.new && mv .claude.new .claude",
        ],
    )
    .await;
    assert_eq!(allowed, "", "these attempts were not refused");
    let err = sb
        .write(".claude/settings.json", "{\"hooks\": {}}\n")
        .await
        .unwrap_err();
    assert!(err.to_string().contains("protected"), "{err}");
    assert!(!settings.exists() && !local.exists());
    let meta = std::fs::symlink_metadata(ws.path().join(".claude")).unwrap();
    assert!(meta.is_dir(), ".claude was replaced");

    // Other files in .claude stay writable.
    sb.write(".claude/agents/reviewer.md", "---\nname: reviewer\n---\n")
        .await
        .unwrap();
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn protected_names_hold_behind_host_links() {
    let ws = tempfile::tempdir().unwrap();
    let settings = ws.path().join(".claude/settings.json");
    std::fs::create_dir(ws.path().join("fake")).unwrap();
    let Some(sb) = sandbox_with(ws.path(), |b| b.protect(&settings)).await else {
        return;
    };
    // A host program redirects `.claude` after the sandbox started.
    std::os::unix::fs::symlink("fake", ws.path().join(".claude")).unwrap();
    let err = sb
        .write(".claude/settings.json", "{\"hooks\": {}}\n")
        .await
        .unwrap_err();
    assert!(err.to_string().contains("protected"), "{err}");
    let allowed = allowed_attempts(&sb, &["echo x > .claude/settings.json"]).await;
    assert_eq!(allowed, "", "these attempts were not refused");
    assert!(!ws.path().join("fake/settings.json").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn claude_directory_cannot_become_a_link() {
    let ws = tempfile::tempdir().unwrap();
    let settings = ws.path().join(".claude/settings.json");
    let Some(sb) = sandbox_with(ws.path(), |b| b.protect(&settings)).await else {
        return;
    };
    let allowed = allowed_attempts(
        &sb,
        &[
            "mkdir fake && echo {} > fake/settings.json && ln -s fake .claude",
            // `mv` falls back to copying and may leave an empty `.claude`.
            "mv fake .claude",
        ],
    )
    .await;
    assert_eq!(allowed, "", "these attempts were not refused");
    if let Ok(meta) = std::fs::symlink_metadata(ws.path().join(".claude")) {
        assert!(meta.is_dir(), ".claude is a link");
    }
    assert!(!settings.exists());
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

fn action(name: &str, command: &[&str]) -> claustrum_sandbox::ActionDef {
    claustrum_sandbox::ActionDef {
        name: name.into(),
        description: format!("test action {name}"),
        command: command.iter().map(|s| (*s).to_owned()).collect(),
        ..Default::default()
    }
}

fn pattern_input(name: &str, pattern: &str) -> claustrum_sandbox::action::InputDef {
    claustrum_sandbox::action::InputDef {
        name: name.into(),
        pattern: Some(pattern.into()),
        ..Default::default()
    }
}

/// Actions used by the tests below: echo, a validated echo, a sleeper that
/// records its pid, and one that tampers with the configuration.
fn test_actions() -> Vec<claustrum_sandbox::ActionDef> {
    vec![
        action("echo", &["/bin/echo", "fixed", "$(id)"]),
        claustrum_sandbox::ActionDef {
            inputs: vec![
                pattern_input("word", "[a-z]{1,10}"),
                claustrum_sandbox::action::InputDef {
                    default: Some("dflt".into()),
                    ..pattern_input("second", "[a-z]{1,10}")
                },
            ],
            ..action("say", &["/bin/echo", "{word}", "{second}"])
        },
        claustrum_sandbox::ActionDef {
            timeout_secs: Some(0),
            ..action(
                "sleep",
                &["/bin/sh", "-c", "echo $$ > sleeper.pid; exec /bin/sleep 30"],
            )
        },
        action(
            "tamper",
            &[
                "/bin/sh",
                "-c",
                "echo 'network = \"host\"' > claustrum.toml; echo done",
            ],
        ),
    ]
}

#[tokio::test(flavor = "multi_thread")]
async fn host_actions_run_through_bash() {
    let ws = tempfile::tempdir().unwrap();
    let Some(sb) = sandbox_with(ws.path(), |b| b.actions(test_actions())).await else {
        return;
    };
    assert!(sb.commands().iter().any(|c| c == "host"));
    assert_eq!(sb.action_command(), Some("host"));

    // Listing, piping and no shell interpretation of the fixed argv.
    let out = sb
        .bash(
            "host | head -1; host echo | tr a-z A-Z; host say hello; host say second=x word=abc",
            ExecOptions::default(),
        )
        .await
        .unwrap();
    assert!(out.success(), "stderr: {}", out.stderr_lossy());
    assert_eq!(
        out.stdout_lossy(),
        "Host actions (run with `host <name> [input ...]`; inputs positionally in the order \
         listed or as name=value):\nFIXED $(ID)\nhello dflt\nabc x\n"
    );

    // Refusals: extra inputs, bad inputs, unknown actions. Exit code 2, no run.
    let out = sb
        .bash(
            "host echo extra; echo rc=$?; host say 'hello; id'; echo rc=$?; host say -n; echo rc=$?; \
             host nope; echo rc=$?; host say; echo rc=$?",
            ExecOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(out.stdout_lossy(), "rc=2\nrc=2\nrc=2\nrc=2\nrc=2\n");
    let stderr = out.stderr_lossy();
    for expected in [
        "takes no inputs",
        "does not match the pattern",
        "must not start with `-`",
        "unknown action `nope`",
        "missing required input `word`",
    ] {
        assert!(
            stderr.contains(expected),
            "missing `{expected}` in {stderr}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn killed_bash_call_stops_the_host_action() {
    let ws = tempfile::tempdir().unwrap();
    let Some(sb) = sandbox_with(ws.path(), |b| b.actions(test_actions())).await else {
        return;
    };
    let started = std::time::Instant::now();
    let out = sb
        .bash(
            "host sleep",
            ExecOptions {
                timeout: Some(Duration::from_secs(2)),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(out.reason, ExitReason::TimedOut);
    assert!(started.elapsed() < Duration::from_secs(10));
    let pid: i32 = std::fs::read_to_string(ws.path().join("sleeper.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    // SAFETY: signal 0 only checks whether the process exists.
    let alive = unsafe { libc::kill(pid, 0) } == 0;
    assert!(!alive, "host process {pid} outlived the guest");
}

/// Whether the host process whose pid the `sleep` action wrote is alive.
fn sleeper_alive(ws: &std::path::Path) -> bool {
    let pid: i32 = std::fs::read_to_string(ws.join("sleeper.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // SAFETY: signal 0 only checks whether the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

async fn wait_for(path: &std::path::Path) {
    for _ in 0..100 {
        if path.exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("{} never appeared", path.display());
}

#[tokio::test(flavor = "multi_thread")]
async fn dropped_bash_call_stops_the_guest_and_its_action() {
    let ws = tempfile::tempdir().unwrap();
    let Some(sb) = sandbox_with(ws.path(), |b| b.actions(test_actions())).await else {
        return;
    };
    // The caller gives up (an MCP cancellation drops the tool future).
    let call = sb.bash(
        "echo started > started; host sleep; echo after > after",
        ExecOptions {
            timeout: Some(Duration::from_secs(60)),
            ..Default::default()
        },
    );
    let pid_file = ws.path().join("sleeper.pid");
    let give_up = async {
        wait_for(&pid_file).await;
    };
    tokio::select! {
        _ = call => panic!("the command finished on its own"),
        _ = give_up => {}
    }
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(
        !sleeper_alive(ws.path()),
        "host action outlived the dropped call"
    );
    assert!(!ws.path().join("after").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn dropped_action_call_stops_the_action() {
    let ws = tempfile::tempdir().unwrap();
    let Some(sb) = sandbox_with(ws.path(), |b| b.actions(test_actions())).await else {
        return;
    };
    let call = sb.run_action("sleep", Default::default());
    let pid_file = ws.path().join("sleeper.pid");
    tokio::select! {
        _ = call => panic!("the action finished on its own"),
        _ = wait_for(&pid_file) => {}
    }
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(
        !sleeper_alive(ws.path()),
        "host action outlived the dropped call"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn queued_action_honours_its_timeout() {
    let ws = tempfile::tempdir().unwrap();
    let Some(sb) = sandbox_with(ws.path(), |b| b.actions(test_actions())).await else {
        return;
    };
    let first = sb.bash(
        "host sleep",
        ExecOptions {
            timeout: Some(Duration::from_secs(60)),
            ..Default::default()
        },
    );
    tokio::pin!(first);
    // Let the first action start and take the lock.
    let pid_file = ws.path().join("sleeper.pid");
    tokio::select! {
        _ = &mut first => panic!("the first action finished on its own"),
        _ = wait_for(&pid_file) => {}
    }
    let started = std::time::Instant::now();
    let second = sb.bash(
        "host echo",
        ExecOptions {
            timeout: Some(Duration::from_secs(1)),
            ..Default::default()
        },
    );
    let out = tokio::select! {
        _ = &mut first => panic!("the first action finished on its own"),
        out = second => out.unwrap(),
    };
    assert_eq!(out.reason, ExitReason::TimedOut);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the queued call waited {:?}",
        started.elapsed()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn host_command_channel_is_bounded() {
    let ws = tempfile::tempdir().unwrap();
    let Some(sb) = sandbox_with(ws.path(), |b| b.actions(test_actions())).await else {
        return;
    };
    // 32 MiB straight into the request channel, past the shim.
    let out = sb
        .bash(
            "head -c 33554432 /dev/zero > /.claustrum/cmd/host; echo \"exit=$?\"",
            ExecOptions::default(),
        )
        .await
        .unwrap();
    let stdout = out.stdout_lossy();
    assert!(
        !stdout.contains("exit=0"),
        "stdout: {stdout} stderr: {}",
        out.stderr_lossy()
    );
    // The channel still works for well-formed requests.
    let out = sb.bash("host echo", ExecOptions::default()).await.unwrap();
    assert!(out.success(), "stderr: {}", out.stderr_lossy());
}

#[tokio::test(flavor = "multi_thread")]
async fn host_actions_cannot_change_the_configuration() {
    let ws = tempfile::tempdir().unwrap();
    let config = ws.path().join("claustrum.toml");
    std::fs::write(&config, "# original\n").unwrap();
    // Without the OS sandbox the action can write the file; the restore
    // afterwards is what protects it.
    let Some(sb) = sandbox_with(ws.path(), |b| {
        b.protect(ws.path().join("claustrum.toml"))
            .actions(test_actions())
            .policy(Policy {
                default_timeout: Some(Duration::from_secs(60)),
                confinement: claustrum_sandbox::Confinement {
                    mode: claustrum_sandbox::ConfinementMode::Off,
                    ..Default::default()
                },
                ..Policy::default()
            })
    })
    .await
    else {
        return;
    };
    let out = sb
        .bash("host tamper", ExecOptions::default())
        .await
        .unwrap();
    assert_eq!(out.exit_code, 1);
    assert_eq!(out.stdout_lossy(), "done\n");
    assert!(
        out.stderr_lossy().contains("restored"),
        "{}",
        out.stderr_lossy()
    );
    assert_eq!(std::fs::read_to_string(&config).unwrap(), "# original\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn confined_actions_cannot_even_write_the_configuration() {
    let ws = tempfile::tempdir().unwrap();
    let config = ws.path().join("claustrum.toml");
    std::fs::write(&config, "# original\n").unwrap();
    // The library default confines where the platform can.
    let Some(sb) = sandbox_with(ws.path(), |b| {
        b.protect(ws.path().join("claustrum.toml"))
            .actions(test_actions())
    })
    .await
    else {
        return;
    };
    if !claustrum_sandbox::Confinement::default()
        .active()
        .unwrap_or(false)
    {
        eprintln!("skipping: no confinement backend");
        return;
    }
    let out = sb
        .bash("host tamper", ExecOptions::default())
        .await
        .unwrap();
    assert_eq!(out.stdout_lossy(), "done\n");
    assert!(
        !out.stderr_lossy().contains("restored"),
        "{}",
        out.stderr_lossy()
    );
    assert_eq!(std::fs::read_to_string(&config).unwrap(), "# original\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn run_action_serves_the_mcp_tool() {
    let ws = tempfile::tempdir().unwrap();
    let Some(sb) = sandbox_with(ws.path(), |b| b.actions(test_actions())).await else {
        return;
    };
    let inputs = [("word".to_owned(), "hi".to_owned())].into();
    let out = sb.run_action("say", inputs).await.unwrap();
    assert!(out.success());
    assert_eq!(out.stdout_lossy(), "hi dflt\n");

    let err = sb
        .run_action("say", [("word".to_owned(), "H I".to_owned())].into())
        .await
        .unwrap_err();
    assert!(matches!(err, claustrum_sandbox::Error::Action(_)), "{err}");
    assert!(
        sb.action_listing()
            .unwrap()
            .contains("say: test action say")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn action_command_must_not_shadow_a_package_command() {
    let ws = tempfile::tempdir().unwrap();
    let dir = packages_dir();
    if !dir.join("bash.webc").exists() || !dir.join("coreutils.webc").exists() {
        return;
    }
    let err = Sandbox::builder()
        .workspace(ws.path())
        .package_named(dir.join("bash.webc"), "wasmer/bash@1.0.25")
        .package_named(dir.join("coreutils.webc"), "wasmer/coreutils@1.0.25")
        .action_command("ls")
        .actions(test_actions())
        .build()
        .await
        .expect_err("must fail");
    assert!(err.to_string().contains("already provided"), "{err}");
}

/// A loopback HTTP server that answers every request with `hello`, and
/// counts the connections it accepted.
async fn hello_server() -> (
    std::net::SocketAddr,
    std::sync::Arc<std::sync::atomic::AtomicUsize>,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = hits.clone();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = listener.accept().await {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf).await;
                let _ = s
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello",
                    )
                    .await;
            });
        }
    });
    (addr, hits)
}

fn network_policy(network: claustrum_sandbox::NetworkPolicy) -> Policy {
    Policy {
        default_timeout: Some(Duration::from_secs(300)),
        network,
        ..Policy::default()
    }
}

const PY_CONNECT: &str = "python3 -c \"import socket,sys; s=socket.create_connection(('127.0.0.1', int(sys.argv[1])), timeout=10); s.sendall(b'GET / HTTP/1.0\\r\\n\\r\\n'); print(s.recv(100).decode().split()[-1])\"";

#[tokio::test(flavor = "multi_thread")]
async fn guest_connections_follow_the_allowlist() {
    let ws = tempfile::tempdir().unwrap();
    let (addr, hits) = hello_server().await;
    let allowed =
        claustrum_sandbox::NetworkPolicy::allowlist(&[format!("127.0.0.1:{}", addr.port())])
            .unwrap();
    let Some(sb) = sandbox_with(ws.path(), |b| b.policy(network_policy(allowed))).await else {
        return;
    };
    if !has_command(&sb, "python3") {
        return;
    }
    let script = format!("{PY_CONNECT} {}", addr.port());
    let out = sb.bash(&script, ExecOptions::default()).await.unwrap();
    assert!(out.success(), "stderr: {}", out.stderr_lossy());
    assert_eq!(out.stdout_lossy(), "hello\n");
    assert!(out.network_notes.is_empty(), "{:?}", out.network_notes);
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);

    // A different port on the same address is refused before it reaches the host.
    let (other, other_hits) = hello_server().await;
    let out = sb
        .bash(
            &format!("{PY_CONNECT} {}", other.port()),
            ExecOptions::default(),
        )
        .await
        .unwrap();
    assert!(!out.success());
    assert!(
        out.stderr_lossy().contains("PermissionError"),
        "{}",
        out.stderr_lossy()
    );
    assert_eq!(other_hits.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert!(
        out.network_notes
            .iter()
            .any(|n| n.starts_with("refused tcp 127.0.0.1:") && n.contains("local or private")),
        "{:?}",
        out.network_notes
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn disabled_and_audit_modes() {
    let ws = tempfile::tempdir().unwrap();
    let (addr, hits) = hello_server().await;
    let script = format!("{PY_CONNECT} {}", addr.port());

    let Some(sb) = sandbox(ws.path()).await else {
        return;
    };
    if !has_command(&sb, "python3") {
        return;
    }
    let out = sb
        .bash(
            &script,
            ExecOptions {
                timeout: Some(Duration::from_secs(300)),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(!out.success());
    assert!(
        out.network_notes.iter().any(|n| n.contains("disabled")),
        "{:?}",
        out.network_notes
    );
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 0);

    let audit = claustrum_sandbox::NetworkPolicy {
        mode: claustrum_sandbox::net::NetMode::Audit,
        ..Default::default()
    };
    let Some(sb) = sandbox_with(ws.path(), |b| b.policy(network_policy(audit))).await else {
        return;
    };
    let out = sb.bash(&script, ExecOptions::default()).await.unwrap();
    assert!(out.success(), "stderr: {}", out.stderr_lossy());
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(
        out.network_notes
            .iter()
            .any(|n| n.starts_with("audit mode allowed")),
        "{:?}",
        out.network_notes
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn host_actions_go_through_the_proxy() {
    let ws = tempfile::tempdir().unwrap();
    let (addr, hits) = hello_server().await;
    let (blocked, blocked_hits) = hello_server().await;
    let log = ws.path().join("net.jsonl");
    let mut network =
        claustrum_sandbox::NetworkPolicy::allowlist(&[format!("127.0.0.1:{}", addr.port())])
            .unwrap();
    network.log = Some(log.clone());
    let fetch = |name: &str, port: u16| claustrum_sandbox::ActionDef {
        timeout_secs: Some(30),
        ..action(
            name,
            &[
                "/usr/bin/curl",
                "-sSf",
                "--max-time",
                "10",
                &format!("http://127.0.0.1:{port}/"),
            ],
        )
    };
    let Some(sb) = sandbox_with(ws.path(), |b| {
        b.policy(network_policy(network)).actions([
            fetch("fetch", addr.port()),
            fetch("blocked", blocked.port()),
        ])
    })
    .await
    else {
        return;
    };
    if !std::path::Path::new("/usr/bin/curl").exists() {
        eprintln!("skipping: no /usr/bin/curl");
        return;
    }
    let out = sb.bash("host fetch", ExecOptions::default()).await.unwrap();
    assert!(out.success(), "stderr: {}", out.stderr_lossy());
    assert_eq!(out.stdout_lossy(), "hello");
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);

    let out = sb
        .bash("host blocked", ExecOptions::default())
        .await
        .unwrap();
    assert!(!out.success());
    assert_eq!(blocked_hits.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert!(
        out.network_notes
            .iter()
            .any(|n| n.starts_with("refused http 127.0.0.1:")),
        "{:?}",
        out.network_notes
    );

    let entries = claustrum_sandbox::net::read_log(&log).unwrap();
    assert!(
        entries
            .iter()
            .any(|e| e.source == "action:fetch" && e.verdict == "allowed")
    );
    assert!(
        entries
            .iter()
            .any(|e| e.source == "action:blocked" && e.verdict == "refused")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn network_log_is_read_only_for_the_guest() {
    let ws = tempfile::tempdir().unwrap();
    let mut network = claustrum_sandbox::NetworkPolicy::allowlist(&["crates.io"]).unwrap();
    network.log = Some(ws.path().join("net.jsonl"));
    let Some(sb) = sandbox_with(ws.path(), |b| b.policy(network_policy(network))).await else {
        return;
    };
    let allowed = allowed_attempts(
        &sb,
        &[
            "echo forged > net.jsonl",
            "rm -f net.jsonl",
            "mv net.jsonl gone.jsonl",
        ],
    )
    .await;
    assert_eq!(allowed, "", "these attempts were not refused");
    assert!(ws.path().join("net.jsonl").exists());
    assert!(sb.write("net.jsonl", "forged").await.is_err());
}
