//! The configured actions as a set: the `host` guest command and the entry
//! point the MCP `Action` tool uses.

use std::{collections::BTreeMap, sync::Arc};

use super::{
    bind::{Refusal, parse_args},
    host::ActionExecutor,
    run::ActionOutcome,
    spec::ActionSpec,
};
use crate::hostcmd::{Cancel, HostCommand, HostOutput, Invocation};

/// Exit code of a refused invocation (bad inputs, unknown action).
const REFUSED: i32 = 2;

#[derive(Debug)]
pub(crate) struct ActionSet {
    command: String,
    specs: Vec<ActionSpec>,
    executor: Arc<dyn ActionExecutor>,
}

impl ActionSet {
    pub(crate) fn new(
        command: String,
        specs: Vec<ActionSpec>,
        executor: Arc<dyn ActionExecutor>,
    ) -> Self {
        Self {
            command,
            specs,
            executor,
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
        let confined = self
            .specs
            .iter()
            .filter(|spec| self.executor.is_confined(spec))
            .count();
        let mixed = confined > 0 && confined < self.specs.len();
        for spec in &self.specs {
            s.push_str(&format!("  {}", spec.name));
            if mixed && !self.executor.is_confined(spec) {
                s.push_str(" [unconfined]");
            }
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
        if confined > 0 {
            s.push_str(if mixed {
                "Actions not marked [unconfined] run in an OS sandbox: "
            } else {
                "The actions run in an OS sandbox: "
            });
            s.push_str(
                "they can write only the workspace, their own $TMPDIR and declared caches, \
                 cannot read credential stores, cannot change the Claustrum configuration, \
                 and reach the network only through Claustrum's proxy under the same rules as \
                 the sandbox.\n",
            );
        }
        s
    }

    /// Validate and run one invocation (on the executor's side).
    pub(crate) fn run(
        &self,
        name: &str,
        positional: &[String],
        named: &BTreeMap<String, String>,
        guest_cwd: &str,
        cancel: &Cancel,
    ) -> Result<ActionOutcome, Refusal> {
        self.executor
            .run(name, positional, named, guest_cwd, cancel)
    }
}

/// Environment variables that point common tools at the proxy.
pub(crate) const PROXY_VARS: &[&str] = &[
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "CARGO_HTTP_PROXY",
    "npm_config_proxy",
    "npm_config_https_proxy",
    "NO_PROXY",
    "no_proxy",
];

pub(crate) fn proxy_env(url: &str) -> BTreeMap<String, String> {
    PROXY_VARS
        .iter()
        .map(|k| {
            let v = if k.eq_ignore_ascii_case("no_proxy") {
                String::new()
            } else {
                url.to_owned()
            };
            ((*k).to_owned(), v)
        })
        .collect()
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
        match self.run(
            name,
            &positional,
            &named,
            &invocation.cwd,
            &invocation.cancel,
        ) {
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
