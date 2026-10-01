# What Claude sees

## Tools

| Tool | Parameters (same as Claude Code) | Implementation |
| --- | --- | --- |
| `Bash` | `command`, `timeout` (ms, max 600 000; 0 or none: the default), `description` | `bash -c` in a fresh WASIX process |
| `Read` | `file_path`, `offset`, `limit` | native, `cat -n` style output |
| `Write` | `file_path`, `content` | native, creates parent directories |
| `Edit` | `file_path`, `old_string`, `new_string`, `replace_all` | native, exact string replacement |
| `Glob` | `pattern`, `path` | native, newest first, skips `.git`, `.hg`, `.svn`, `node_modules`, `target` |
| `Grep` | `pattern`, `path`, `glob`, `output_mode`, `-i`, `-C`, `head_limit` | native, ripgrep engine |
| `Action` | `name`, `inputs` | only when [host actions](host-actions/index.md) are configured |
| `WritePlan`, `EditPlan` | as `Write` and `Edit` | plan files only, see [Plan mode](plan-mode.md) |

In Claude Code they appear as `mcp__claustrum__Bash`, `mcp__claustrum__Read` and so on.

`AskUserQuestion` stays enabled as a built-in tool, and so do `EnterPlanMode` and
`ExitPlanMode` unless `[claude] plans = false`; they ask the user or switch the mode and
touch nothing. [`[claude] tools`](configuration/reference.md#claude) adds further built-in
tools, for example `WebSearch` (which runs on the API side) or `Agent` for
[subagents](subagents.md).

## Sandbox layout

| Guest path | Backing | Notes |
| --- | --- | --- |
| `/workspace` | host project directory | read/write; the working directory |
| `/workspace/claustrum.toml` | host file | read-only, see [Protected files](configuration/protected-files.md) |
| `/workspace/.claude/settings.json`, `settings.local.json` | host files | read-only: Claude Code runs their hooks on the host |
| `/tmp`, `/home/claude` | in memory | persist for the lifetime of the server |
| `/bin`, `/usr/bin` | package commands | populated from the loaded `.webc` files, see [Packages](packages.md) |
| `/etc/claustrum/profile.sh` | in memory | sourced by every bash via `BASH_ENV` |
| `/.claustrum/cmd/<name>` | host process | request channel of a host command |
| additional [`[[mounts]]`](configuration/reference.md#mounts) | host directories | read-only at the configured guest path; read/write with `writable = true` |

Nothing else on the host is visible. `/tmp` and `/home/claude` are lost when the session
ends; only `/workspace` and writable mounts reach the host's disk.

## Environment

Guest commands get a fixed environment (`HOME=/home/claude`, `USER=claude`,
`PATH=/usr/local/bin:/bin:/usr/bin`, `TERM=dumb`, `LANG=C.UTF-8`) plus anything under
[`[sandbox.env]`](configuration/reference.md#sandbox). Because WASIX reports stdio as a
terminal, the profile disables colours and pagers (`NO_COLOR`, `CLICOLOR=0`, `PAGER=cat`,
`GIT_PAGER=cat`, `jq -M`, `ls -1 --color=never`) and sets `PYTHONUNBUFFERED=1`, so captured
output stays clean.

## Limits

Each command runs with the limits from [`[sandbox]`](configuration/reference.md#sandbox): a
timeout (120 seconds by default), at most 1 MiB kept per output stream, 64 guest processes and
threads, and 1 GiB of memory per guest process. A `Bash` call ends after 10 minutes at the
latest, whatever the configuration says.

Refused network connections are appended to the tool result as
`[network: refused ...]`, so Claude can ask for the destination instead of trying
workarounds; see [Network](network/index.md).
