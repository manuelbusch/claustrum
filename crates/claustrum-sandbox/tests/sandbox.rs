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

#[tokio::test(flavor = "multi_thread")]
async fn host_actions_cannot_change_the_configuration() {
    let ws = tempfile::tempdir().unwrap();
    let config = ws.path().join("claustrum.toml");
    std::fs::write(&config, "# original\n").unwrap();
    let Some(sb) = sandbox_with(ws.path(), |b| {
        b.protect(ws.path().join("claustrum.toml"))
            .actions(test_actions())
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
