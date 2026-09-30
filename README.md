<p align="center">
  <img src="docs/assets/claustrum-logo.svg" alt="Claustrum" width="440">
</p>

<p align="center">
  <strong>Give your programming agents the freedom to work while keeping all boundaries firmly under control.</strong><br>
  More autonomy for the agents, full control for you — all within a sandbox they can’t escape from.
</p>

**A sandbox for [Claude Code](https://claude.com/claude-code).** Claustrum runs a POSIX
environment and a curated set of tools inside [Wasmer](https://wasmer.io/) using
[WASIX](https://wasix.org/) and exposes them to Claude through an
[MCP](https://modelcontextprotocol.io/) server. Claude Code starts with its built-in tools
removed and uses Claustrum's sandboxed tools instead, so nothing Claude does touches the host
outside the project directory.

**Highlights**

- **Two sandbox layers.** Commands run as WebAssembly in WASIX, and Claustrum's own host
  processes are additionally confined by the operating system (Seatbelt on macOS;
  bubblewrap, Landlock and seccomp on Linux), which also keeps credential stores such as
  `~/.ssh` unreadable.
- **Trust before use.** A `claustrum.toml` that comes with a project is only used after you
  have reviewed and accepted its exact content, like `direnv allow`; any change asks again.
- **A sandbox Claude cannot loosen.** The configuration and Claude Code's
  `.claude/settings.json` are read-only inside the sandbox, so Claude can neither widen its
  permissions nor plant hooks that run on the host.
- **Network by allowlist.** Nothing is reachable by default. Allowed destinations are checked
  per host name and port, private addresses and cloud metadata endpoints need an explicit
  entry, and an audit mode plus `claustrum network report` build the allowlist for you.
- **Host actions instead of holes.** Tools that only exist on the host (cargo, git, a deploy
  script) can be declared as fixed commands with validated inputs, run confined, with a
  timeout and through the network proxy.
- **Bounded commands.** Every command runs with a timeout and limits on output, processes
  and memory; a runaway command is killed with its whole process tree.

> **Status:** early. The core loop works end to end (bash + coreutils + python + jq in the
> sandbox, native file tools, Claude Code driving them over MCP, OS confinement on macOS and
> Linux), but the tool set is small and the configuration format may still change.

```
┌─────────────────────────────────────────────────────────────────────────────┐
│  Your machine                                                               │
│                                                                             │
│   claude ─────────── MCP over stdio ──────────▶  claustrum                  │
│   --tools AskUserQuestion,…                       │                         │
│   (no built-in Bash, Read, Write, ...)            │  Bash  Read  Write      │
│                                                   │  Edit  Glob  Grep       │
│                                                   ▼  Action                 │
│                                    ┌─────────────────────────────────┐      │
│                                    │  WASIX sandbox                  │      │
│                                    │   /workspace  ◀── project dir   │      │
│                                    │   /bin, /usr/bin ◀── .webc pkgs │      │
│                                    │   /tmp, /home/claude  in memory │      │
│                                    │   network: allowlist gate       │      │
│                                    └─────────────────────────────────┘      │
│                                                                             │
│   everything else on the host: invisible                                    │
└─────────────────────────────────────────────────────────────────────────────┘
```

## Contents

- [Why](#why)
- [Quick start](#quick-start)
- [Documentation](#documentation)
- [Development](#development)
- [Roadmap](#roadmap)
- [License](#license)

## Why

Claude Code's built-in tools run with the full privileges of the user who started it.
Permission prompts help, but they rely on the user reviewing every command. Claustrum
replaces that trust boundary with a technical one: the only things Claude can affect are the
files that were explicitly mounted into the sandbox. This makes it practical to let Claude
work autonomously on a project without exposing the rest of the machine.

The boundary also covers Claustrum's own configuration. It usually sits in the project
directory, but Claude can only read it, never loosen its own sandbox for the next run. A
second, operating-system sandbox (Seatbelt on macOS; bubblewrap, Landlock and seccomp on
Linux) contains everything Claustrum runs natively on the host.

## Quick start

Requirements: Rust 1.95+ (edition 2024) and a working `claude` on your `PATH`.

```sh
cargo install --path crates/claustrum-cli   # installs the `claustrum` binary

# Download the default packages (bash, coreutils, python, jq) and precompile them
claustrum pkg sync

# Start Claude Code in the current project, sandboxed
cd ~/my-project
claustrum run
```

## Documentation

The **[user guide](user_guide/src/introduction.md)** covers everything about using
Claustrum:

- [Installation](user_guide/src/installation.md) and
  [your first session](user_guide/src/getting-started.md)
- [How it works](user_guide/src/how-it-works.md) and
  [what Claude sees](user_guide/src/what-claude-sees.md) inside the sandbox
- [Configuration](user_guide/src/configuration/index.md), including
  [trust](user_guide/src/configuration/trust.md),
  [protected files](user_guide/src/configuration/protected-files.md) and the
  [reference](user_guide/src/configuration/reference.md)
- [Network](user_guide/src/network/index.md), [packages](user_guide/src/packages.md) and
  [host actions](user_guide/src/host-actions/index.md)
- [Plan mode](user_guide/src/plan-mode.md) and
  [subagents and workflows](user_guide/src/subagents.md)
- The security model: [two sandbox layers](user_guide/src/security/index.md) and
  [OS confinement](user_guide/src/security/confinement.md)
- [Commands](user_guide/src/commands.md) and
  [troubleshooting](user_guide/src/troubleshooting.md)

The guide is an [mdbook](https://rust-lang.github.io/mdBook/); `mdbook serve user_guide`
renders it locally. [`claustrum.example.toml`](claustrum.example.toml) documents every
configuration key, and [`examples/rust.claustrum.toml`](examples/rust.claustrum.toml) is a
complete setup for a Rust project.

## Development

```
crates/
  claustrum-sandbox/   runtime, mounts, packages, process execution, native tools,
                       protected files (protect.rs), plan store (plans.rs),
                       host commands (hostcmd.rs), host actions (action/), network
                       gate and action proxy (net/)
  claustrum-mcp/       MCP server (rmcp) exposing the tools
  claustrum-cli/       `claustrum` binary: run, serve (broker + confined worker),
                       pkg, network, plans, trust, configuration
  claustrum-confine/   OS confinement profiles and backends (Seatbelt on macOS,
                       bubblewrap + Landlock + seccomp on Linux), test helper binary
shim/                  guest-side WASI shim for host commands (wasm32-wasip1), built by
                       scripts/build-shim.sh and committed as
                       crates/claustrum-sandbox/assets/hostcmd.wasm
user_guide/            the user guide (mdbook)
```

```sh
# Put the packages where the tests and the spike example expect them
mkdir -p packages && claustrum --packages-dir packages pkg sync

cargo test --workspace
cargo run -p claustrum-sandbox --example spike -- . 'ls -la; echo hi | tr a-z A-Z'
```

The integration tests skip themselves when the packages are missing. The confinement tests
run the real binary and check that a simulated runtime escape stays inside the OS profile.

How host commands work: a small WASI shim is registered under each command name (`host`).
It writes its arguments to `/.claustrum/cmd/<name>` and reads the result back; the host runs
the command while serving that read, so pipes and redirections in bash behave as usual.

Environment switches for testing: `CLAUSTRUM_NO_BWRAP=1` forces the Landlock-only backend on
Linux, `CLAUSTRUM_CONFINE_HELPER` points to another confinement helper binary (an absolute
path; its use is announced on stderr).

## Roadmap

- git inside the sandbox (blocked on a WASIX build, see
  [Packages](user_guide/src/packages.md#git-is-not-available-yet)); sed/awk/grep as WASIX
  builds.
- Expose the native tools inside the guest as well, so `grep` in a Bash call hits the fast
  path.
- UDP to explicit addresses; TLS server name checks in the guest gate.
- OS confinement on Windows (AppContainer + Job objects).
- Brush (a bash-compatible shell written in Rust) compiled to WASIX as an alternative shell.
- `.gitignore`-aware Glob/Grep, persistent working directory across `cd` in Bash calls, PTY
  emulation for interactive tools.

## License

Claustrum is licensed under the [MIT License](LICENSE).

The packages that run inside the sandbox are downloaded at runtime from the Wasmer registry
and are not part of this repository; they keep their own licenses (bash, for example, is
GPL).
