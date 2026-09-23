//! Claustrum sandbox runtime.
//!
//! This crate embeds the Wasmer WASIX runtime and exposes a small, opinionated
//! API for running guest commands (bash, coreutils, ...) against a persistent
//! workspace, plus "native" tools (read, write, edit, glob, grep) that operate
//! directly on the guest file system from the host.
//!
//! The main entry point is [`Sandbox`], built through [`SandboxBuilder`].

mod capture;
mod error;
pub mod fs;
pub mod native;
mod packages;
mod policy;
mod process;
mod runtime;
mod sandbox;

pub use error::{Error, Result};
pub use packages::{Downloaded, PackageSet, download};
pub use policy::{NetworkPolicy, Policy};
pub use process::{ExecOptions, ExecOutput, ExitReason};
pub use runtime::{BundledPackage, RuntimeConfig};
pub use sandbox::{Sandbox, SandboxBuilder};

/// Guest path of the mounted project directory.
pub const WORKSPACE: &str = "/workspace";
