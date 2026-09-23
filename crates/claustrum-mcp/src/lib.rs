//! Claustrum MCP server.
//!
//! Exposes the sandbox to Claude Code as MCP tools. The tool names and
//! parameters deliberately mirror Claude Code's built-in `Bash`, `Read`,
//! `Write`, `Edit`, `Glob` and `Grep` tools so that the model can use them
//! without adapting, once the built-ins are disabled.

mod format;
mod server;

pub use server::{ClaustrumServer, serve_stdio};
