# Configuration reference

All keys, grouped by section. Paths may start with `~/`. The annotated
[`claustrum.example.toml`](https://github.com/manuelbusch/claustrum/blob/main/claustrum.example.toml)
contains the same information as comments.

## `[sandbox]`

| Key | Default | Meaning |
| --- | --- | --- |
| `workspace` | current directory | host directory mounted read/write at `/workspace` |
| `timeout_secs` | `120` | per-command timeout in seconds; `0` = unlimited. A `Bash` call always ends after at most 10 minutes. Host actions inherit it unless they set their own. |
| `max_output_bytes` | `1048576` | bytes kept per output stream (stdout, stderr) of a command |
| `max_threads` | `64` | guest processes and threads per command, counted together |
| `max_memory_mb` | `1024` | memory one guest process may grow to, in MiB; `0` allows wasm32's 4 GiB |
| `confinement` | `"best-effort"` | OS sandbox around the worker and the host actions: `"best-effort"`, `"required"` or `"off"`, see [OS confinement](../security/confinement.md) |
| `deny_read` | `[]` | host paths confined processes may never read, in addition to the built-in credential stores (`~/.ssh`, `~/.aws`, `~/.gnupg`, keychains, browser profiles, ...) |
| `network` | – | shorthand for `[network] mode`, accepts `"disabled"` or `"host"` |

### `[sandbox.env]`

Extra environment variables for guest commands:

```toml
[sandbox.env]
RUST_LOG = "info"
```

## `[network]`

| Key | Default | Meaning |
| --- | --- | --- |
| `mode` | `"disabled"` | `"disabled"`, `"allowlist"`, `"audit"` or `"host"`, see [Network](../network/index.md) |
| `allow` | `[]` | destinations as `host[:ports]`, see [allow entries](../network/index.md#allow-entries) |
| `log` | per-workspace file in the user state directory | JSON-lines file every decision is appended to; relative to the workspace. It may not lie in the state directory (other than its `network` directory), in Claude Code's configuration directory, or be a protected file |

## `[packages]`

| Key | Default | Meaning |
| --- | --- | --- |
| `dir` | `packages` in the user data directory | directory holding the `.webc` files; `--packages-dir` / `CLAUSTRUM_PACKAGES_DIR` override it |
| `online` | `false` | let the runtime fetch missing packages from the Wasmer registry (the worker then needs outbound network) |

### `[[packages.package]]`

The packages loaded into the sandbox. When the list is empty, bash, coreutils, python and jq
are used.

| Key | Meaning |
| --- | --- |
| `file` | file name in the packages directory; may also name a directory package created by `pkg add-wasm` |
| `source` | what `claustrum pkg sync` downloads, e.g. `wasmer/bash@=1.0.25`; pin with `@=<version>` |
| `id` | package identity, only needed for files obtained elsewhere that other packages depend on |

See [Packages](../packages.md).

## `[[mounts]]`

Additional host directories, mounted at absolute guest paths.

| Key | Default | Meaning |
| --- | --- | --- |
| `guest` | – | absolute path inside the sandbox, e.g. `/data` |
| `host` | – | host directory |
| `writable` | `false` | mount read/write instead of read-only |

```toml
[[mounts]]
guest = "/data"
host = "~/datasets"

[[mounts]]
guest = "/out"
host = "~/results"
writable = true
```

## `[actions]`

| Key | Default | Meaning |
| --- | --- | --- |
| `command` | `"host"` | guest command that triggers the actions (`host test` in Bash); must not collide with a package command |

### `[[actions.action]]`

| Key | Default | Meaning |
| --- | --- | --- |
| `name` | – | name used to trigger the action, `[a-z][a-z0-9-]{0,31}` |
| `description` | `""` | shown to Claude in listings and in the `Action` tool description |
| `command` | – | the host argv; `command[0]` is an absolute path or a name looked up on `PATH` at startup, the rest may contain `{input}` placeholders. No shell is involved. |
| `cwd` | workspace | working directory, relative to the workspace and inside it |
| `timeout_secs` | `[sandbox] timeout_secs` | wall-clock limit; `0` = none |
| `max_output_bytes` | `[sandbox] max_output_bytes` | bytes kept per output stream |
| `env` | `{}` | environment variables for the program; only `PATH`, `HOME` and `LANG` are inherited from the host |
| `env_passthrough` | `[]` | host environment variables forwarded by name |
| `writable` | `[]` | extra host directories the confined program may write, e.g. `~/.cargo/registry`; relative paths are taken against the workspace |
| `confine` | `"os"` | `"none"` runs this action without OS confinement |

### `[[actions.action.input]]`

Inputs are taken positionally in the order they are declared, or as `name=value`.

| Key | Default | Meaning |
| --- | --- | --- |
| `name` | – | placeholder name, `[a-z][a-z0-9_]{0,31}` |
| `description` | – | shown to Claude |
| `kind` | inferred from `pattern` / `choices` | `"pattern"`, `"choices"`, `"path"` or `"integer"` |
| `pattern` | – | regular expression the whole value must match |
| `choices` | – | list of allowed values |
| `must_exist` | `false` | `path` only: refuse paths that do not exist |
| `min`, `max` | – | `integer` only: inclusive bounds |
| `default` | – | makes the input optional |
| `max_len` | `256` | maximum length in bytes |
| `allow_leading_dash` | `false` | permit values starting with `-` |

See [Host actions](../host-actions/index.md) and [Inputs](../host-actions/inputs.md).

## `[claude]`

| Key | Default | Meaning |
| --- | --- | --- |
| `binary` | `claude` on `PATH` | path to the Claude Code binary |
| `tools` | `[]` | built-in Claude Code tools to keep in addition to `AskUserQuestion` (and the plan mode tools); e.g. `WebSearch`, `Agent`, `Workflow`. See [Subagents and workflows](../subagents.md). |
| `args` | `[]` | extra arguments always passed to `claude`; flags that would override the sandbox are [refused](trust.md#flags-claude-args-may-not-contain) |
| `system_prompt` | – | text appended to the system prompt in addition to the sandbox notice |
| `plans` | `true` | keep `EnterPlanMode`/`ExitPlanMode` and add `WritePlan`/`EditPlan`, see [Plan mode](../plan-mode.md) |
