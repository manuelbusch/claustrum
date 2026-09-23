//! Running a single guest command to completion.

use std::{collections::BTreeMap, io::Write as _, sync::Arc, time::Duration};

use wasmer_wasix::{
    Pipe, Runtime,
    bin_factory::spawn_exec,
    capabilities::Capabilities,
    runners::wasi::{PackageOrHash, RuntimeOrEngine, WasiRunner},
};
use wasmer_wasix_types::wasi::Signal;

use crate::{
    Error, Policy, Result,
    capture::{CaptureFile, CaptureHandle},
    fs::Mount,
    packages::PackageSet,
};

/// Per-invocation options for [`crate::Sandbox::exec`].
#[derive(Clone, Debug, Default)]
pub struct ExecOptions {
    pub args: Vec<String>,
    /// Working directory inside the guest. Defaults to the sandbox cwd.
    pub cwd: Option<String>,
    /// Extra environment variables layered over the sandbox environment.
    pub env: BTreeMap<String, String>,
    /// Bytes fed to stdin, followed by EOF. `None` closes stdin immediately.
    pub stdin: Option<Vec<u8>>,
    /// Overrides the policy's default timeout.
    pub timeout: Option<Duration>,
}

/// Why a command stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitReason {
    Exited,
    TimedOut,
}

/// The result of a finished guest command.
#[derive(Clone, Debug)]
pub struct ExecOutput {
    pub exit_code: i32,
    pub reason: ExitReason,
    pub stdout: Vec<u8>,
    pub stdout_truncated: bool,
    pub stderr: Vec<u8>,
    pub stderr_truncated: bool,
    pub duration: Duration,
}

impl ExecOutput {
    pub fn success(&self) -> bool {
        self.reason == ExitReason::Exited && self.exit_code == 0
    }

    pub fn stdout_lossy(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    pub fn stderr_lossy(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }
}

/// Everything needed to spawn one guest process.
pub(crate) struct Spawn<'a> {
    pub runtime: &'a Arc<dyn Runtime + Send + Sync>,
    pub packages: &'a PackageSet,
    pub mounts: &'a [Mount],
    pub policy: &'a Policy,
    pub base_env: &'a BTreeMap<String, String>,
    pub command: &'a str,
    pub cwd: String,
    pub options: ExecOptions,
}

pub(crate) async fn run(spawn: Spawn<'_>) -> Result<ExecOutput> {
    let started = std::time::Instant::now();
    let package = spawn.packages.resolve(spawn.command)?;

    let stdout = CaptureHandle::new(spawn.policy.max_output_bytes);
    let stderr = CaptureHandle::new(spawn.policy.max_output_bytes);

    let stdin: Box<dyn virtual_fs::VirtualFile + Send + Sync> = match &spawn.options.stdin {
        Some(bytes) => {
            let (mut tx, rx) = Pipe::channel();
            tx.write_all(bytes)?;
            tx.close();
            Box::new(rx)
        }
        None => Box::new(virtual_fs::NullFile::default()),
    };

    let mut env = spawn.base_env.clone();
    env.extend(
        spawn
            .options
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone())),
    );

    let mut capabilities = Capabilities::default();
    capabilities.threading.max_threads = spawn.policy.max_threads.map(|n| n as usize);
    capabilities.http_client.allow_all = false;

    let mut runner = WasiRunner::new();
    runner
        .with_args(spawn.options.args.clone())
        .with_envs(env)
        .with_forward_host_env(false)
        .with_current_dir(spawn.cwd.clone())
        .with_injected_packages(
            spawn
                .packages
                .packages()
                .iter()
                .filter(|p| p.id != package.id)
                .map(|p| (**p).clone()),
        )
        .with_capabilities(capabilities)
        .with_stdin(stdin)
        .with_stdout(Box::new(CaptureFile::new(stdout.clone())))
        .with_stderr(Box::new(CaptureFile::new(stderr.clone())));
    for mount in spawn.mounts {
        runner.with_mount(mount.guest.clone(), Arc::clone(&mount.fs));
    }

    let command = package
        .get_command(spawn.command)
        .ok_or_else(|| Error::CommandNotFound(spawn.command.to_owned()))?;
    let wasi = command
        .metadata()
        .annotation::<webc::metadata::annotations::Wasi>("wasi")
        .map_err(|e| Error::Spawn(format!("invalid command metadata: {e}")))?
        .unwrap_or_else(|| webc::metadata::annotations::Wasi::new(spawn.command));
    let exec_name = wasi.exec_name.as_deref().unwrap_or(spawn.command);

    let mut builder = runner
        .prepare_webc_env(
            exec_name,
            &wasi,
            PackageOrHash::Package(&package),
            RuntimeOrEngine::Runtime(Arc::clone(spawn.runtime)),
            None,
        )
        .map_err(|e| Error::Spawn(format!("{e:#}")))?;
    dedupe_env(builder.get_env_mut());

    let wasi_env = builder.build().map_err(|e| Error::Spawn(format!("{e}")))?;
    let process = wasi_env.process.clone();
    let main_tid = wasi_env.tid();

    let mut task = spawn_exec((*package).clone(), spawn.command, wasi_env, spawn.runtime)
        .await
        .map_err(|e| Error::Spawn(format!("{e}")))?;

    let timeout = spawn.options.timeout.or(spawn.policy.default_timeout);
    let wait = task.wait_finished();
    tokio::pin!(wait);

    let (result, reason) = match timeout {
        Some(limit) => {
            let deadline = tokio::time::sleep(limit);
            tokio::pin!(deadline);
            tokio::select! {
                r = &mut wait => (r, ExitReason::Exited),
                _ = &mut deadline => {
                    tracing::warn!(command = spawn.command, ?limit, "guest command timed out; killing");
                    (kill_tree(&process, &main_tid, &mut wait).await, ExitReason::TimedOut)
                }
            }
        }
        None => (wait.await, ExitReason::Exited),
    };

    let exit_code = match result {
        Ok(code) => code.raw(),
        Err(err) => {
            // Non-zero exits surface as Ok(code); anything else is a real failure.
            return Err(Error::Execution(err.to_string()));
        }
    };

    let (stdout, stdout_truncated) = stdout.snapshot();
    let (stderr, stderr_truncated) = stderr.snapshot();
    Ok(ExecOutput {
        exit_code: if reason == ExitReason::TimedOut {
            137
        } else {
            exit_code
        },
        reason,
        stdout,
        stdout_truncated,
        stderr,
        stderr_truncated,
        duration: started.elapsed(),
    })
}

/// Kill a process and its descendants.
///
/// WASIX delivers a process signal to the *children* of a process that is
/// currently waiting on them, not to the process itself, so a plain SIGKILL
/// only stops the innermost child and a shell keeps running its script.
/// Signal the root's main thread directly as well, repeat until the root
/// exits, then fall back to marking it terminated.
async fn kill_tree<F>(
    process: &wasmer_wasix::os::task::process::WasiProcess,
    main_tid: &wasmer_wasix::WasiThreadId,
    wait: &mut std::pin::Pin<&mut F>,
) -> std::result::Result<wasmer_wasix_types::wasi::ExitCode, Arc<wasmer_wasix::WasiRuntimeError>>
where
    F: std::future::Future<
            Output = std::result::Result<
                wasmer_wasix_types::wasi::ExitCode,
                Arc<wasmer_wasix::WasiRuntimeError>,
            >,
        >,
{
    const ATTEMPTS: u32 = 40;
    const INTERVAL: Duration = Duration::from_millis(25);
    for _ in 0..ATTEMPTS {
        process.signal_thread(main_tid, Signal::Sigkill);
        process.signal_process(Signal::Sigkill);
        let tick = tokio::time::sleep(INTERVAL);
        tokio::pin!(tick);
        tokio::select! {
            r = wait.as_mut() => return r,
            _ = &mut tick => {}
        }
    }
    tracing::warn!("process ignored SIGKILL; forcing termination");
    process.terminate(wasmer_wasix_types::wasi::ExitCode::from(137));
    wait.as_mut().await
}

/// WASI exposes `environ` as an array, so collapse duplicate keys (last wins).
fn dedupe_env(env: &mut Vec<(String, Vec<u8>)>) {
    let mut seen = std::collections::HashSet::with_capacity(env.len());
    env.reverse();
    env.retain(|(k, _)| seen.insert(k.clone()));
    env.reverse();
}
