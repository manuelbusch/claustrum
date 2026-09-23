//! Loaded WASIX packages and command lookup.

use std::{path::Path, sync::Arc};

use wasmer_wasix::{Runtime, bin_factory::BinaryPackage};

use crate::{Error, Result};

/// The set of packages whose commands are available inside a sandbox.
#[derive(Clone, Debug, Default)]
pub struct PackageSet {
    packages: Vec<Arc<BinaryPackage>>,
}

impl PackageSet {
    /// Load a `.webc` file and its dependencies.
    pub async fn add_webc(
        &mut self,
        path: &Path,
        runtime: &(dyn Runtime + Send + Sync),
    ) -> Result<()> {
        let container = wasmer_package::utils::from_disk(path).map_err(|e| Error::Package {
            path: path.to_path_buf(),
            message: e.to_string(),
        })?;
        let pkg = BinaryPackage::from_webc(&container, runtime)
            .await
            .map_err(|e| Error::Package {
                path: path.to_path_buf(),
                message: format!("{e:#}"),
            })?;
        if !self.packages.iter().any(|p| p.id == pkg.id) {
            self.packages.push(Arc::new(pkg));
        }
        Ok(())
    }

    /// All loaded packages.
    pub fn packages(&self) -> &[Arc<BinaryPackage>] {
        &self.packages
    }

    /// Names of all commands provided by the loaded packages, sorted.
    pub fn command_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .packages
            .iter()
            .flat_map(|p| p.commands.iter().map(|c| c.name().to_owned()))
            .collect();
        names.sort();
        names.dedup();
        names
    }

    /// Find the package that provides `command`.
    pub fn resolve(&self, command: &str) -> Result<Arc<BinaryPackage>> {
        let matches: Vec<&Arc<BinaryPackage>> = self
            .packages
            .iter()
            .filter(|p| p.get_command(command).is_some())
            .collect();
        match matches.as_slice() {
            [] => Err(Error::CommandNotFound(command.to_owned())),
            [pkg] => Ok(Arc::clone(pkg)),
            many => Err(Error::CommandAmbiguous {
                command: command.to_owned(),
                packages: many.iter().map(|p| p.id.to_string()).collect(),
            }),
        }
    }
}

/// A package fetched from the Wasmer registry into a local `.webc` file.
#[derive(Clone, Debug)]
pub struct Downloaded {
    pub path: std::path::PathBuf,
    /// Resolved identity, `namespace/name@version`.
    pub id: String,
}

/// Resolve `spec` (e.g. `wasmer/bash` or `wasmer/bash@1.0.25`) against the
/// Wasmer registry and download its `.webc` into `dest_dir`. The file is named
/// after the package (`bash.webc`).
pub async fn download(spec: &str, dest_dir: &Path) -> Result<Downloaded> {
    use wasmer_wasix::{
        http::{HttpClient, HttpRequest, HttpRequestOptions},
        runtime::resolver::{BackendSource, Source},
    };

    let client = wasmer_wasix::http::default_http_client()
        .ok_or_else(|| Error::Init("no HTTP client available".into()))?;
    let client = Arc::new(client);
    let source = BackendSource::new(
        BackendSource::WASMER_PROD_ENDPOINT
            .parse()
            .expect("registry endpoint is a valid URL"),
        client.clone(),
    );
    let package: wasmer_config::package::PackageSource = spec
        .parse()
        .map_err(|e| Error::Other(format!("invalid package spec `{spec}`: {e}")))?;
    let mut summaries = source
        .query(&package)
        .await
        .map_err(|e| Error::Other(format!("registry lookup for `{spec}` failed: {e}")))?;
    // Prefer the highest version when several match.
    summaries.sort_by(|a, b| a.pkg.id.cmp(&b.pkg.id));
    let summary = summaries
        .pop()
        .ok_or_else(|| Error::Other(format!("no package matches `{spec}`")))?;
    let id = summary.pkg.id.to_string();

    let request = HttpRequest {
        url: summary.dist.webc.clone(),
        method: http::Method::GET,
        headers: Default::default(),
        body: None,
        options: HttpRequestOptions {
            gzip: false,
            cors_proxy: None,
        },
    };
    let response = client
        .request(request)
        .await
        .map_err(|e| Error::Other(format!("download of `{id}` failed: {e}")))?;
    if !response.is_ok() {
        return Err(Error::Other(format!(
            "download of `{id}` failed with HTTP {}",
            response.status
        )));
    }
    let body = response.body.unwrap_or_default();

    let name = summary
        .pkg
        .id
        .as_named()
        .map(|n| {
            n.full_name
                .rsplit('/')
                .next()
                .unwrap_or(&n.full_name)
                .to_owned()
        })
        .unwrap_or_else(|| "package".to_owned());
    std::fs::create_dir_all(dest_dir)?;
    let path = dest_dir.join(format!("{name}.webc"));
    std::fs::write(&path, body)?;
    Ok(Downloaded { path, id })
}
