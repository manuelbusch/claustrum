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
                                        │  │  network: allowlist gate, off by   │  │
                                        │  │  default                           │  │
                                        │  └────────────────────────────────────┘  │
                                        └──────────────────────────────────────────┘
```

1. **Sandbox.** Claustrum embeds the Wasmer runtime and boots a WASIX environment for every
   command. WASIX extends WASI with the POSIX features real tools need (threads, pipes,
   fork/exec), so `bash` and `coreutils` compiled to WebAssembly run unmodified. The guest sees
   an in-memory root with the project directory mounted read/write at `/workspace`. Everything
   else on the host is invisible, and networking is off unless the configuration allows
   specific destinations (see [Network](#network)).
2. **Tools.** The MCP server offers `Bash`, `Read`, `Write`, `Edit`, `Glob` and `Grep`, named
   and shaped like Claude Code's built-in tools so the model needs no adaptation. `Bash` runs a
   guest process; the file tools are implemented natively in Rust on top of the same virtual
   file system, so they are fast and never spawn a process.
3. **Claude Code.** `claustrum run` launches `claude` with `--tools ""` (no built-in tools),
   `--strict-mcp-config`, the Claustrum server as its only MCP server, `mcp__claustrum__*`
   pre-approved, and a system prompt that explains the sandbox layout. From Claude's point of
   view nothing changes: it still has file and shell access, but only inside the sandbox.

### Two sandbox layers

WASIX is the first layer: guest code only sees what the runtime hands it. Everything
Claustrum itself runs natively on the host is the attack surface behind it: the Wasmer
runtime and its JIT, the native file tools, and host actions, which start real host
programs. A bug in the runtime would otherwise land an attacker directly in your user
account, and an action like `cargo test` runs code from the workspace.

So Claustrum wraps those processes in an operating-system sandbox as a second layer. Because
macOS Seatbelt profiles cannot be nested (a confined process cannot confine its children
more tightly), `claustrum serve` splits into a small unconfined **broker** and a confined
**worker**:

```
claude ──stdio/MCP──▶ worker: Wasmer + tools            [OS profile: workspace, cache, log;
                        │                                 no exec, no other files, no
                        │ socket: "run action X"          network unless configured]
                        ▼
                     broker: config, action proxy ──▶ action: host program
                     (never runs guest code)           [own OS profile per run]
```

- The **worker** hosts the Wasmer runtime and serves MCP on the inherited stdio. It may
  read its binary, the packages and the configuration, write the workspace, extra mounts,
  the module cache, the network log and a private temporary directory, and nothing else.
  It cannot start programs. It has no network access unless the network mode or online
  package loading needs it. The configuration files and their directories stay
  read-only, and credential stores (`~/.ssh`, `~/.aws`, `~/.gnupg`, keychains, browser
  profiles, ...) stay unreadable even when they lie inside a mount.
- The **broker** loads the configuration, runs the action proxy and starts every action on
  the worker's request. It receives only an action name plus raw inputs and validates them
  against its own copy of the definitions, so a compromised worker cannot run anything
  that was not declared.
- Every **action** runs in its own profile. It may read the file system except the
  credential stores, and write only the workspace, a fresh `$TMPDIR` and the directories
  listed in the action's `writable`. The configuration is never writable. Network access
  is limited to Claustrum's proxy on `localhost`, so the allowlist is enforced, not just
  suggested; in `host` network mode, network access is unrestricted.

`[sandbox] confinement` selects how strict this is: `"best-effort"` (default) confines
where the platform supports it and warns loudly where not, `"required"` refuses to start
without it, and `"off"` runs everything in one process as a single layer. `claustrum run` and
`serve` print the active state on start. Additional unreadable paths go in
`[sandbox] deny_read`.

| Platform | Backend | Status |
| --- | --- | --- |
| macOS | Seatbelt (`sandbox-exec`, generated SBPL profile) | implemented |
| Linux | Landlock (file system, TCP) + seccomp | planned; runs unconfined with a warning until then |
| Windows | AppContainer + Job object | planned; runs unconfined with a warning until then |

Stronger isolation (a Linux micro-VM through Virtualization.framework, Firecracker or
Hyper-V) is possible later and would also allow running real Linux toolchains inside it.

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
| `claustrum network report [--workspace DIR] [--all]` | Summarise refused and audit-flagged connections and suggest `allow` entries. |

Global options: `--config FILE` (or `CLAUSTRUM_CONFIG`), `--packages-dir DIR` (or
`CLAUSTRUM_PACKAGES_DIR`). Logging goes to stderr and is controlled by `CLAUSTRUM_LOG`
(e.g. `CLAUSTRUM_LOG=debug`).

### Configuration

Claustrum looks for `claustrum.toml` in the current directory, then for `config.toml` in the
user configuration directory. See [`claustrum.example.toml`](claustrum.example.toml) for all
options: workspace, network access, timeouts, packages, extra mounts, and which built-in
Claude tools (if any) to keep.

Inside the sandbox every configuration file Claustrum could load is read-only:
`claustrum.toml` in the workspace (even when it does not exist yet), the default search
paths and the file passed with `--config`, in every mount that contains them. The guest
can read them, but writing, truncating, creating, deleting, renaming or replacing them is
refused, as is renaming or removing a directory that contains one. The check runs on the
host below every tool, resolves symlinks and hard links, and compares names
case-insensitively on macOS. Change the configuration from outside the sandbox.

### Network

Every connection Claustrum starts, from the guest and from [host actions](#host-actions),
passes one gate that allows only declared destinations:

```toml
[network]
mode = "allowlist"      # disabled (default) | allowlist | audit | host
allow = [
  "crates.io",          # port defaults to 443
  "*.crates.io",        # subdomains only, not crates.io itself
  "github.com:443",
  "pypi.org:80,443",
  "127.0.0.1:5432",     # local and private addresses need an explicit IP entry
]
```

How the gate decides:

- **Names are checked when they are resolved.** Only allowed names resolve, and the
  addresses they resolve to may then be used on the ports of the matching entry. A literal
  IP that was never resolved from an allowed name is refused, so allowing `github.com`
  does not open arbitrary addresses.
- **Local and private addresses need an explicit entry.** Loopback, RFC 1918, link-local
  (including the cloud metadata address `169.254.169.254`), CGNAT and IPv6 ULA are refused
  even when an allowed name resolves to them, which defeats DNS rebinding and keeps the
  guest away from services on your machine.
- **Only outgoing TCP is supported.** UDP, listening sockets and TCP sockets bound before
  connecting are refused, because their later destinations would not pass the gate. DNS
  resolution happens on the host and needs no UDP in the guest.
- **Guest traffic cannot bypass the gate.** The guest has no sockets of its own; every
  socket call goes through the WASIX runtime, which Claustrum wraps.
- **Host actions go through a local proxy.** Actions are started with `HTTP_PROXY`,
  `HTTPS_PROXY` and the variants cargo and npm read, pointing at a proxy on `127.0.0.1`
  that applies the same allowlist and checks the host name of every `CONNECT`. It requires
  a per-session credential, so other local processes cannot use it. Confined actions
  cannot reach anything but the proxy (see [Two sandbox layers](#two-sandbox-layers)), so
  git over SSH, raw sockets and programs that ignore the proxy variables simply fail.
  Unconfined actions (`confine = "none"`, or no OS confinement on the platform) can
  ignore the proxy.

Refused connections are logged and appended to the tool result as
`[network: refused tcp example.com:443 (example.com is not in the allowlist)]`, so Claude
can ask for the destination instead of trying workarounds. `claustrum run` and `serve`
print the active mode on start.

To build an allowlist, run a session with `mode = "audit"`: everything is reachable, and
everything the allowlist would refuse is logged. `claustrum network report` then groups
the log by destination and prints ready-to-paste `allow` entries; review them before
adding, the report cannot tell a needed download from an unwanted one. The log is JSON
lines in the user state directory, one file per workspace, or wherever `[network] log`
points.

Known limit: the guest gate decides on address and port, not on the TLS server name. Two
names served from the same CDN address share their grant. The action proxy sees the host
name of each `CONNECT` and checks it.

`[sandbox] network = "disabled"` or `"host"` still works as a shorthand for the mode; the
former Wasmer ruleset strings are no longer accepted.

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

### Host actions

Sometimes a task needs something that only exists on the host: the Rust toolchain, a
deploy script, a formatter. Instead of loosening the sandbox, `claustrum.toml` can declare
**actions**: fixed host commands that Claude may *trigger*, in the spirit of a CI job.

```toml
[[actions.action]]
name = "test"
description = "Run the whole Rust test suite"
command = ["cargo", "test", "--workspace"]
timeout_secs = 600

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

Inside the sandbox the actions appear as the `host` command (`host` lists them, `host test`
triggers one, `host test-crate claustrum-cli` or `host test-crate crate=claustrum-cli`
passes inputs), and as the `Action` MCP tool whose description carries the same list.

What holds, by construction:

- **No free-form commands.** The argv is fixed in the configuration, which is read-only
  inside the sandbox. No shell is involved on the host; `command[0]` is resolved once at
  startup (absolute path or a bare name on `PATH`).
- **Every input is validated** before it is substituted into a single argv element:
  `pattern` (anchored regex), `choices`, `path` (must resolve inside the workspace, also
  through symlinks) or `integer` (with `min`/`max`). Values are limited in length, may not
  contain control characters and may not start with `-` unless `allow_leading_dash` is
  set. An action without inputs refuses any argument.
- **The process is contained in time and output.** It starts with a clean environment
  (`PATH`, `HOME`, `LANG`, plus `env` and `env_passthrough`), a closed stdin, a working
  directory inside the workspace and its own process group. The action's timeout or a
  killed Bash call ends it with SIGKILL, output is capped, and one action runs at a time.
- **The configuration stays what it was.** The protected files are snapshotted before and
  restored after every action, so an action cannot loosen the sandbox for the next run.

- **The program is confined** by the OS sandbox (see
  [Two sandbox layers](#two-sandbox-layers)): writes stay in the workspace, `$TMPDIR` and
  the action's `writable` list, credential stores are unreadable, and the network is only
  reachable through the proxy. An action that needs a package cache declares it, e.g.
  `writable = ["~/.cargo/registry"]` for cargo. Never make a directory writable that the
  host executes from later (`~/.cargo/bin`, `~/.cargo/config.toml`, shell profiles). A
  single action can opt out with `confine = "none"`.

**Use with care.** Every action is a hole in the sandbox that you cut on purpose, and the
validation only guards the edges you declared. Confinement narrows what the program can do,
but it still runs as your user with read access to most of the file system, so these ways
around the sandbox remain:

- **Trigger-only is not safe by itself.** Many programs execute configuration or scripts
  from the workspace, which Claude can write: `cargo` honours `.cargo/config.toml`
  (`rustc-wrapper`, `runner`), `build.rs` and proc macros; `git` honours `.git/config`
  (`core.hooksPath`, `core.fsmonitor`, aliases); `npm` runs `package.json` scripts;
  `make` runs the Makefile. Confined, such code can read your files (except credential
  stores) and write the workspace and the declared caches. Unconfined, it is plain code
  execution on the host.
- **Validated inputs can still be interpreted.** A pattern only limits characters; the
  program decides what they mean. `ext::sh -c …` is a valid git URL, `user@host:` is a
  remote for scp and rsync, `@file` reads a file for curl, `key=value` after `git -c` or
  `cargo --config` changes behaviour. A leading `-` is refused, these are not. Keep patterns
  to the characters the program needs and check how it treats them.
- **Placeholders must never reach an interpreter.** `sh -c "… {x}"` or `python -c` would
  parse the value again on the host, so Claustrum refuses placeholders in those positions.
  Put the code in a script and pass the input as an argument.
- **Environment values are inputs too.** A placeholder in `env` (`RUSTFLAGS = "{flags}"`)
  or an `env_passthrough` of `PATH`, `LD_PRELOAD`, `DYLD_INSERT_LIBRARIES`, `GIT_*` and the
  like controls what the program loads or runs.
- **Path inputs are checked before the program opens them.** Claude Code can run tools in
  parallel, so a file could in principle be replaced by a symlink between the check and
  the open. Do not rely on `path` inputs to keep a program away from host files.
- **What the proxy lets through leaves the machine.** A confined action can send what it
  reads to any allowed destination. Keep the allowlist short.
- **Side effects leave the sandbox.** `git push`, `deploy` or `npm publish` ship whatever
  the workspace contains. Only the Claustrum configuration files are restored afterwards,
  not anything else an action may write on the host.

Claustrum refuses the clear cases at startup (placeholders after `sh -c` and friends) and
logs a warning for the rest: programs known to execute workspace code, patterns such as
`.*`, placeholders in `env`, forwarded loader variables. `claustrum run` and `serve` also
print which actions are enabled. Rules of thumb: declare no inputs when you can, make
patterns as narrow as possible, and only declare a command you would let Claude run on the
host directly. Without OS confinement (`confine = "none"`, `confinement = "off"`, or a
platform without a backend yet), an action is as trusted as the host command behind it.

## Sandbox layout

| Guest path | Backing | Notes |
| --- | --- | --- |
| `/workspace` | host project directory | read/write, this is the working directory |
| `/workspace/claustrum.toml` | host file | read-only, see [Configuration](#configuration) |
| `/tmp`, `/home/claude` | in-memory | persist for the lifetime of the server |
| `/bin`, `/usr/bin` | package commands | populated from the loaded `.webc` files |
| `/etc/claustrum/profile.sh` | in-memory | sourced by every bash via `BASH_ENV` |
| `/.claustrum/cmd/<name>` | host process | request channel of a host command (see below) |

Guest commands get a fixed environment (`HOME=/home/claude`, `PATH=/usr/local/bin:/bin:/usr/bin`,
`TERM=dumb`) plus anything set under `[sandbox.env]`. Because WASIX reports stdio as a
terminal, the profile disables colours and pagers (`NO_COLOR`, `PAGER=cat`, `jq -M`) so that
captured output stays clean. Each `Bash` call is a fresh process; only the file system
persists between calls.

Host commands such as `host` are a small WASI shim (`shim/`, built by
`scripts/build-shim.sh` and committed as `crates/claustrum-sandbox/assets/hostcmd.wasm`)
registered under the command name. It writes its arguments to `/.claustrum/cmd/<name>`
and reads the result back; the host runs the command while serving that read, so pipes
and redirections in bash behave as usual.

## Development

```
crates/
  claustrum-sandbox/   runtime, mounts, packages, process execution, native tools,
                       host commands (hostcmd.rs), host actions (action/) and the
                       network gate and action proxy (net/)
shim/                  guest-side WASI shim for host commands (wasm32-wasip1)
  claustrum-mcp/       MCP server (rmcp) exposing the tools
  claustrum-cli/       `claustrum` binary: run, serve (broker + confined worker), pkg,
                       configuration
  claustrum-confine/   OS confinement profiles and backends (Seatbelt on macOS)
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
- UDP to explicit addresses; TLS server name checks in the guest gate.
- OS confinement on Linux (Landlock + seccomp) and Windows (AppContainer + Job objects).
- Brush (a bash-compatible shell written in Rust) compiled to WASIX as an alternative shell.
- `.gitignore`-aware Glob/Grep, persistent working directory across `cd` in Bash calls,
  PTY emulation for interactive tools.

## License

Not yet decided. The bash and coreutils packages are GPL-licensed and are downloaded at
runtime; they are not part of this repository.
