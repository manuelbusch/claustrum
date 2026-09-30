# Host actions

Sometimes a task needs something that only exists on the host: the Rust toolchain, a deploy
script, a formatter. Instead of loosening the sandbox, `claustrum.toml` can declare
**actions**: fixed host commands that Claude may *trigger*, in the spirit of a CI job.

## Declaring actions

```toml
[[actions.action]]
name = "test"
description = "Run the whole Rust test suite"
command = ["cargo", "test", "--workspace"]
timeout_secs = 600
writable = ["~/.cargo/registry"]

[[actions.action]]
name = "test-crate"
description = "Run the tests of one crate, optionally filtered by test name"
command = ["cargo", "test", "-p", "{crate}", "--", "{filter}"]

[[actions.action.input]]
name = "crate"
pattern = "[a-z][a-z0-9-]{0,40}"

[[actions.action.input]]
name = "filter"
pattern = "[A-Za-z0-9_:]{0,80}"
default = ""
```

- `command` is the argv, one element per string. No shell is involved; `{name}`
  placeholders are replaced by validated [inputs](inputs.md), each within its own argv
  element.
- `description` is what Claude reads to decide when to use the action; make it precise.
- `writable` lists host directories the program may write besides the workspace and its
  private `$TMPDIR`, typically a package cache.
- All other keys (`cwd`, `env`, `env_passthrough`, `timeout_secs`, `max_output_bytes`,
  `confine`) are listed in the [reference](../configuration/reference.md#actionsaction).

## How Claude uses them

Inside the sandbox the actions appear as the `host` command (`host` lists them,
`host test` triggers one, `host test-crate claustrum-cli` or
`host test-crate crate=claustrum-cli` passes inputs), and as the `Action` MCP tool whose
description carries the same list. Because `host` is an ordinary command inside bash, pipes
and redirections work as usual (`host test 2>&1 | tail -20`).

The command name can be changed with `[actions] command`. It is a good idea to mention the
actions in [`[claude] system_prompt`](../configuration/reference.md#claude), especially when
they replace tools Claude would normally expect, such as `cargo` or `git`.

## Before you declare one

Every action is a hole in the sandbox that you cut on purpose. Read
[Use with care](risks.md) before declaring actions for programs that execute code from the
workspace (cargo, npm, make, git, ...). What Claustrum guarantees for every action is listed
in [Lifecycle and guarantees](lifecycle.md).
