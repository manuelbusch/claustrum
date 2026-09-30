# Subagents and workflows

## Subagents

The `Agent` tool is off by default and can be enabled in the configuration:

```toml
[claude]
tools = ["Agent"]
```

Subagents only get tools the session already has, so they work through the same
`mcp__claustrum__*` tools and stay in the sandbox. Probed with Claude Code 2.1.284:

- Explore and general-purpose get the Claustrum tools only, no built-in `Bash`, `Read`,
  `Write`, ...
- `tools: Bash, Read, Write` in an agent's frontmatter does not bring the built-ins back;
  Claude Code refuses to start an agent whose tool list resolves to nothing.
- `hooks` and `mcpServers` in an agent's frontmatter take no effect: no hook command runs on
  the host, and `--strict-mcp-config` keeps the extra server out.

Explore also gets `mcp__claustrum__Write` and `Edit`, so unlike in plain Claude Code it
is not read-only.

The guarantees above are Claude Code's behavior, not Claustrum's: `.claude/agents/` stays
writable from the sandbox, and a later Claude Code release that honors frontmatter hooks
there would run them on the host. Run
[`scripts/probe-subagents.sh`](https://github.com/manuelbusch/claustrum/blob/main/scripts/probe-subagents.sh)
on the host after upgrading Claude Code; every file it lists under "host side effects" is an
escape.

## Workflows

Workflows (multi-agent orchestration, "ultracode") need the `Workflow` tool as well:

```toml
[claude]
tools = ["Agent", "Workflow"]
```

The script runs in the `claude` process, but every agent it starts gets the Claustrum tools
only, like a subagent, and cannot start agents of its own.
[`scripts/probe-workflow.js`](https://github.com/manuelbusch/claustrum/blob/main/scripts/probe-workflow.js)
checks this: ask Claude to run it as a workflow and look at `leakedBuiltins` in the result,
which must be empty. Claude has to pass the script inline, because the `Workflow` tool reads
script files on the host and does not accept paths it has not seen itself.

## Host actions

Subagents and workflow agents can trigger [host actions](host-actions/index.md) through
`mcp__claustrum__Action` like the main session.
