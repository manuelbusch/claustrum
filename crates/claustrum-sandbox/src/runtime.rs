//! Construction of the shared Wasmer/WASIX runtime.
//!
//! One [`PluggableRuntime`] is built per process. It owns the compiled-module
//! cache, the package loader and the package source list. Per-sandbox
//! differences (network policy) are layered on top with `OverriddenRuntime`.

use std::{path::PathBuf, sync::Arc};

use wasmer_wasix::{
    PluggableRuntime, Runtime,
    runtime::{
        DefaultTty,
        module_cache::{FileSystemCache, ModuleCache, SharedCache},
        package_loader::BuiltinPackageLoader,
        resolver::{BackendSource, InMemorySource, MultiSource, PackageSummary},
        task_manager::tokio::TokioTaskManager,
    },
};

use crate::{Error, Result};

/// Options for building the shared runtime.
#[derive(Clone, Debug)]
pub struct RuntimeConfig {
    /// Root directory for caches (compiled modules, downloaded packages).
    pub cache_dir: PathBuf,
    /// Local `.webc` files registered as package sources. Dependencies between
    /// bundled packages (e.g. bash → coreutils) are resolved offline from here.
    pub bundled_packages: Vec<BundledPackage>,
    /// Whether the Wasmer registry may be contacted to resolve packages that
    /// are not bundled. Off by default.
    pub online: bool,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        let cache_dir = directories::ProjectDirs::from("de", "buschmanuel", "claustrum")
            .map(|d| d.cache_dir().to_path_buf())
            .unwrap_or_else(|| std::env::temp_dir().join("claustrum-cache"));
        Self {
            cache_dir,
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

    let module_cache =
        SharedCache::default().with_fallback(FileSystemCache::new(modules_dir, tasks.clone()));

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

    runtime
        .set_tty(Arc::new(DefaultTty::default()))
        .set_module_cache(module_cache)
        .set_source(source)
        .set_package_loader(BuiltinPackageLoader::new().with_cache_dir(packages_dir))
        // Deny networking by default; sandboxes opt in through their policy.
        .set_networking_implementation(virtual_net::UnsupportedVirtualNetworking::default());

    Ok(Arc::new(runtime))
}
