# Trusting a project's configuration

A `claustrum.toml` in the current directory usually comes with the project, possibly one you
just cloned, and it configures the sandbox that is meant to protect you from that project. It
could turn confinement off, open the network, mount your home directory, declare host
commands or name the program started as `claude`. So Claustrum does not use it until you have
accepted its exact content, as `direnv allow` does:

- In a terminal, `claustrum run` lists what the file changes, marks with `!` everything that
  reaches the host, and asks. Without a terminal (`serve` started by another MCP client, CI)
  it refuses.
- `claustrum trust` records the file after showing the same list, and `--revoke` forgets it.
  The record is the file's SHA-256 in the user state directory, so any change, such as a
  `git pull`, asks again.
- A file passed with `--config` (or `CLAUSTRUM_CONFIG`), the user configuration and the
  built-in defaults need no confirmation.

The check applies to every `claustrum` command started in a directory with an untrusted
`claustrum.toml`, not only to `run`.

`[sandbox] workspace` is always flagged with `!`: it replaces the project directory at
`/workspace` with another host directory, read/write, so `workspace = "~"` alone hands the
guest your whole home directory.

## Setups without a terminal

When `claustrum serve` is started by another MCP client or in CI, there is nobody to ask.
Either run `claustrum trust` once beforehand in the same directory, or pass the file
explicitly with `--config` / `CLAUSTRUM_CONFIG`, which counts as your own choice.

## Flags `[claude] args` may not contain

Independently of trust, `[claude] args` may not contain flags that would override what
`claustrum run` sets up (`--tools`, `--allowedTools`, `--mcp-config`, `--strict-mcp-config`,
`--disallowedTools`, `--permission-mode`, `--dangerously-skip-permissions`, `--settings`,
`--setting-sources`, `--add-dir`, `--plugin-dir`, `--system-prompt`,
`--append-system-prompt` (use `[claude] system_prompt`), `--permission-prompt-tool`, ..., also in their
kebab-case and `--flag=value` forms), in any configuration file. Use
[`[claude] tools`](reference.md#claude) for built-in tools and the command line for the rest.
