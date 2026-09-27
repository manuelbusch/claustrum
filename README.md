# Claustrum

**A sandbox for [Claude Code](https://claude.com/claude-code).** Claustrum runs a POSIX
environment and a curated set of tools inside [Wasmer](https://wasmer.io/) using
[WASIX](https://wasix.org/) and exposes them to Claude through an
[MCP](https://modelcontextprotocol.io/) server. Claude Code starts with its built-in tools
removed and uses Claustrum's sandboxed tools instead, so nothing Claude does touches the host
outside the project directory.

> **Status:** early. The core loop works end to end (bash + coreutils + python + jq in the
> sandbox, native file tools, Claude Code driving them over MCP, OS confinement on macOS and
> Linux), but the tool set is small and the configuration format may still change.

```
┌─────────────────────────────────────────────────────────────────────────────┐
│  Your machine                                                               │
│                                                                             │
│   claude ─────────── MCP over stdio ──────────▶  claustrum                  │
│   --tools ""                                      │                         │
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
- [How it works](#how-it-works)
- [What Claude sees](#what-claude-sees)
- [Two sandbox layers](#two-sandbox-layers)
- [Getting started](#getting-started)
- [Configuration](#configuration)
- [Network](#network)
- [Packages](#packages)
- [Host actions](#host-actions)
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
directory, but Claude can only read it, never loosen its own sandbox for the next run.

## How it works

Claustrum is three things stacked on each other:

| Layer | What it is | What it gives you |
| --- | --- | --- |
| **Sandbox** | Wasmer runtime booting a WASIX environment per command | `bash`, `coreutils`, `python`, `jq` run unmodified as WebAssembly. The guest sees an in-memory root with the project mounted read/write at `/workspace`. Networking is off unless configured. |
| **Tools** | An MCP server (`claustrum serve`) | `Bash`, `Read`, `Write`, `Edit`, `Glob`, `Grep`, named and shaped like Claude Code's own tools, plus `Action` for declared host commands. |
| **Launcher** | `claustrum run` | Starts `claude` with `--tools ""`, `--strict-mcp-config`, Claustrum as its only MCP server, the tools pre-approved and a system prompt describing the sandbox. |

From Claude's point of view nothing changes: it still has file and shell access, only inside
the sandbox.

### A `Bash` call, step by step

```
 Claude                claustrum (host)                          WASIX guest
 ──────                ────────────────                          ───────────
 Bash("ls | wc -l")
   │
   ├─ MCP request ───▶ spawn WasiRunner: bash -c "ls | wc -l"
   │                     mounts: /workspace (host dir, rw)
   │                             /tmp, /home/claude (memory)
   │                             /.claustrum (host-command channel)
   │                     env: HOME=/home/claude PATH=/bin:...  ──▶  bash
   │                     stdout/stderr → bounded capture             │ fork/exec
   │                     timeout → SIGKILL to the whole tree         ├─ ls  ──▶ /workspace
   │                                                                 └─ wc
   │                   ◀──────────────── exit code, captured output ──┘
   ◀─ result: output, [exit code N], [network: refused ...] notes
```

The file tools skip the guest entirely: `Read`, `Write`, `Edit`, `Glob` and `Grep` are native
Rust on top of the same virtual file system, so they are fast and never spawn a process. Each
`Bash` call is a fresh process; only the file system persists between calls.

## What Claude sees

### Tools

| Tool | Parameters (same as Claude Code) | Implementation |
| --- | --- | --- |
| `Bash` | `command`, `timeout` (ms, max 600 000), `description` | `bash -c` in a fresh WASIX process |
| `Read` | `file_path`, `offset`, `limit` | native, `cat -n` style output |
| `Write` | `file_path`, `content` | native, creates parent directories |
| `Edit` | `file_path`, `old_string`, `new_string`, `replace_all` | native, exact string replacement |
| `Glob` | `pattern`, `path` | native, newest first, skips `.git`, `node_modules`, `target` |
| `Grep` | `pattern`, `path`, `glob`, `output_mode`, `-i`, `-C`, `head_limit` | native, ripgrep engine |
| `Action` | `name`, `inputs` | only when [host actions](#host-actions) are configured |
| `WritePlan`, `EditPlan` | as `Write` and `Edit` | plan files only, see [Plan mode](#plan-mode) |

`AskUserQuestion`, `EnterPlanMode` and `ExitPlanMode` stay enabled as the only built-in
tools; they ask the user or switch the mode and touch nothing.

### Subagents

The `Agent` tool is off by default and can be enabled with `[claude] tools = ["Agent"]`.
Subagents only get tools the session already has, so they work through the same
`mcp__claustrum__*` tools and stay in the sandbox. Probed with Claude Code 2.1.284:

- Explore and general-purpose get the Claustrum tools only, no built-in `Bash`, `Read`,
  `Write`, ...
- `tools: Bash, Read, Write` in an agent's frontmatter does not bring the built-ins back;
  Claude Code refuses to start an agent whose tool list resolves to nothing.
- `hooks` and `mcpServers` in an agent's frontmatter take no effect: no hook command runs on
  the host, and `--strict-mcp-config` keeps the extra server out.

Explore also gets `mcp__claustrum__Write` and `Edit`, so unlike in plain Claude Code it
is not read-only. The guarantees above are Claude Code's behavior, not Claustrum's:
`.claude/agents/` stays writable from the sandbox, and a later Claude Code release that
honors frontmatter hooks there would run them on the host. Run
[`scripts/probe-subagents.sh`](scripts/probe-subagents.sh) on the host after upgrading
Claude Code; every file it lists under "host side effects" is an escape.

### Sandbox layout

| Guest path | Backing | Notes |
| --- | --- | --- |
| `/workspace` | host project directory | read/write; the working directory |
| `/workspace/claustrum.toml` | host file | read-only, see [Configuration](#configuration) |
| `/workspace/.claude/settings.json`, `settings.local.json` | host files | read-only: Claude Code runs their hooks on the host |
| `/tmp`, `/home/claude` | in memory | persist for the lifetime of the server |
| `/bin`, `/usr/bin` | package commands | populated from the loaded `.webc` files |
| `/etc/claustrum/profile.sh` | in memory | sourced by every bash via `BASH_ENV` |
| `/.claustrum/cmd/<name>` | host process | request channel of a host command |
| additional `[[mounts]]` | host directories | read-only at the configured guest path; read/write with `writable = true` |

Guest commands get a fixed environment (`HOME=/home/claude`, `USER=claude`,
`PATH=/usr/local/bin:/bin:/usr/bin`, `TERM=dumb`, `LANG=C.UTF-8`) plus anything under
`[sandbox.env]`. Because WASIX reports stdio as a terminal, the profile disables colours and
pagers (`NO_COLOR`, `PAGER=cat`, `jq -M`, `ls --color=never`) so captured output stays clean.

## Two sandbox layers

WASIX is the first layer: guest code only sees what the runtime hands it. Everything that
Claustrum itself runs natively on the host is the attack surface behind it: the Wasmer
runtime and its JIT, the native file tools, and host actions, which start real host
programs. A bug in the runtime would otherwise land an attacker directly in your user
account.

So Claustrum wraps those processes in an operating-system sandbox as a second layer.
Because macOS Seatbelt profiles cannot be nested, `claustrum serve` splits into a small
unconfined **broker** and a confined **worker**:

```
                       stdio / MCP
 claude ──────────────────────────────▶ worker   Wasmer runtime + native tools
                                          │      OS profile: read own binary, packages,
                                          │      config, read-only mounts; write workspace,
                                          │      writable mounts, cache, log, private tmp;
                                          │      no exec; no network
                                          │      unless the network mode needs it
                                          │
                                          │  Unix socket: "run action <name>, inputs …"
                                          ▼
                                        broker   loads the configuration, never runs
                                          │      guest code, validates every request
                                          │      against its own copy of the actions,
                                          │      runs the action network proxy
                                          │
                                          ▼
                                        action   one host program per run
                                                 OS profile: read the file system except
                                                 credential stores; write workspace,
                                                 $TMPDIR, declared `writable`; network
                                                 only via the proxy on localhost
```

| Process | May read | May write | May execute | Network |
| --- | --- | --- | --- | --- |
| **worker** | its binary, packages, configuration, read-only extra mounts, the user-wide module cache | workspace, writable extra mounts, its workspace's cache and network log, private temp dir | nothing | none, or outbound only if the mode requires it |
| **broker** | everything (unconfined) | everything (unconfined); for the worker only this workspace's plan files | actions only, each in its own profile | proxy on `127.0.0.1` |
| **action** | everything except credential stores and `deny_read` | workspace, fresh `$TMPDIR`, `writable` list | anything | `localhost:<proxy port>` only (`host` mode: unrestricted) |

Compiled modules are native code that Wasmer loads without further checks, so the worker
never writes the user-wide module cache: it reads it and caches what is missing in a
directory of its own workspace. That cache is only ever loaded by the worker of the same
workspace. `claustrum pkg sync` and `pkg precompile`, which run unconfined, fill the
user-wide cache.

Credential stores (`~/.ssh`, `~/.aws`, `~/.gnupg`, keychains, browser profiles, `~/.claude`,
...) stay unreadable for confined processes even when they lie inside a mount. The
configuration files stay read-only for all of them.

`[sandbox] confinement` selects how strict this is:

| Value | Behaviour |
| --- | --- |
| `"best-effort"` (default) | confine where the platform supports it, warn loudly where not |
| `"required"` | refuse to start without OS confinement |
| `"off"` | single process, single layer; host actions run with your full rights |

| Platform | Backend | Status |
| --- | --- | --- |
| macOS | Seatbelt (`sandbox-exec`, generated SBPL profile) | implemented |
| Linux | bubblewrap (mount, PID, IPC, network namespaces) + Landlock + seccomp | implemented |
| Linux without user namespaces | Landlock + seccomp | implemented, weaker (see below) |
| Windows | AppContainer + Job object | planned; runs unconfined with a warning until then |

On Linux, bubblewrap builds the file system view (root read-only, writable trees bound
read/write, credential stores hidden behind empty mounts, the configuration bound read-only
over itself, a private network namespace when the process may not use the network). A helper
stage (`claustrum __confine-exec`) then applies Landlock (file access, TCP ports, abstract
sockets, signals) and a seccomp filter that refuses new namespaces, `ptrace`, `mount`, `bpf`,
`io_uring`, kernel keyrings, new Unix sockets and, without network, IP sockets. Confined
actions reach the proxy through a relay inside their network namespace.

Where unprivileged user namespaces are disabled (Ubuntu 24.04+ restricts them through
AppArmor), Claustrum falls back to Landlock and seccomp and says so on start. Landlock can
only grant, not deny, so two guarantees get weaker there: a file inside a writable tree cannot
be made read-only (the configuration is then protected by the WASIX layer and restored after
each action only), and the proxy port is reachable on every address, not only on `localhost`.
A process that escapes the WASIX layer could then also create `.claude/settings.json` or
`settings.local.json` in the workspace and so plant Claude Code hooks, which run on the host.
Enable unprivileged user namespaces where you can, so that bubblewrap is used.

`claustrum run` and `serve` print the active confinement, network mode and enabled actions on
every start.

## Getting started

Requirements: Rust 1.95+ (edition 2024) and a working `claude` on your `PATH`.

```sh
cargo install --path crates/claustrum-cli   # installs the `claustrum` binary

# Download the default packages (bash, coreutils, python, jq) and precompile them
claustrum pkg sync

# Start Claude Code in the current project, sandboxed
cd ~/my-project
claustrum run
```

Everything after `--` is passed through to `claude`, for example a headless prompt:

```sh
claustrum run --permission-mode acceptEdits -- -p "Add a README for this project"
```

`claustrum run --dry-run` prints the exact `claude` command line.

### Commands

| Command | Purpose |
| --- | --- |
| `claustrum run [--workspace DIR] [--permission-mode MODE] [-- claude args…]` | Launch Claude Code against the sandbox |
| `claustrum serve [--workspace DIR]` | Serve the tools over MCP on stdio (what `run` starts under the hood) |
| `claustrum pkg sync [--force]` | Download the configured packages and precompile them |
| `claustrum pkg add <spec>` | Download one package, e.g. `python/python` |
| `claustrum pkg add-wasm <name> <file.wasm> [--alias cmd]` | Register a self-built WASIX binary as a package |
| `claustrum pkg list` / `pkg commands` | Show installed packages and the commands they provide |
| `claustrum pkg precompile` | Compile all packages into the user-wide module cache ahead of time |
| `claustrum network report [--workspace DIR] [--all]` | Summarise refused connections and suggest `allow` entries |
| `claustrum plans [--workspace DIR]` | List the plan files Claude wrote for a workspace |
| `claustrum trust [--revoke]` | Review the project's `claustrum.toml` and trust it (see below) |

Global options: `--config FILE` (or `CLAUSTRUM_CONFIG`), `--packages-dir DIR` (or
`CLAUSTRUM_PACKAGES_DIR`). Logging goes to stderr and is controlled by `CLAUSTRUM_LOG`
(e.g. `CLAUSTRUM_LOG=debug`).

## Configuration

Claustrum looks for `claustrum.toml` in the current directory, then for `config.toml` in the
user configuration directory (`~/Library/Application Support/de.buschmanuel.claustrum/` on
macOS, `~/.config/claustrum/` on Linux). Every setting is optional. A small but complete
example:

```toml
[sandbox]
timeout_secs = 120            # per command, 0 = unlimited
confinement = "best-effort"   # best-effort | required | off

[network]
mode = "allowlist"            # disabled (default) | allowlist | audit | host
allow = ["crates.io", "*.crates.io", "github.com:443"]

[[mounts]]
guest = "/data"
host = "~/datasets"           # read-only unless writable = true

[[actions.action]]
name = "test"
description = "Run the whole Rust test suite"
command = ["cargo", "test", "--workspace"]
writable = ["~/.cargo/registry"]
```

| Section | Keys | Purpose |
| --- | --- | --- |
| `[sandbox]` | `workspace`, `timeout_secs`, `max_output_bytes`, `max_threads`, `max_memory_mb`, `confinement`, `deny_read`, `env` | resource limits, the second layer, guest environment |
| `[network]` | `mode`, `allow`, `log` | what the guest and host actions may reach, see [Network](#network) |
| `[packages]` | `dir`, `online`, `[[packages.package]]` (`file`, `source`, `id`) | which `.webc` files are loaded, see [Packages](#packages) |
| `[[mounts]]` | `guest`, `host`, `writable` | additional host directories, read-only by default |
| `[actions]` | `command`, `[[actions.action]]` | fixed host commands Claude may trigger, see [Host actions](#host-actions) |
| `[claude]` | `binary`, `tools`, `args`, `system_prompt`, `plans` | how `claude` is launched, [plan mode](#plan-mode) |

[`claustrum.example.toml`](claustrum.example.toml) documents every key;
[`examples/rust.claustrum.toml`](examples/rust.claustrum.toml) is a complete setup for a Rust
project (check, build, test, clippy, fmt, doc, add-dep, update and the local git workflow as confined
actions; dependency sources mounted read-only).

### A project's configuration needs your trust

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

Independently of trust, `[claude] args` may not contain flags that would override what
`claustrum run` sets up (`--tools`, `--allowedTools`, `--mcp-config`, `--strict-mcp-config`,
`--permission-mode`, `--dangerously-skip-permissions`, `--settings`, `--setting-sources`,
`--add-dir`, `--plugin-dir`, `--system-prompt`, ...), in any configuration file. Use
`[claude] tools` for built-in tools and the command line for the rest.

### The configuration is read-only inside the sandbox

Every file Claustrum could load its configuration from is protected: `claustrum.toml` in the
workspace (even when it does not exist yet), the default search paths and the file passed
with `--config`, in every mount that contains them. The guest can read them but writing,
truncating, creating, deleting, renaming or replacing them is refused, as is renaming or
removing a directory that contains one. The check runs on the host below every tool, follows
symlinks and hard links, and compares names case-insensitively on macOS. Change the
configuration from outside the sandbox.

Every host-backed mount is also confined to its directory. A symlink that points outside,
checked into a repository or created by a host action, is refused by every tool; links
inside the mount keep working. When a host action changes a protected file, Claustrum puts
it back without following anything the action left in its place: links are removed,
directories are moved aside as `<name>.claustrum-moved`.

The same holds for Claude Code's own project settings, `.claude/settings.json` and
`.claude/settings.local.json`. Claude Code runs on the host, outside the sandbox, and executes
the hooks, status line and helper commands configured there. The guest can neither create
those files nor turn `.claude` into a link. Other files in `.claude` (agents, commands,
skills) stay writable. Their shell snippets only run through the built-in Bash tool, which
Claustrum removes.

`claustrum serve` creates `.claude` in the workspace if it is missing. Under bubblewrap, a
settings file that does not exist yet cannot be protected by a bind mount of its own, so
`.claude` itself is bound read-only and its existing entries read/write again: a new entry
directly in `.claude` (a new `agents` directory, say) has to be created outside the sandbox,
while everything below existing entries stays writable.

### Plan mode

`claustrum run --permission-mode plan` (or Shift+Tab in the session) works as usual. Claude
Code names a plan file in its own plan directory, `~/.claude/plans/<slug>.md`, and in plan
mode refuses every tool that is not marked read-only, so Claustrum's `Write`, `Edit` and
`Bash` too. Claude writes the plan with `WritePlan` and `EditPlan` instead:

- They take the plan file path and nothing else: a `<slug>.md` directly in the plan
  directory.
- The broker writes the file, never following a link. It only touches files that this
  workspace created, recorded in a ledger in the user state directory. Plans of other
  projects can be neither read nor changed.
- The plan directory is not mounted in the guest, and the confined worker may not read
  `~/.claude` at all.

Claude Code accepts a custom `plansDirectory` only inside the project, where the guest could
tamper with what Claude Code later reads and writes on the host, so Claustrum keeps the
default. `claustrum plans` lists a workspace's plans; `[claude] plans = false` removes the
plan tools and the plan mode tools.

## Network

Every connection Claustrum starts, from the guest and from [host actions](#host-actions),
passes one gate that allows only declared destinations.

| Mode | Guest and actions may reach | Logged |
| --- | --- | --- |
| `disabled` (default) | nothing | refusals |
| `allowlist` | the `allow` entries only | refusals |
| `audit` | everything | everything the allowlist would refuse |
| `host` | everything | allowed connections at debug level |

`allow` entries are `host[:ports]`:

```toml
[network]
mode = "allowlist"
allow = [
  "crates.io",          # port defaults to 443
  "*.crates.io",        # subdomains only, not crates.io itself
  "github.com:443",
  "pypi.org:80,443",    # several ports, or "8000-8100", or "*"
  "127.0.0.1:5432",     # local and private addresses need an explicit IP or CIDR entry
]
```

### How the gate decides

```
 guest: resolve("crates.io")            guest: connect(1.2.3.4:443)
          │                                       │
          ▼                                       ▼
  name in allowlist? ── no ──▶ REFUSED    IP entry covers ip:port? ── yes ──▶ ALLOWED
          │ yes                                   │ no
          ▼                                       ▼
  resolve on the host                      loopback / private / link-local /
  grant each address for the               CGNAT / ULA / metadata address? ── yes ──▶ REFUSED
  entry's ports                                   │ no
          │                                       ▼
          ▼                                was ip granted by an allowed name,
       ALLOWED                             and is the port in that grant?
                                              yes ──▶ ALLOWED     no ──▶ REFUSED
```

- **Names are checked when they are resolved.** Only allowed names resolve, and the
  addresses they resolve to may then be used on the ports of the matching entry. A literal IP
  that was never resolved from an allowed name is refused, so allowing `github.com` does not
  open arbitrary addresses.
- **Local and private addresses need an explicit entry**, even when an allowed name
  resolves to them. This defeats DNS rebinding and keeps the guest away from services on your
  machine and from cloud metadata endpoints such as `169.254.169.254`.
- **Only outgoing TCP is supported.** UDP, listening sockets and TCP sockets bound before
  connecting are refused, because their later destinations would not pass the gate. DNS
  resolution happens on the host.
- **Guest traffic cannot bypass the gate.** The guest has no sockets of its own; every socket
  call goes through the WASIX runtime, which Claustrum wraps.
- **Host actions go through a local proxy.** Actions start with `HTTP_PROXY`, `HTTPS_PROXY`
  and the variants cargo and npm read, pointing at a proxy on `127.0.0.1` that applies the
  same allowlist and checks the host name of every `CONNECT`. It requires a per-session
  credential, so other local processes cannot use it. Confined actions cannot reach anything
  but the proxy, so git over SSH, raw sockets and programs that ignore the proxy variables
  simply fail. Unconfined actions can ignore the proxy.

Refused connections are logged and appended to the tool result as
`[network: refused tcp example.com:443 (example.com is not in the allowlist)]`, so Claude can
ask for the destination instead of trying workarounds.

### Building an allowlist

Run a session with `mode = "audit"`: everything is reachable, and everything the allowlist
would refuse is logged. Then:

```sh
claustrum network report
```

groups the log by destination and prints ready-to-paste `allow` entries. Review them before
adding, the report cannot tell a needed download from an unwanted one. The log is JSON lines
in the user state directory, one file per workspace, or wherever `[network] log` points.

A grant lasts 15 minutes after the name last resolved to the address (longer than a Bash call
may run), so an address a name no longer points to stops being reachable.

Known limit: the guest gate decides on address and port, not on the TLS server name. Two
names served from the same CDN address share their grant. The action proxy sees the host name
of each `CONNECT` and checks it.

## Packages

Packages are `.webc` files from the [Wasmer registry](https://wasmer.io/explore). `pkg sync`
stores them in the user data directory and records each package's identity in a `.webc.id`
file next to it, which is how dependencies between packages (bash depends on coreutils)
resolve offline. Files obtained some other way can be given an explicit `id` in the
configuration.

The default packages are pinned to the versions below (`wasmer/coreutils@=1.0.25`, ...), so a
new release in the registry does not change the sandbox. In your own `[[packages.package]]`
entries, pin with `@=<version>`: a bare `@<version>` is a semver requirement and takes the
newest compatible release. To upgrade, change the version and run `claustrum pkg sync --force`;
without `--force`, packages already present are kept.

| Package | Provides | Notes |
| --- | --- | --- |
| `wasmer/bash` | `bash`, `sh` | the shell behind the `Bash` tool |
| `wasmer/coreutils` | `ls`, `cat`, `cp`, `mv`, `sort`, `wc`, ... | WASIX build of coreutils |
| `python/python` | `python`, `python3`, `pip` | CPython 3.13 with the standard library, about 60 MB; compiles for close to a minute on first use, which is why `pkg sync` precompiles |
| `syrusakbary/jq` | `jq` | jq 1.6 |

Self-built tools can be added without the `wasmer` CLI: compile a Rust program with
[`cargo wasix`](https://github.com/wasix-org/cargo-wasix) and register the result with
`claustrum pkg add-wasm <name> <file.wasm>`, which writes a directory package (`wasmer.toml`
plus the module) into the packages directory. [`scripts/build-jaq.sh`](scripts/build-jaq.sh)
does this for [jaq](https://github.com/01mf02/jaq), a current jq implementation in Rust.

**git is not available yet.** No WASIX build of git is published anywhere; the only known
recipe is the [wasinix](https://github.com/wasix-org/wasinix) Nix flake, which builds on
x86_64 Linux only and needs a patched runtime. gitoxide does not target WASIX either. Until
that changes, git operations happen on the host, for example through a host action.

## Host actions

Sometimes a task needs something that only exists on the host: the Rust toolchain, a deploy
script, a formatter. Instead of loosening the sandbox, `claustrum.toml` can declare
**actions**: fixed host commands that Claude may *trigger*, in the spirit of a CI job.

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

Inside the sandbox the actions appear as the `host` command (`host` lists them,
`host test` triggers one, `host test-crate claustrum-cli` or
`host test-crate crate=claustrum-cli` passes inputs), and as the `Action` MCP tool whose
description carries the same list.

### Lifecycle of one action

```
 guest: host test-crate claustrum-cli
   │
   ▼
 shim writes args + cwd to /.claustrum/cmd/host ──▶ worker: parse, forward to broker
                                                        │
                                                        ▼
                                              broker: look up "test-crate"
                                                bind inputs: length, control chars,
                                                leading "-", then pattern / choices /
                                                path (inside workspace) / integer
                                                      │ refused ──▶ exit 2 + reason
                                                      ▼ ok
                                                snapshot protected files
                                                start program in its own OS profile:
                                                  argv fixed, one element per placeholder
                                                  clean env (PATH, HOME, LANG, + env)
                                                  stdin closed, own process group
                                                  proxy variables set, private $TMPDIR
                                                      │
                                                      ▼
                                                wait: timeout or killed Bash call
                                                  ──▶ SIGKILL the whole group
                                                restore protected files if changed
                                                      │
   ◀────────── exit code, bounded stdout/stderr, network notes ──────────┘
```

### Input kinds

| `kind` | Accepts | Options |
| --- | --- | --- |
| `pattern` | values matching the anchored regex | `pattern` |
| `choices` | one of the listed values | `choices` |
| `path` | a path that resolves inside the workspace, also through symlinks | `must_exist` |
| `integer` | an integer | `min`, `max` |

Every value is limited to `max_len` bytes (default 256), may not contain control characters
and may not start with `-` unless `allow_leading_dash = true`. `default` makes an input
optional. An action without inputs refuses any argument.

### What holds, by construction

- **No free-form commands.** The argv is fixed in the configuration, which is read-only
  inside the sandbox. No shell is involved on the host; `command[0]` is resolved once at
  startup.
- **Every input is validated** before it is substituted into a single argv element.
- **The process is contained in time and output.** Clean environment, closed stdin, a working
  directory inside the workspace, its own process group, a timeout, capped output, one action
  at a time.
- **The configuration stays what it was.** Protected files are snapshotted before and restored
  after every action.
- **The program is confined** by the OS sandbox: writes stay in the workspace, `$TMPDIR` and
  the action's `writable` list, credential stores are unreadable, and the network is only
  reachable through the proxy. A single action can opt out with `confine = "none"`.

### Use with care

Every action is a hole in the sandbox that you cut on purpose, and the validation only guards
the edges you declared. Confinement narrows what the program can do, but it still runs as
your user with read access to most of the file system.

| Risk | Example | Mitigation |
| --- | --- | --- |
| Programs execute code from the workspace, which Claude can write | `cargo` honours `.cargo/config.toml`, `build.rs`, proc macros; `git` honours `.git/config` and hooks; `npm` runs `package.json` scripts; `make` runs the Makefile | keep such actions confined; never `confine = "none"` for them |
| Validated inputs are still interpreted by the program | `ext::sh -c …` is a valid git URL, `@file` reads a file for curl, `key=value` after `git -c` changes behaviour | keep patterns to the characters the program needs; check how it treats them |
| Placeholders reaching an interpreter | `sh -c "… {x}"`, `python -c "{x}"` | refused at startup; put the code in a script and pass the input as an argument |
| Environment values are inputs too | `RUSTFLAGS = "{flags}"`, `env_passthrough` of `PATH`, `LD_PRELOAD`, `GIT_*` | warned at startup; avoid |
| Path inputs are checked before the program opens them | a file replaced by a symlink between check and open | do not rely on `path` inputs to keep a program away from host files |
| Side effects leave the sandbox | `git push`, `npm publish` ship whatever the workspace contains | only declare commands you would let Claude run on the host directly |
| Writable caches | `~/.cargo/bin`, `~/.cargo/config.toml`, shell profiles | never make a directory writable that the host executes from later |

Claustrum refuses the clear cases at startup and logs a warning for the rest. Without OS
confinement (`confine = "none"`, `confinement = "off"`, or a platform without a backend), an
action is as trusted as the host command behind it.

## Development

```
crates/
  claustrum-sandbox/   runtime, mounts, packages, process execution, native tools,
                       host commands (hostcmd.rs), host actions (action/), network
                       gate and action proxy (net/)
  claustrum-mcp/       MCP server (rmcp) exposing the tools
  claustrum-cli/       `claustrum` binary: run, serve (broker + confined worker),
                       pkg, network, configuration
  claustrum-confine/   OS confinement profiles and backends (Seatbelt on macOS,
                       bubblewrap + Landlock + seccomp on Linux)
shim/                  guest-side WASI shim for host commands (wasm32-wasip1), built by
                       scripts/build-shim.sh and committed as
                       crates/claustrum-sandbox/assets/hostcmd.wasm
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

[`CODE_REVIEW.md`](CODE_REVIEW.md) holds the notes of a security and stability review of the
current code, with findings ordered by severity.

## Roadmap

- git inside the sandbox (blocked on a WASIX build, see Packages); sed/awk/grep as WASIX
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
