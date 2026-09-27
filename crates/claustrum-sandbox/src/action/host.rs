//! The side that actually runs actions on the host.
//!
//! [`ActionHost`] validates an invocation against its own copy of the
//! definitions and starts the program, confined by the OS sandbox where
//! enabled. It lives in the same process as the sandbox, or, when the sandbox
//! runs in a confined worker, in the unconfined broker, which receives the
//! worker's requests through [`super::remote`]. Either way everything the
//! guest sends is treated as untrusted and bound again here.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, TryLockError},
    time::Duration,
};

/// How often an action waiting for another one checks its cancel flag.
const LOCK_POLL: Duration = Duration::from_millis(50);

use claustrum_confine::{Exec, Network, Profile};

use super::{
    bind::{Refusal, bind},
    command::proxy_env,
    run::{ActionOutcome, execute},
    spec::{ActionSpec, Confine},
};
use crate::{
    hostcmd::Cancel,
    net::{ActionProxy, ConnectionLog, NetMode, NetPolicy, ProxyHandle},
    policy::Confinement,
};

/// Runs a validated action somewhere: locally ([`ActionHost`]) or through
/// the broker ([`super::remote::RemoteActions`]).
pub trait ActionExecutor: Send + Sync + std::fmt::Debug {
    /// Validate and run one invocation. `guest_cwd` resolves relative path
    /// inputs.
    fn run(
        &self,
        name: &str,
        positional: &[String],
        named: &BTreeMap<String, String>,
        guest_cwd: &str,
        cancel: &Cancel,
    ) -> Result<ActionOutcome, Refusal>;

    /// Whether `spec` runs inside the OS sandbox.
    fn is_confined(&self, spec: &ActionSpec) -> bool;
}

#[derive(Debug)]
pub struct ActionHost {
    command: String,
    specs: Vec<ActionSpec>,
    workspace: PathBuf,
    protected: Vec<PathBuf>,
    /// Network proxy the actions are pointed at; `None` in host mode.
    proxy: Option<ProxyHandle>,
    net_log: Arc<ConnectionLog>,
    /// Whether actions with `confine = "os"` get the OS sandbox.
    confined: bool,
    deny_read: Vec<PathBuf>,
    /// One action at a time.
    running: Mutex<()>,
}

impl ActionHost {
    /// Start the host side: the network proxy (unless the network is in host
    /// mode) and the confinement check. Needs a tokio runtime.
    #[allow(clippy::too_many_arguments)]
    pub async fn start(
        command: String,
        specs: Vec<ActionSpec>,
        workspace: PathBuf,
        protected: Vec<PathBuf>,
        net_policy: Arc<NetPolicy>,
        net_log: Arc<ConnectionLog>,
        confinement: &Confinement,
    ) -> Result<Self, String> {
        let confined = confinement.active()?;
        // Actions reach the network through the proxy, which applies the
        // same policy. In host mode they are left alone.
        let proxy = if net_policy.mode() != NetMode::Host {
            Some(
                ActionProxy::start(net_policy, Arc::clone(&net_log))
                    .await
                    .map_err(|e| format!("cannot start the action proxy: {e}"))?,
            )
        } else {
            None
        };
        if !confined {
            for spec in &specs {
                if spec.confine == Confine::Os {
                    tracing::warn!(
                        action = spec.name,
                        "OS confinement is off: the action runs with your full rights"
                    );
                }
            }
        }
        Ok(Self {
            command,
            specs,
            workspace,
            protected,
            proxy,
            net_log,
            confined,
            deny_read: confinement.deny_read.clone(),
            running: Mutex::new(()),
        })
    }

    pub fn specs(&self) -> &[ActionSpec] {
        &self.specs
    }

    fn profile(&self, spec: &ActionSpec, tmp: &Path) -> Profile {
        let mut p = Profile::new(format!("action {}", spec.name), &spec.program);
        p.exec = Exec::Any;
        p.read_everything = true;
        p.deny_read = self.deny_read.clone();
        p.write = vec![self.workspace.clone(), tmp.to_path_buf()];
        for dir in &spec.writable {
            // Bind mounts (Linux) need the directory to exist; the
            // configuration asked for it, so create it like the program would.
            if !dir.exists()
                && let Err(e) = std::fs::create_dir_all(dir)
            {
                tracing::warn!(action = spec.name, path = %dir.display(), error = %e, "cannot create a writable directory");
            }
            p.write.push(dir.clone());
        }
        p.deny_write = self.protected.clone();
        // Bubblewrap cannot keep a missing file directly in a writable tree
        // from being created and refuses such a profile. The run restores
        // protected paths afterwards and moves aside what was created.
        #[cfg(target_os = "linux")]
        p.deny_write.retain(|f| {
            f.exists()
                || !p
                    .write
                    .iter()
                    .any(|w| f.parent() == Some(claustrum_confine::resolve(w).as_path()))
        });
        p.network = match &self.proxy {
            Some(proxy) => Network::Loopback(proxy.addr().port()),
            None => Network::Any,
        };
        p.proxy_socket = self.proxy.as_ref().and_then(ProxyHandle::unix_socket);
        p
    }
}

impl ActionExecutor for ActionHost {
    fn is_confined(&self, spec: &ActionSpec) -> bool {
        self.confined && spec.confine == Confine::Os
    }

    fn run(
        &self,
        name: &str,
        positional: &[String],
        named: &BTreeMap<String, String>,
        guest_cwd: &str,
        cancel: &Cancel,
    ) -> Result<ActionOutcome, Refusal> {
        let spec = self.specs.iter().find(|s| s.name == name).ok_or_else(|| {
            Refusal(format!(
                "unknown action `{name}`; run `{}` without arguments for the list",
                self.command
            ))
        })?;
        let bound = bind(spec, positional, named, guest_cwd, &self.workspace)?;
        let confined = self.is_confined(spec);
        tracing::info!(
            action = name,
            program = %spec.program.display(),
            argv = ?bound.argv,
            inputs = ?bound.values,
            cwd = %spec.cwd.display(),
            confined,
            "running host action"
        );
        // Wait for a running action without losing sight of the cancel
        // flag: the guest's timeout or a dropped call must still end this
        // invocation while another action holds the lock.
        let _guard = loop {
            if cancel.is_cancelled() {
                return Err(Refusal(format!(
                    "action `{name}`: cancelled before it started"
                )));
            }
            match self.running.try_lock() {
                Ok(guard) => break guard,
                Err(TryLockError::Poisoned(p)) => break p.into_inner(),
                Err(TryLockError::WouldBlock) => std::thread::sleep(LOCK_POLL),
            }
        };
        if cancel.is_cancelled() {
            return Err(Refusal(format!(
                "action `{name}`: cancelled before it started"
            )));
        }
        let seq = self.net_log.next_seq();
        let mut env = self
            .proxy
            .as_ref()
            .map(|p| proxy_env(&p.url_for(name)))
            .unwrap_or_default();
        let tmp = if confined {
            let dir =
                PrivateTmp::create(name).map_err(|e| Refusal(format!("action `{name}`: {e}")))?;
            env.insert("TMPDIR".into(), dir.path().display().to_string());
            Some(dir)
        } else {
            None
        };
        let profile = tmp.as_ref().map(|t| self.profile(spec, t.path()));
        let mut outcome = execute(
            spec,
            &bound,
            &self.protected,
            &env,
            cancel,
            profile.as_ref(),
        )
        .map_err(|e| Refusal(format!("action `{name}`: {e}")))?;
        outcome.network_notes = self.net_log.notes_since(seq);
        tracing::info!(
            action = name,
            exit_code = outcome.exit_code,
            killed = outcome.killed,
            duration = ?outcome.duration,
            "host action finished"
        );
        Ok(outcome)
    }
}

/// A fresh temporary directory for one confined action, removed afterwards.
/// The action's `$TMPDIR`: a fresh directory with a random name, created
/// exclusively and readable by the user only, so that other local users can
/// neither pre-create it nor read what the action leaves there. Removed when
/// the action is done.
struct PrivateTmp(tempfile::TempDir);

impl PrivateTmp {
    fn create(action: &str) -> std::io::Result<Self> {
        // Canonical, so that OS profiles match it (macOS: /var → /private/var).
        let base = std::env::temp_dir().canonicalize()?;
        let prefix = format!("claustrum-{action}-");
        let mut builder = tempfile::Builder::new();
        builder.prefix(&prefix);
        // Set at creation, not afterwards: nobody else can open it between.
        #[cfg(unix)]
        builder.permissions(std::os::unix::fs::PermissionsExt::from_mode(0o700));
        let dir = builder.tempdir_in(base)?;
        Ok(Self(dir))
    }

    fn path(&self) -> &Path {
        self.0.path()
    }
}

#[cfg(all(test, unix))]
mod tmp_tests {
    use super::PrivateTmp;

    #[test]
    fn private_tmp_is_random_private_and_removed() {
        use std::os::unix::fs::PermissionsExt;
        let a = PrivateTmp::create("build").unwrap();
        let b = PrivateTmp::create("build").unwrap();
        assert_ne!(a.path(), b.path());
        let mode = std::fs::metadata(a.path()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
        let path = a.path().to_path_buf();
        drop(a);
        assert!(!path.exists());
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::{
        action::spec::{ActionDef, CompileContext},
        net::NetMode,
        policy::ConfinementMode,
    };

    const PROBE: &str = r#"
echo x > inside && echo "workspace-write ok"
echo x >> claustrum.toml && echo "config-write ALLOWED"
echo x > "$ESCAPE" && echo "outside-write ALLOWED"
cat "$SECRET/key" && echo "secret-read ALLOWED"
echo x > "$TMPDIR/t" && echo "tmp-write ok"
/usr/bin/nc -z -G 2 1.1.1.1 443 && echo "network ALLOWED"
echo done
"#;

    async fn host(ws: &Path, secret: &Path, confine: Confine, mode: ConfinementMode) -> ActionHost {
        let spec = ActionDef {
            name: "probe".into(),
            command: vec!["/bin/sh".into(), "probe.sh".into()],
            env_passthrough: vec!["ESCAPE".into(), "SECRET".into()],
            confine,
            ..Default::default()
        }
        .compile(&CompileContext {
            workspace: ws,
            default_timeout: Some(Duration::from_secs(30)),
            max_output_bytes: 64 * 1024,
            resolve_programs: true,
        })
        .unwrap();
        ActionHost::start(
            "host".into(),
            vec![spec],
            ws.to_path_buf(),
            vec![ws.join("claustrum.toml")],
            Arc::new(NetPolicy::new(NetMode::Disabled, Vec::new())),
            Arc::new(ConnectionLog::memory()),
            &Confinement {
                mode,
                deny_read: vec![secret.to_path_buf()],
            },
        )
        .await
        .unwrap()
    }

    fn run(host: &ActionHost) -> String {
        let out = host
            .run(
                "probe",
                &[],
                &BTreeMap::new(),
                crate::WORKSPACE,
                &Cancel::new(),
            )
            .unwrap();
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn confined_action_stays_in_its_profile() {
        let ws = tempfile::tempdir().unwrap();
        let ws = ws.path().canonicalize().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside = outside.path().canonicalize().unwrap();
        std::fs::create_dir(outside.join("secret")).unwrap();
        std::fs::write(outside.join("secret/key"), "k").unwrap();
        std::fs::write(ws.join("claustrum.toml"), "# original\n").unwrap();
        std::fs::write(ws.join("probe.sh"), PROBE).unwrap();
        // SAFETY: test-only; no other thread reads these variables.
        unsafe {
            std::env::set_var("ESCAPE", outside.join("escape"));
            std::env::set_var("SECRET", outside.join("secret"));
        }

        let confined = host(
            &ws,
            &outside.join("secret"),
            Confine::Os,
            ConfinementMode::Required,
        )
        .await;
        let out = run(&confined);
        assert!(out.contains("workspace-write ok"), "{out}");
        assert!(out.contains("tmp-write ok"), "{out}");
        assert!(out.contains("done"), "{out}");
        assert!(!out.contains("ALLOWED"), "{out}");
        assert!(!outside.join("escape").exists());
        assert_eq!(
            std::fs::read_to_string(ws.join("claustrum.toml")).unwrap(),
            "# original\n"
        );

        // `confine = "none"` opts out: the same script escapes (the config is
        // restored afterwards by the snapshot).
        let open = host(
            &ws,
            &outside.join("secret"),
            Confine::None,
            ConfinementMode::Required,
        )
        .await;
        let out = run(&open);
        assert!(out.contains("outside-write ALLOWED"), "{out}");
        assert!(out.contains("secret-read ALLOWED"), "{out}");
        assert_eq!(
            std::fs::read_to_string(ws.join("claustrum.toml")).unwrap(),
            "# original\n"
        );
    }
}
