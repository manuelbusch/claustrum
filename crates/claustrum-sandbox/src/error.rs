use std::path::PathBuf;

/// Errors produced by the sandbox.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("unable to initialise the runtime: {0}")]
    Init(String),

    #[error("unable to load package `{path}`: {message}")]
    Package { path: PathBuf, message: String },

    #[error("command `{0}` is not provided by any loaded package")]
    CommandNotFound(String),

    #[error("command `{command}` is ambiguous; provided by {packages:?}")]
    CommandAmbiguous {
        command: String,
        packages: Vec<String>,
    },

    #[error("unable to start the guest process: {0}")]
    Spawn(String),

    #[error("guest process failed: {0}")]
    Execution(String),

    #[error("invalid guest path `{path}`: {message}")]
    InvalidPath { path: String, message: String },

    #[error("file system error at `{path}`: {source}")]
    Fs {
        path: String,
        #[source]
        source: virtual_fs::FsError,
    },

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub(crate) fn fs(path: impl Into<String>, source: virtual_fs::FsError) -> Self {
        Error::Fs {
            path: path.into(),
            source,
        }
    }

    pub(crate) fn invalid_path(path: impl Into<String>, message: impl Into<String>) -> Self {
        Error::InvalidPath {
            path: path.into(),
            message: message.into(),
        }
    }
}
