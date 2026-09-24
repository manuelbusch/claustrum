//! The configured actions as a set: the `host` guest command and the entry
//! point the MCP `Action` tool uses.

use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::Mutex,
};

use super::{
    bind::{Refusal, bind, parse_args},
    run::{ActionOutcome, execute},
    spec::ActionSpec,
};
use crate::hostcmd::{Cancel, HostCommand, HostOutput, Invocation};

/// Exit code of a refused invocation (bad inputs, unknown action).
const REFUSED: i32 = 2;

#[derive(Debug)]
pub(crate) struct ActionSet {
    command: String,
    specs: Vec<ActionSpec>,
    workspace: PathBuf,
    protected: Vec<PathBuf>,
    /// One action at a time per sandbox.
    running: Mutex<()>,
}

impl ActionSet {
    pub(crate) fn new(
        command: String,
        specs: Vec<ActionSpec>,
        workspace: PathBuf,
        protected: Vec<PathBuf>,
    ) -> Self {
        Self {
            command,
            specs,
            workspace,
            protected,
            running: Mutex::new(()),
        }
    }

    pub(crate) fn command(&self) -> &str {
        &self.command
    }

    pub(crate) fn specs(&self) -> &[ActionSpec] {
        &self.specs
    }

    /// Human-readable list of the actions, shown for `host` without
    /// arguments and in the MCP tool description.
    pub(crate) fn listing(&self) -> String {
        let mut s = format!(
            "Host actions (run with `{} <name> [input ...]`; inputs positionally in the order \
             listed or as name=value):\n",
            self.command
        );
        for spec in &self.specs {
            s.push_str(&format!("  {}", spec.name));
            if !spec.description.is_empty() {
                s.push_str(&format!(": {}", spec.description));
            }
            s.push('\n');
            if spec.inputs.is_empty() {
                s.push_str("      (no inputs)\n");
            }
            for input in &spec.inputs {
                s.push_str(&format!("      {}\n", input.describe()));
            }
        }
        s
    }

    /// Validate, run and log one invocation.
    pub(crate) fn run(
        &self,
        name: &str,
        positional: &[String],
        named: &BTreeMap<String, String>,
        guest_cwd: &str,
        cancel: &Cancel,
    ) -> Result<ActionOutcome, Refusal> {
        let spec = self
            .specs
            .iter()
            .find(|s| s.name == name)
            .ok_or_else(|| {
                Refusal(format!(
                    "unknown action `{name}`; run `{}` without arguments for the list",
                    self.command
                ))
            })?;
        let bound = bind(spec, positional, named, guest_cwd, &self.workspace)?;
        tracing::info!(
            action = name,
            program = %spec.program.display(),
            argv = ?bound.argv,
            inputs = ?bound.values,
            cwd = %spec.cwd.display(),
            "running host action"
        );
        let _guard = self
            .running
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if cancel.is_cancelled() {
            return Err(Refusal(format!("action `{name}`: cancelled before it started")));
        }
        let outcome = execute(spec, &bound, &self.protected, cancel)
            .map_err(|e| Refusal(format!("action `{name}`: {e}")))?;
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

impl HostCommand for ActionSet {
    fn name(&self) -> &str {
        &self.command
    }

    fn run(&self, args: &[String], invocation: &Invocation) -> HostOutput {
        let Some((name, rest)) = args.split_first() else {
            return HostOutput::ok(self.listing());
        };
        if name == "--list" || name == "-l" || name == "--help" || name == "-h" {
            return HostOutput::ok(self.listing());
        }
        let Some(spec) = self.specs.iter().find(|s| &s.name == name) else {
            return HostOutput::fail(
                REFUSED,
                format!(
                    "{}: unknown action `{name}`\n{}",
                    self.command,
                    self.listing()
                ),
            );
        };
        let (positional, named) = parse_args(spec, rest);
        match self.run(name, &positional, &named, &invocation.cwd, &invocation.cancel) {
            Ok(outcome) => {
                let mut stderr = outcome.stderr;
                if outcome.stdout_truncated {
                    stderr.extend_from_slice(b"\nclaustrum: stdout truncated\n");
                }
                if outcome.stderr_truncated {
                    stderr.extend_from_slice(b"\nclaustrum: stderr truncated\n");
                }
                if outcome.killed {
                    stderr.extend_from_slice(
                        format!(
                            "\nclaustrum: action `{name}` was killed after {:.1?}\n",
                            outcome.duration
                        )
                        .as_bytes(),
                    );
                }
                HostOutput {
                    stdout: outcome.stdout,
                    stderr,
                    // A child's exit status travels through WASIX's errno
                    // type, so anything above about 77 reaches bash as 79.
                    code: if outcome.exit_code >= 78 {
                        1
                    } else {
                        outcome.exit_code
                    },
                }
            }
            Err(refusal) => HostOutput::fail(REFUSED, format!("{}: {refusal}\n", self.command)),
        }
    }
}
