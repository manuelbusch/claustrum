//! The [`Sandbox`]: a persistent workspace plus a set of guest commands.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use wasmer_wasix::{Runtime, runtime::OverriddenRuntime, virtual_net::DynVirtualNetworking};

use crate::{
    BundledPackage, Error, ExecOptions, ExecOutput, NetworkPolicy, PackageSet, Policy, Result,
    RuntimeConfig, WORKSPACE,
    fs::{self, GuestFs, Mount},
    native, process,
    runtime::build_runtime,
};

/// Guest directory holding Claustrum's own support files.
const ETC_DIR: &str = "/etc/claustrum";
const PROFILE_FILE: &str = "/profile.sh";

/// Sourced by every bash via `BASH_ENV`. The guest cannot tell that stdout is
/// not a terminal (WASIX reports stdio as character devices), so tools that
/// colourise or page based on `isatty` are tamed here.
const PROFILE: &str = r#"# Claustrum sandbox profile. Output is captured for a language model, not shown
# on a terminal: no colours, no pagers, no interactive prompts.
export PAGER=cat GIT_PAGER=cat NO_COLOR=1 CLICOLOR=0 PYTHONUNBUFFERED=1
jq() { command jq -M "$@"; }
ls() { command ls --color=never "$@"; }
"#;

/// Builder for a [`Sandbox`].
#[derive(Debug)]
pub struct SandboxBuilder {
    workspace: Option<PathBuf>,
    packages: Vec<BundledPackage>,
    dir_packages: Vec<PathBuf>,
    policy: Policy,
    runtime: RuntimeConfig,
    env: BTreeMap<String, String>,
    extra_mounts: Vec<(String, PathBuf)>,
    protected: Vec<PathBuf>,
}

impl Default for SandboxBuilder {
    fn default() -> Self {
        let mut env = BTreeMap::new();
        env.insert("HOME".into(), "/home/claude".into());
        env.insert("USER".into(), "claude".into());
        env.insert("PATH".into(), "/usr/local/bin:/bin:/usr/bin".into());
        env.insert("TERM".into(), "dumb".into());
        env.insert("LANG".into(), "C.UTF-8".into());
        env.insert("PWD".into(), WORKSPACE.into());
        Self {
            workspace: None,
            packages: Vec::new(),
            dir_packages: Vec::new(),
            policy: Policy::default(),
            runtime: RuntimeConfig::default(),
            env,
            extra_mounts: Vec::new(),
            protected: Vec::new(),
        }
    }
}

impl SandboxBuilder {
    /// Host directory mounted read/write at `/workspace`.
    pub fn workspace(mut self, dir: impl Into<PathBuf>) -> Self {
        self.workspace = Some(dir.into());
        self
    }

    /// Add a `.webc` package whose commands become available in the guest.
    pub fn package(mut self, webc: impl Into<PathBuf>) -> Self {
        self.packages.push(BundledPackage::new(webc));
        self
    }

    /// Add a `.webc` package under an explicit identity (`ns/name@version`),
    /// so that other bundled packages can depend on it.
    pub fn package_named(mut self, webc: impl Into<PathBuf>, id: impl Into<String>) -> Self {
        self.packages.push(BundledPackage::named(webc, id));
        self
    }

    /// Add a directory package (`wasmer.toml` plus `.wasm` modules), e.g. a
    /// self-built WASIX binary.
    pub fn package_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.dir_packages.push(dir.into());
        self
    }

    pub fn policy(mut self, policy: Policy) -> Self {
        self.policy = policy;
        self
    }

    pub fn runtime_config(mut self, config: RuntimeConfig) -> Self {
        self.runtime = config;
        self
    }

    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }

    /// Mount an additional host directory at an absolute guest path.
    pub fn mount(mut self, guest: impl Into<String>, host: impl Into<PathBuf>) -> Self {
        self.extra_mounts.push((guest.into(), host.into()));
        self
    }

    /// Make a host file read-only for the guest, whether or not it exists.
    ///
    /// Inside every host-backed mount that contains it, the file can be read
    /// but not written, truncated, created, removed, renamed or replaced, and
    /// no directory containing it can be renamed or removed. Relative paths
    /// are resolved against the current directory. Used for the Claustrum
    /// configuration, which must not be changeable from inside the sandbox.
    pub fn protect(mut self, path: impl Into<PathBuf>) -> Self {
        self.protected.push(path.into());
        self
    }

    /// Build the sandbox. Must be called from within a tokio runtime.
    pub async fn build(mut self) -> Result<Sandbox> {
        let workspace_dir = self
            .workspace
            .take()
            .ok_or_else(|| Error::Init("a workspace directory is required".into()))?;
        let handle = tokio::runtime::Handle::try_current()
            .map_err(|_| Error::Init("a tokio runtime is required".into()))?;
        let workspace_dir = workspace_dir
            .canonicalize()
            .map_err(|e| Error::Init(format!("cannot resolve {}: {e}", workspace_dir.display())))?;
        let cwd = std::env::current_dir()
            .map_err(|e| Error::Init(format!("cannot determine the current directory: {e}")))?;
        let mut protected = Vec::with_capacity(self.protected.len());
        for p in &self.protected {
            let abs = cwd.join(p);
            let resolved = crate::protect::resolve_host_path(&abs)
                .map_err(|e| Error::Init(format!("cannot resolve {}: {e}", abs.display())))?;
            if !protected.contains(&resolved) {
                protected.push(resolved);
            }
        }

        // Bundled packages double as offline package sources so that
        // dependencies between them (bash → coreutils) resolve locally.
        for p in &self.packages {
            if !self.runtime.bundled_packages.contains(p) {
                self.runtime.bundled_packages.push(p.clone());
            }
        }
        let base_runtime = build_runtime(&self.runtime)?;

        let mut packages = PackageSet::default();
        for package in &self.packages {
            packages.add_webc(&package.path, &*base_runtime).await?;
        }
        for dir in &self.dir_packages {
            packages.add_dir(dir, &*base_runtime).await?;
        }

        let etc = fs::mem_dir();
        crate::native::write_file(&*etc, Path::new(PROFILE_FILE), PROFILE.as_bytes())
            .await
            .map_err(|e| Error::fs(format!("{ETC_DIR}{PROFILE_FILE}"), e))?;
        self.env
            .entry("BASH_ENV".into())
            .or_insert_with(|| format!("{ETC_DIR}{PROFILE_FILE}"));

        let mut mounts = vec![
            Mount {
                guest: WORKSPACE.into(),
                fs: fs::host_dir(handle.clone(), &workspace_dir, &protected)?,
            },
            Mount {
                guest: "/tmp".into(),
                fs: fs::mem_dir(),
            },
            Mount {
                guest: "/home/claude".into(),
                fs: fs::mem_dir(),
            },
            Mount {
                guest: ETC_DIR.into(),
                fs: etc,
            },
        ];
        for (guest, host) in &self.extra_mounts {
            if !guest.starts_with('/') {
                return Err(Error::invalid_path(guest, "mount paths must be absolute"));
            }
            mounts.push(Mount {
                guest: guest.clone(),
                fs: fs::host_dir(handle.clone(), host, &protected)?,
            });
        }

        let networking = networking_for(&self.policy.network)?;
        let runtime: Arc<dyn Runtime + Send + Sync> =
            Arc::new(OverriddenRuntime::new(base_runtime).with_networking(networking));

        Ok(Sandbox {
            inner: Arc::new(Inner {
                runtime,
                packages,
                guest_fs: GuestFs::new(mounts.clone()),
                mounts,
                policy: self.policy,
                env: self.env,
                workspace_dir,
                protected,
                cwd: Mutex::new(WORKSPACE.to_owned()),
            }),
        })
    }
}

fn networking_for(policy: &NetworkPolicy) -> Result<DynVirtualNetworking> {
    Ok(match policy {
        NetworkPolicy::Disabled => Arc::new(virtual_net::UnsupportedVirtualNetworking::default()),
        NetworkPolicy::Host => Arc::new(virtual_net::host::LocalNetworking::default()),
        NetworkPolicy::Ruleset(rules) => {
            let ruleset: virtual_net::ruleset::Ruleset = rules
                .join(",")
                .parse()
                .map_err(|e| Error::Init(format!("invalid network ruleset: {e}")))?;
            Arc::new(virtual_net::host::LocalNetworking::with_ruleset(ruleset))
        }
    })
}

/// A persistent sandbox. Cheap to clone; all clones share the same state.
#[derive(Clone)]
pub struct Sandbox {
    inner: Arc<Inner>,
}

struct Inner {
    runtime: Arc<dyn Runtime + Send + Sync>,
    packages: PackageSet,
    mounts: Vec<Mount>,
    guest_fs: GuestFs,
    policy: Policy,
    env: BTreeMap<String, String>,
    workspace_dir: PathBuf,
    protected: Vec<PathBuf>,
    cwd: Mutex<String>,
}

impl std::fmt::Debug for Sandbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sandbox")
            .field("workspace_dir", &self.inner.workspace_dir)
            .field("commands", &self.inner.packages.command_names().len())
            .finish()
    }
}

impl Sandbox {
    pub fn builder() -> SandboxBuilder {
        SandboxBuilder::default()
    }

    /// The host directory mounted at `/workspace`.
    pub fn workspace_dir(&self) -> &Path {
        &self.inner.workspace_dir
    }

    /// The guest view of the persistent mounts, for native tools.
    pub fn guest_fs(&self) -> &GuestFs {
        &self.inner.guest_fs
    }

    /// Resolved host paths of the files the guest may read but not change.
    pub fn protected_paths(&self) -> &[PathBuf] {
        &self.inner.protected
    }

    pub fn policy(&self) -> &Policy {
        &self.inner.policy
    }

    /// Names of all guest commands (from the loaded packages).
    pub fn commands(&self) -> Vec<String> {
        self.inner.packages.command_names()
    }

    /// Compile every distinct module of the loaded packages into the module
    /// cache, so that the first invocation of a large tool (python takes
    /// close to a minute to compile) does not eat into a command timeout.
    /// Returns the number of modules compiled or loaded.
    pub async fn precompile(&self) -> Result<usize> {
        use std::collections::HashSet;
        let mut seen = HashSet::new();
        let mut count = 0;
        for package in self.inner.packages.packages() {
            for command in &package.commands {
                if !seen.insert(*command.hash()) {
                    continue;
                }
                tracing::info!(command = command.name(), "compiling module");
                self.inner
                    .runtime
                    .resolve_module(
                        wasmer_wasix::runtime::ModuleInput::Command(std::borrow::Cow::Borrowed(
                            command,
                        )),
                        None,
                        None,
                    )
                    .await
                    .map_err(|e| {
                        Error::Other(format!("cannot compile `{}`: {e}", command.name()))
                    })?;
                count += 1;
            }
        }
        Ok(count)
    }

    /// Current working directory used for commands and relative paths.
    pub fn cwd(&self) -> String {
        self.inner.cwd.lock().expect("cwd lock poisoned").clone()
    }

    /// Change the working directory (guest path, may be relative to the cwd).
    pub fn set_cwd(&self, path: &str) -> Result<String> {
        let normalized = fs::normalize_guest_path(path, &self.cwd())?;
        let (mount, inner) = self.inner.guest_fs.resolve(&normalized)?;
        let meta = mount
            .fs
            .metadata(&inner)
            .map_err(|e| Error::fs(&normalized, e))?;
        if !meta.is_dir() {
            return Err(Error::invalid_path(path, "not a directory"));
        }
        *self.inner.cwd.lock().expect("cwd lock poisoned") = normalized.clone();
        Ok(normalized)
    }

    /// Run a guest command to completion and capture its output.
    pub async fn exec(&self, command: &str, options: ExecOptions) -> Result<ExecOutput> {
        let cwd = options.cwd.clone().unwrap_or_else(|| self.cwd());
        let cwd = fs::normalize_guest_path(&cwd, &self.cwd())?;
        process::run(process::Spawn {
            runtime: &self.inner.runtime,
            packages: &self.inner.packages,
            mounts: &self.inner.mounts,
            policy: &self.inner.policy,
            base_env: &self.inner.env,
            command,
            cwd,
            options,
        })
        .await
    }

    /// Run `bash -c <script>` in the sandbox.
    pub async fn bash(&self, script: &str, mut options: ExecOptions) -> Result<ExecOutput> {
        let mut args = vec!["-c".to_owned(), script.to_owned()];
        args.append(&mut options.args);
        options.args = args;
        self.exec("bash", options).await
    }
}

/// Native tool wrappers that use the sandbox's current working directory.
impl Sandbox {
    pub async fn read(&self, path: &str, opts: native::ReadOptions) -> Result<native::ReadOutput> {
        native::read(self.guest_fs(), path, &self.cwd(), opts).await
    }

    pub async fn write(&self, path: &str, content: &str) -> Result<native::WriteOutput> {
        native::write(self.guest_fs(), path, &self.cwd(), content).await
    }

    pub async fn edit(
        &self,
        path: &str,
        old_string: &str,
        new_string: &str,
        replace_all: bool,
    ) -> Result<native::EditOutput> {
        native::edit(
            self.guest_fs(),
            path,
            &self.cwd(),
            old_string,
            new_string,
            replace_all,
        )
        .await
    }

    pub fn glob(&self, pattern: &str, root: Option<&str>) -> Result<native::GlobOutput> {
        native::glob(self.guest_fs(), pattern, root, &self.cwd())
    }

    pub async fn grep(
        &self,
        pattern: &str,
        opts: native::GrepOptions,
    ) -> Result<native::GrepOutput> {
        native::grep(self.guest_fs(), pattern, &self.cwd(), opts).await
    }
}
