//! `claustrum serve`: MCP server over stdio.
//!
//! With OS confinement active this process is only the broker: it starts the
//! MCP server with the Wasmer runtime as a confined worker process
//! (`claustrum __worker`) that inherits stdin/stdout, and keeps for itself
//! what the worker must not do: starting host actions (each in its own OS
//! sandbox) and running their network proxy. The worker reaches the broker
//! through a socket on file descriptor [`BROKER_FD`].
//!
//! ```text
//! claude ──stdio──▶ worker (Wasmer + tools, OS profile)
//!                     │ socket: "run action X with inputs …"
//!                     ▼
//!                  broker (this process) ──▶ action (own OS profile)
//! ```

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::config::Config;

#[cfg(unix)]
pub use broker::{WorkerArgs, worker};

#[derive(clap::Args, Debug)]
pub struct Args {
    /// Host directory to mount at /workspace. Defaults to the current directory.
    #[arg(long)]
    pub workspace: Option<PathBuf>,
}

pub async fn run(config: Config, args: Args) -> Result<()> {
    let workspace = config.workspace(args.workspace.as_deref())?;
    config.migrate_state(&workspace);
    let confined = config.confinement()?.active().map_err(anyhow::Error::msg)?;
    let network = config.network_policy(&workspace)?;
    eprintln!("claustrum: {}", Config::network_notice(&network));
    eprintln!("claustrum: {}", Config::confinement_notice(confined));
    #[cfg(unix)]
    if confined {
        return broker::broker(config, &workspace).await;
    }
    serve_in_process(config, &workspace).await
}

/// No OS confinement: everything in this process, as a single layer.
async fn serve_in_process(config: Config, workspace: &Path) -> Result<()> {
    let sandbox = config.build_sandbox(Some(workspace), None).await?;
    tracing::info!(
        workspace = %sandbox.workspace_dir().display(),
        commands = sandbox.commands().len(),
        "sandbox ready"
    );
    if let Some(notice) = Config::actions_notice(sandbox.actions(), false) {
        eprintln!("claustrum: {notice}");
    }
    claustrum_mcp::serve_stdio(sandbox).await
}

#[cfg(unix)]
mod broker {
    use std::{
        os::{fd::AsRawFd, unix::net::UnixStream},
        path::{Path, PathBuf},
        sync::Arc,
    };

    use anyhow::{Context, Result};
    use claustrum_confine::{Network, Profile};
    use claustrum_sandbox::{
        RuntimeConfig,
        action::{ActionExecutor, ActionHost, remote},
        net::{ConnectionLog, NetMode, NetPolicy},
        plans::PlanStore,
    };

    use crate::config::{Config, Remote, default_config_paths};

    /// Descriptor number of the broker socket in the worker.
    const BROKER_FD: i32 = 3;
    /// Tells the worker that [`BROKER_FD`] is open.
    const BROKER_FD_ENV: &str = "CLAUSTRUM_BROKER_FD";

    pub(super) async fn broker(config: Config, workspace: &Path) -> Result<()> {
        let policy = config.policy(workspace)?;
        let specs = config.validate_actions(workspace)?;
        // So that the missing Claude Code settings have a directory that
        // bubblewrap can bind read-only (see `claustrum_confine::linux`).
        for claude_dir in config.claude_settings_dirs(workspace) {
            std::fs::create_dir_all(&claude_dir)
                .with_context(|| format!("cannot create {}", claude_dir.display()))?;
        }
        let protected = resolve_all(&config.protected_paths(workspace));
        let log_path = policy
            .network
            .log
            .clone()
            .context("the network log path is not set")?;

        let plans: Option<Arc<dyn PlanStore>> = config
            .host_plans(workspace)?
            .map(|p| Arc::new(p) as Arc<dyn PlanStore>);
        let host: Option<Arc<ActionHost>> = if specs.is_empty() {
            None
        } else {
            let net_policy = Arc::new(NetPolicy::new(
                policy.network.mode,
                policy.network.allow.clone(),
            ));
            let net_log = Arc::new(ConnectionLog::with_file(&log_path));
            Some(Arc::new(
                ActionHost::start(
                    config.action_command().to_owned(),
                    specs.clone(),
                    workspace.to_path_buf(),
                    protected.clone(),
                    net_policy,
                    net_log,
                    &policy.confinement,
                )
                .await
                .map_err(anyhow::Error::msg)?,
            ))
        };
        if let Some(notice) = Config::actions_notice(&specs, true) {
            eprintln!("claustrum: {notice}");
        }

        let exe = std::env::current_exe()
            .and_then(|p| p.canonicalize())
            .context("cannot locate the claustrum binary")?;
        // Private to the user (tempfile's default is 0755).
        let tmp = tempfile::Builder::new()
            .prefix("claustrum-worker-")
            .permissions(std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .tempdir()
            .context("cannot create the worker's temporary directory")?;
        let profile = worker_profile(&config, workspace, &exe, &protected, &log_path, tmp.path())?;

        let mut cmd = claustrum_confine::command(&profile, &exe)?;
        cmd.arg("__worker").arg("--workspace").arg(workspace);
        if let Some(path) = &config.path {
            cmd.arg("--config").arg(claustrum_confine::resolve(path));
        }
        cmd.arg("--packages-dir")
            .arg(&config.packages_dir)
            .env("TMPDIR", tmp.path());

        let (broker_end, worker_end) =
            UnixStream::pair().context("cannot create the broker socket")?;
        let fd = worker_end.as_raw_fd();
        cmd.env(BROKER_FD_ENV, BROKER_FD.to_string());
        // SAFETY: only async-signal-safe calls between fork and exec.
        unsafe {
            use std::os::unix::process::CommandExt as _;
            cmd.pre_exec(move || {
                if fd == BROKER_FD {
                    let flags = libc::fcntl(fd, libc::F_GETFD);
                    if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                } else if libc::dup2(fd, BROKER_FD) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = cmd.spawn().context("cannot start the confined worker")?;
        drop(worker_end);
        tracing::info!(pid = child.id(), profile = %profile.name, "worker started");

        let server = (host.is_some() || plans.is_some()).then(|| {
            let executor = host.map(|h| h as Arc<dyn ActionExecutor>);
            std::thread::spawn(move || {
                if let Err(e) = remote::serve(broker_end, executor, plans) {
                    tracing::error!(error = %e, "stopped serving the worker");
                }
            })
        });

        let pid = child.id();
        let wait = tokio::task::spawn_blocking(move || child.wait());
        tokio::pin!(wait);
        let status = tokio::select! {
            status = &mut wait => status??,
            _ = shutdown_signal() => {
                tracing::info!("shutting down");
                stop_worker(pid, &mut wait).await;
                drop(server);
                drop(tmp);
                std::process::exit(143);
            }
        };
        drop(server);
        drop(tmp);
        if status.success() {
            Ok(())
        } else {
            std::process::exit(status.code().unwrap_or(1));
        }
    }

    /// End the worker: SIGTERM, then SIGKILL if it is still there after a
    /// grace period. `wait` is the task reaping it. Without this the worker
    /// could outlive the broker on macOS, where nothing ties it to its parent.
    async fn stop_worker<F>(pid: u32, wait: &mut std::pin::Pin<&mut F>)
    where
        F: std::future::Future,
    {
        const GRACE: std::time::Duration = std::time::Duration::from_secs(2);
        let Ok(pid) = i32::try_from(pid) else {
            return;
        };
        // SAFETY: signalling our own child, which `wait` has not reaped yet.
        unsafe { libc::kill(pid, libc::SIGTERM) };
        if tokio::time::timeout(GRACE, wait.as_mut()).await.is_err() {
            tracing::warn!(pid, "worker ignored SIGTERM; killing it");
            // SAFETY: as above.
            unsafe { libc::kill(pid, libc::SIGKILL) };
            let _ = tokio::time::timeout(GRACE, wait.as_mut()).await;
        }
    }

    async fn shutdown_signal() {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
        let mut int = signal(SignalKind::interrupt()).expect("SIGINT handler");
        tokio::select! {
            _ = term.recv() => {},
            _ = int.recv() => {},
        }
    }

    /// What the Wasmer worker may touch: its binary, the packages, the
    /// configuration, read-only extra mounts and the user-wide module cache
    /// (read), the workspace, writable extra mounts, its workspace's module
    /// cache and network log and a private temporary directory (read/write),
    /// never the
    /// configuration (write) or credential stores (read). It may not start any
    /// program; host actions go through the broker.
    fn worker_profile(
        config: &Config,
        workspace: &Path,
        exe: &Path,
        protected: &[PathBuf],
        log_path: &Path,
        tmp: &Path,
    ) -> Result<Profile> {
        let confinement = config.confinement()?;
        let mut p = Profile::new("worker", exe);
        p.read.push(exe.to_path_buf());
        p.read.push(config.packages_dir.clone());
        for package in config.packages() {
            p.read.push(package.file);
        }
        p.read.extend(protected.iter().cloned());
        p.read.extend(default_config_paths());

        p.write.push(workspace.to_path_buf());
        for m in &config.file.mounts {
            let host = crate::config::expand_home(&m.host);
            if m.writable {
                p.write.push(host);
            } else {
                p.read.push(host);
            }
        }
        // The user-wide module cache is read-only for the worker; it writes
        // a cache of its own workspace (see `Config::worker_cache_dir`).
        p.read
            .push(RuntimeConfig::default().cache_dir.join("modules"));
        let cache = config.worker_cache_dir(workspace);
        // Bind mounts need the directory to exist.
        std::fs::create_dir_all(&cache)
            .with_context(|| format!("cannot create {}", cache.display()))?;
        p.write.push(cache);
        // Only this workspace's log, created here so that it exists for the
        // bind mount; the broker appends to the same file.
        if let Some(dir) = log_path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("cannot create {}", dir.display()))?;
        }
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)
            .with_context(|| format!("cannot create {}", log_path.display()))?;
        p.write.push(log_path.to_path_buf());
        p.write.push(tmp.to_path_buf());

        p.deny_write = protected.to_vec();
        // Bubblewrap cannot keep a missing file directly in a writable tree
        // from being created and refuses such a profile. Only a project
        // `claustrum.toml` lies there; a new one is not used before the user
        // trusts its content, and the WASIX layer refuses to create it.
        #[cfg(target_os = "linux")]
        p.deny_write.retain(|f| {
            f.exists()
                || !p
                    .write
                    .iter()
                    .any(|w| f.parent() == Some(claustrum_confine::resolve(w).as_path()))
        });
        p.deny_read = confinement.deny_read;
        // Also when CLAUDE_CONFIG_DIR moves it out of the built-in list; plan
        // files go through the broker.
        p.deny_read.push(crate::config::claude_config_dir());
        p.network = match config.network_mode()? {
            NetMode::Disabled if !config.file.packages.online => Network::None,
            NetMode::Host => Network::Any,
            _ => Network::Outbound,
        };
        Ok(p)
    }

    fn resolve_all(paths: &[PathBuf]) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = Vec::new();
        for p in paths {
            let r = claustrum_confine::resolve(p);
            if !out.contains(&r) {
                out.push(r);
            }
        }
        out
    }

    /// `claustrum __worker`: the confined half of `serve`.
    #[derive(clap::Args, Debug)]
    pub struct WorkerArgs {
        #[arg(long)]
        pub workspace: PathBuf,
    }

    pub async fn worker(config: Config, args: WorkerArgs) -> Result<()> {
        let (executor, plans) = match std::env::var(BROKER_FD_ENV) {
            Ok(fd) => {
                let fd: i32 = fd.parse().context("invalid broker descriptor")?;
                // SAFETY: the broker opened this descriptor for us (see
                // `broker`) and nothing else in this process uses it.
                let stream = unsafe {
                    use std::os::fd::FromRawFd as _;
                    UnixStream::from_raw_fd(fd)
                };
                let client = remote::BrokerClient::new(stream)?;
                let actions = !config.file.actions.list.is_empty();
                (
                    actions.then(|| Arc::clone(&client) as Arc<dyn ActionExecutor>),
                    config
                        .file
                        .claude
                        .plans
                        .then_some(client as Arc<dyn PlanStore>),
                )
            }
            Err(_) => anyhow::bail!("__worker is started by `claustrum serve`, not directly"),
        };
        #[cfg(debug_assertions)]
        if std::env::var_os(ESCAPE_PROBE_ENV).is_some() {
            escape_probe(&config, &args.workspace);
            return Ok(());
        }
        let sandbox = config
            .build_sandbox(Some(&args.workspace), Some(Remote { executor, plans }))
            .await?;
        tracing::info!(
            workspace = %sandbox.workspace_dir().display(),
            commands = sandbox.commands().len(),
            "confined worker ready"
        );
        claustrum_mcp::serve_stdio(sandbox).await
    }

    /// Debug builds only: when set, the worker does what a WASIX escape
    /// would try with raw host calls, reports each result on stderr and
    /// exits. Used by the confinement tests.
    #[cfg(debug_assertions)]
    pub const ESCAPE_PROBE_ENV: &str = "CLAUSTRUM_ESCAPE_PROBE";

    #[cfg(debug_assertions)]
    fn escape_probe(config: &Config, workspace: &Path) {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
        let report = |what: &str, ok: bool| {
            eprintln!("probe {what}: {}", if ok { "ALLOWED" } else { "denied" });
        };
        report(
            "workspace-write",
            std::fs::write(workspace.join(".claustrum-probe"), "x").is_ok(),
        );
        let _ = std::fs::remove_file(workspace.join(".claustrum-probe"));
        report(
            "config-write",
            std::fs::OpenOptions::new()
                .append(true)
                .open(workspace.join("claustrum.toml"))
                .is_ok(),
        );
        report(
            "claude-settings-write",
            std::fs::write(workspace.join(".claude/settings.json"), "{}").is_ok(),
        );
        let cache = RuntimeConfig::default().cache_dir;
        report(
            "shared-cache-write",
            std::fs::write(cache.join("modules/claustrum-probe"), "x").is_ok(),
        );
        let own = config.worker_cache_dir(workspace).join("claustrum-probe");
        report("workspace-cache-write", std::fs::write(&own, "x").is_ok());
        let _ = std::fs::remove_file(&own);
        report(
            "other-log-write",
            config.network_log_path(workspace).is_ok_and(|log| {
                std::fs::write(log.with_file_name("claustrum-probe.jsonl"), "x").is_ok()
            }),
        );
        let plans = crate::config::claude_plans_dir();
        report("claude-plans-read", std::fs::read_dir(&plans).is_ok());
        report(
            "claude-plans-write",
            std::fs::write(plans.join("claustrum-probe.md"), "x").is_ok(),
        );
        report(
            "home-write",
            std::fs::write(home.join(".claustrum-escape"), "x").is_ok(),
        );
        report("ssh-read", std::fs::read_dir(home.join(".ssh")).is_ok());
        report(
            "exec",
            std::process::Command::new("/bin/echo").output().is_ok(),
        );
        report(
            "network",
            std::net::TcpStream::connect_timeout(
                &"1.1.1.1:443".parse().expect("address"),
                std::time::Duration::from_secs(3),
            )
            .is_ok(),
        );
    }
}
