//! Construction of the shared Wasmer/WASIX runtime.
//!
//! One [`PluggableRuntime`] is built per process. It owns the compiled-module
//! cache, the package loader and the package source list. Per-sandbox
//! differences (network policy) are layered on top with `OverriddenRuntime`.

use std::{path::PathBuf, sync::Arc};

use wasmer::{Engine, Module};
use wasmer_types::ModuleHash;

use wasmer_wasix::{
    PluggableRuntime, Runtime,
    os::tty::{TtyBridge, WasiTtyState},
    runtime::{
        module_cache::{CacheError, FileSystemCache, ModuleCache, SharedCache},
        package_loader::BuiltinPackageLoader,
        resolver::{BackendSource, InMemorySource, MultiSource, PackageSummary},
        task_manager::tokio::TokioTaskManager,
    },
};

use crate::{Error, Result};

/// Options for building the shared runtime.
#[derive(Clone, Debug)]
pub struct RuntimeConfig {
    /// Writable root for caches (compiled modules, downloaded packages, the
    /// host command shim).
    pub cache_dir: PathBuf,
    /// A cache of compiled modules that is consulted before `cache_dir` but
    /// never written. A confined worker gets the user-wide cache here and a
    /// cache of its own workspace as `cache_dir`: compiled modules are native
    /// code loaded without further checks, so the worker must not be able
    /// to plant them for other workspaces or for unconfined processes.
    pub shared_modules: Option<PathBuf>,
    /// Local `.webc` files registered as package sources. Dependencies between
    /// bundled packages (e.g. bash → coreutils) are resolved offline from here.
    pub bundled_packages: Vec<BundledPackage>,
    /// Whether the Wasmer registry may be contacted to resolve packages that
    /// are not bundled. Off by default.
    pub online: bool,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        let cache_dir = std::env::var_os("CLAUSTRUM_CACHE_DIR")
            .map(PathBuf::from)
            .or_else(|| {
                directories::ProjectDirs::from("de", "buschmanuel", "claustrum")
                    .map(|d| d.cache_dir().to_path_buf())
            })
            .unwrap_or_else(|| std::env::temp_dir().join("claustrum-cache"));
        Self {
            cache_dir,
            shared_modules: None,
            bundled_packages: Vec::new(),
            online: false,
        }
    }
}

/// A local `.webc` file that acts as an offline package source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BundledPackage {
    pub path: PathBuf,
    /// Identity in `namespace/name@version` form. Files downloaded with
    /// `wasmer package download` carry no name, so this is how dependencies
    /// like `wasmer/coreutils@^1.0.19` find them. Optional for packages that
    /// nothing depends on.
    pub id: Option<String>,
}

impl BundledPackage {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            id: None,
        }
    }

    pub fn named(path: impl Into<PathBuf>, id: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            id: Some(id.into()),
        }
    }

    fn summary(&self) -> Result<PackageSummary> {
        let err = |message: String| Error::Package {
            path: self.path.clone(),
            message,
        };
        let mut summary =
            PackageSummary::from_webc_file(&self.path).map_err(|e| err(e.to_string()))?;
        if let Some(id) = &self.id {
            let (name, version) = id.rsplit_once('@').ok_or_else(|| {
                err(format!(
                    "package id `{id}` must look like namespace/name@version"
                ))
            })?;
            let version: semver::Version = version
                .parse()
                .map_err(|e| err(format!("invalid version in `{id}`: {e}")))?;
            summary.pkg.id = wasmer_config::package::PackageId::new_named(name, version);
        }
        Ok(summary)
    }
}

/// A TTY bridge that reports "not a terminal" on all three standard streams.
///
/// Guest output is captured and handed to a model, so tools must not emit
/// colours, pagers or progress bars. Wasmer's `DefaultTty` claims a terminal
/// on every stream, which makes `jq`, `ls` and friends colourise.
#[derive(Debug)]
struct NoTty;

impl TtyBridge for NoTty {
    fn reset(&self) {}

    fn tty_get(&self) -> WasiTtyState {
        WasiTtyState {
            stdin_tty: false,
            stdout_tty: false,
            stderr_tty: false,
            echo: false,
            line_buffered: false,
            line_feeds: false,
            ..WasiTtyState::default()
        }
    }

    fn tty_set(&self, _state: WasiTtyState) {}
}

/// A module cache that is only read; saving is a no-op.
#[derive(Debug)]
struct ReadOnlyCache<C>(C);

#[async_trait::async_trait]
impl<C: ModuleCache + Send + Sync> ModuleCache for ReadOnlyCache<C> {
    async fn load(
        &self,
        key: ModuleHash,
        engine: &Engine,
    ) -> std::result::Result<Module, CacheError> {
        self.0.load(key, engine).await
    }

    async fn contains(
        &self,
        key: ModuleHash,
        engine: &Engine,
    ) -> std::result::Result<bool, CacheError> {
        self.0.contains(key, engine).await
    }

    async fn save(
        &self,
        _key: ModuleHash,
        _engine: &Engine,
        _module: &Module,
    ) -> std::result::Result<(), CacheError> {
        Ok(())
    }
}

/// Build the shared runtime. Must be called from within a tokio runtime.
pub(crate) fn build_runtime(config: &RuntimeConfig) -> Result<Arc<dyn Runtime + Send + Sync>> {
    let handle = tokio::runtime::Handle::try_current()
        .map_err(|_| Error::Init("a tokio runtime is required".into()))?;

    let modules_dir = config.cache_dir.join("modules");
    let packages_dir = config.cache_dir.join("packages");
    for dir in [&modules_dir, &packages_dir] {
        std::fs::create_dir_all(dir)
            .map_err(|e| Error::Init(format!("cannot create cache dir {}: {e}", dir.display())))?;
    }

    let tasks = Arc::new(TokioTaskManager::new(handle));

    let own = FileSystemCache::new(modules_dir, tasks.clone());

    let mut source = MultiSource::new();
    let mut bundled = InMemorySource::new();
    for package in &config.bundled_packages {
        bundled.add(package.summary()?);
    }
    source.add_source(bundled);

    let mut runtime = PluggableRuntime::new(tasks.clone());
    if config.online
        && let Some(client) = runtime.http_client().cloned()
    {
        source.add_source(BackendSource::new(
            BackendSource::WASMER_PROD_ENDPOINT
                .parse()
                .expect("registry endpoint is a valid URL"),
            client,
        ));
    }

    match &config.shared_modules {
        // Trusted entries first, so the writable cache can only add modules.
        Some(shared) => runtime.set_module_cache(
            SharedCache::default()
                .with_fallback(ReadOnlyCache(FileSystemCache::new(shared, tasks.clone())))
                .with_fallback(own),
        ),
        None => runtime.set_module_cache(SharedCache::default().with_fallback(own)),
    };
    runtime
        .set_tty(Arc::new(NoTty))
        .set_source(source)
        .set_package_loader(BuiltinPackageLoader::new().with_cache_dir(packages_dir))
        // Deny networking by default; sandboxes opt in through their policy.
        .set_networking_implementation(virtual_net::UnsupportedVirtualNetworking::default());

    Ok(Arc::new(runtime))
}
