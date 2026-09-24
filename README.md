# Claustrum

Claustrum is a sandbox for [Claude Code](https://claude.com/claude-code). It runs a POSIX
environment and a curated set of tools inside [Wasmer](https://wasmer.io/) using
[WASIX](https://wasix.org/), and exposes them to Claude through an
[MCP](https://modelcontextprotocol.io/) server. Claude Code is started with its built-in tools
removed and uses Claustrum's sandboxed tools instead, so nothing Claude does touches the host
system outside the project directory.

> **Status:** early. The core loop works end to end (bash + coreutils in the sandbox, native
> file tools, Claude Code driving them over MCP), but the tool set is small and the
> configuration format may still change.

## How it works

```
claude (host)                          claustrum (host process, Rust)
  --tools ""                            ┌──────────────────────────────────────────┐
  --mcp-config  ── stdio/MCP ─────────▶ │ MCP: Bash, Read, Write, Edit, Glob, Grep │
                                        │        │            │                    │
                                        │        ▼            ▼                    │
                                        │  guest process    native tools           │
                                        │  (WasiRunner)     (virtual-fs directly)  │
                                        │        │            │                    │
                                        │  ┌─────▼────────────▼─────────────────┐  │
                                        │  │ WASIX runtime (wasmer-wasix)       │  │
                                        │  │  /workspace  ← host dir (rw)       │  │
                                        │  │  /bin, /usr/bin ← .webc packages   │  │
                                        │  │  /tmp, /home/claude ← in-memory    │  │
                                        │  │  network: disabled by default      │  │
                                        │  └────────────────────────────────────┘  │
                                        └──────────────────────────────────────────┘
```

1. **Sandbox.** Claustrum embeds the Wasmer runtime and boots a WASIX environment for every
   command. WASIX extends WASI with the POSIX features real tools need (threads, pipes,
   fork/exec), so `bash` and `coreutils` compiled to WebAssembly run unmodified. The guest sees
   an in-memory root with the project directory mounted read/write at `/workspace`. Everything
   else on the host is invisible, and networking is off unless enabled in the configuration.
2. **Tools.** The MCP server offers `Bash`, `Read`, `Write`, `Edit`, `Glob` and `Grep`, named
   and shaped like Claude Code's built-in tools so the model needs no adaptation. `Bash` runs a
   guest process; the file tools are implemented natively in Rust on top of the same virtual
   file system, so they are fast and never spawn a process.
3. **Claude Code.** `claustrum run` launches `claude` with `--tools ""` (no built-in tools),
   `--strict-mcp-config`, the Claustrum server as its only MCP server, `mcp__claustrum__*`
   pre-approved, and a system prompt that explains the sandbox layout. From Claude's point of
   view nothing changes: it still has file and shell access, but only inside the sandbox.

## Why

Claude Code's built-in tools run with the full privileges of the user who started it.
Permission prompts help, but they rely on the user reviewing every command. Claustrum replaces
that trust boundary with a technical one: the only things Claude can affect are the files that
were explicitly mounted into the sandbox. This makes it practical to let Claude work
autonomously on a project without exposing the rest of the machine. The boundary also covers
Claustrum's own configuration: it usually sits in the project directory, but Claude can only
read it, never loosen its sandbox for the next run.

## Getting started

Requirements: Rust 1.95+ (edition 2024) and a working `claude` on your `PATH`.

```sh
cargo install --path crates/claustrum-cli   # installs the `claustrum` binary

# Download the default packages (bash, coreutils) from the Wasmer registry
claustrum pkg sync

# Start Claude Code in the current project, sandboxed
cd ~/my-project
claustrum run
```

Everything after `--` is passed through to `claude`, for example a headless prompt:

```sh
claustrum run --permission-mode acceptEdits -- -p "Add a README for this project"
```

Use `claustrum run --dry-run` to print the exact `claude` command line.

### Commands

| Command | Purpose |
| --- | --- |
| `claustrum run [--workspace DIR] [--permission-mode MODE] [-- claude args…]` | Launch Claude Code against the sandbox. |
| `claustrum serve [--workspace DIR]` | Serve the tools over MCP on stdio (what `run` starts under the hood). |
| `claustrum pkg sync [--force]` | Download the configured packages and precompile them. |
| `claustrum pkg add <spec>` | Download one package, e.g. `python/python`. |
| `claustrum pkg add-wasm <name> <file.wasm> [--alias cmd]` | Register a self-built WASIX binary as a package. |
| `claustrum pkg list` / `pkg commands` | Show installed packages and the commands they provide. |
| `claustrum pkg precompile` | Compile all packages into the module cache ahead of time. |

Global options: `--config FILE` (or `CLAUSTRUM_CONFIG`), `--packages-dir DIR` (or
`CLAUSTRUM_PACKAGES_DIR`). Logging goes to stderr and is controlled by `CLAUSTRUM_LOG`
(e.g. `CLAUSTRUM_LOG=debug`).

### Configuration

Claustrum looks for `claustrum.toml` in the current directory, then for `config.toml` in the
user configuration directory. See [`claustrum.example.toml`](claustrum.example.toml) for all
options: workspace, network policy, timeouts, packages, extra mounts, and which built-in
Claude tools (if any) to keep.

Inside the sandbox every configuration file Claustrum could load is read-only:
`claustrum.toml` in the workspace (even when it does not exist yet), the default search
paths and the file passed with `--config`, in every mount that contains them. The guest
can read them, but writing, truncating, creating, deleting, renaming or replacing them is
refused, as is renaming or removing a directory that contains one. The check runs on the
host below every tool, resolves symlinks and hard links, and compares names
case-insensitively on macOS. Change the configuration from outside the sandbox.

### Packages

Packages are `.webc` files from the [Wasmer registry](https://wasmer.io/explore). `pkg sync`
stores them in the user data directory and records each package's identity in a `.webc.id`
file next to it, which is how dependencies between packages (bash depends on coreutils) resolve
offline. Files obtained some other way can be given an explicit `id` in the configuration.

The default set is `wasmer/bash`, `wasmer/coreutils`, `python/python` (CPython 3.13 with the
standard library, about 60 MB) and `syrusakbary/jq` (jq 1.6). Python takes close to a minute
to compile on first use, which is why `pkg sync` precompiles everything into the module cache.

Self-built tools can be added without the `wasmer` CLI: compile a Rust program with
[`cargo wasix`](https://github.com/wasix-org/cargo-wasix) and register the result with
`claustrum pkg add-wasm <name> <file.wasm>`, which writes a directory package (`wasmer.toml`
plus the module) into the packages directory. [`scripts/build-jaq.sh`](scripts/build-jaq.sh)
does this for [jaq](https://github.com/01mf02/jaq), a current jq implementation in Rust.

**git is not available yet.** No WASIX build of git is published anywhere; the only known
recipe is the [wasinix](https://github.com/wasix-org/wasinix) Nix flake, which builds on
x86_64 Linux only and needs a patched runtime. gitoxide does not target WASIX either. Until
that changes, git operations have to happen on the host.

## Sandbox layout

| Guest path | Backing | Notes |
| --- | --- | --- |
| `/workspace` | host project directory | read/write, this is the working directory |
| `/workspace/claustrum.toml` | host file | read-only, see [Configuration](#configuration) |
| `/tmp`, `/home/claude` | in-memory | persist for the lifetime of the server |
| `/bin`, `/usr/bin` | package commands | populated from the loaded `.webc` files |
| `/etc/claustrum/profile.sh` | in-memory | sourced by every bash via `BASH_ENV` |

Guest commands get a fixed environment (`HOME=/home/claude`, `PATH=/usr/local/bin:/bin:/usr/bin`,
`TERM=dumb`) plus anything set under `[sandbox.env]`. Because WASIX reports stdio as a
terminal, the profile disables colours and pagers (`NO_COLOR`, `PAGER=cat`, `jq -M`) so that
captured output stays clean. Each `Bash` call is a fresh process; only the file system
persists between calls.

## Development

```
crates/
  claustrum-sandbox/   runtime, mounts, packages, process execution, native tools
  claustrum-mcp/       MCP server (rmcp) exposing the tools
  claustrum-cli/       `claustrum` binary: run, serve, pkg, configuration
```

```sh
# Put the packages where the tests and the spike example expect them
mkdir -p packages && claustrum --packages-dir packages pkg sync

cargo test --workspace
cargo run -p claustrum-sandbox --example spike -- . 'ls -la; echo hi | tr a-z A-Z'
```

The integration tests skip themselves when the packages are missing.

## Roadmap

- git inside the sandbox (blocked on a WASIX build, see Packages); sed/awk/grep as WASIX builds.
- Expose the native tools inside the guest as well (so `grep` in a Bash call hits the fast
  path), via Wasmer's builtin-command mechanism.
- Network allowlists per project.
- Brush (a bash-compatible shell written in Rust) compiled to WASIX as an alternative shell.
- `.gitignore`-aware Glob/Grep, persistent working directory across `cd` in Bash calls,
  PTY emulation for interactive tools.

## License

Not yet decided. The bash and coreutils packages are GPL-licensed and are downloaded at
runtime; they are not part of this repository.
