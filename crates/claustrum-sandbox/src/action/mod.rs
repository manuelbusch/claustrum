//! Host actions: fixed host commands the guest may trigger.
//!
//! The user declares actions in `claustrum.toml` (which the guest cannot
//! change). Each one is a fixed program plus argv on the host; the guest can
//! only trigger it, with inputs that are validated against the declaration
//! before they reach the host. See [`ActionDef`] for the declaration,
//! [`bind`] for the validation and `run` for how the program is executed.
//!
//! Inside the guest the actions appear as one command (`host` by default);
//! the MCP server exposes the same set as its `Action` tool.

mod bind;
mod command;
mod run;
mod spec;

pub use bind::{Bound, Refusal, bind, parse_args};
pub(crate) use command::ActionSet;
pub use run::{ActionOutcome, KILLED_EXIT_CODE};
pub use spec::{
    ActionDef, ActionSpec, CompileContext, Confine, DEFAULT_MAX_LEN, InputDef, InputKind,
    InputSpec, Template, audit, compile_all, is_action_name,
};

/// Guest command name used when the configuration does not set one.
pub const DEFAULT_COMMAND: &str = "host";
