# Your first session

Start Claude Code in a project, sandboxed:

```sh
cd ~/my-project
claustrum run
```

`claustrum run` launches `claude` with its built-in file and shell tools removed and
Claustrum as its only MCP server. The project directory is mounted read/write at
`/workspace` inside the sandbox and is the working directory for every command. On every
start, Claustrum prints the active confinement, network mode and enabled host actions.

## When the project has a `claustrum.toml`

If the project contains a `claustrum.toml`, Claustrum does not use it until you have accepted
its exact content. In a terminal, `claustrum run` lists what the file changes, marks with `!`
everything that reaches the host, and asks. You can also review and accept it up front with
`claustrum trust`. See [Trusting a project's configuration](configuration/trust.md).

## Passing arguments to Claude Code

Everything after `--` is passed through to `claude`, for example a headless prompt:

```sh
claustrum run --permission-mode acceptEdits -- -p "Add a README for this project"
```

`--permission-mode` is an option of `claustrum run` itself, because it is one of the flags
Claustrum sets up; `plan` starts in [plan mode](plan-mode.md).

To see the exact `claude` command line without starting anything:

```sh
claustrum run --dry-run
```

## A different workspace

`--workspace DIR` sandboxes another directory than the current one:

```sh
claustrum run --workspace ~/other-project
```

The configuration is still looked up in the *current* directory (see
[Configuration](configuration/index.md)), so either `cd` into the project first or pass its
file with `--config`.

## What to do next

- Claude has no network access by default. If the project needs downloads, open specific
  destinations with an [allowlist](network/index.md).
- Tools that only exist on the host, such as a compiler toolchain or git, can be made
  available as [host actions](host-actions/index.md).
- All command-line options are listed in [Commands](commands.md).
