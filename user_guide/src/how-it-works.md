# How it works

Claustrum is three things stacked on each other:

| Layer | What it is | What it gives you |
| --- | --- | --- |
| **Sandbox** | Wasmer runtime booting a WASIX environment per command | `bash`, `coreutils`, `python`, `jq` run unmodified as WebAssembly. The guest sees an in-memory root with the project mounted read/write at `/workspace`. Networking is off unless configured. |
| **Tools** | An MCP server (`claustrum serve`) | `Bash`, `Read`, `Write`, `Edit`, `Glob`, `Grep`, named and shaped like Claude Code's own tools, plus `Action` for declared host commands. |
| **Launcher** | `claustrum run` | Starts `claude` with every built-in tool removed except a few harmless ones (`--tools`), Claustrum as its only MCP server (`--strict-mcp-config --mcp-config …`), its tools pre-approved (`--allowedTools mcp__claustrum__*`) and a system prompt describing the sandbox (`--append-system-prompt`). |

From Claude's point of view nothing changes: it still has file and shell access, only inside
the sandbox. Which tools remain, and what the sandbox looks like from the inside, is
described in [What Claude sees](what-claude-sees.md).

## A `Bash` call, step by step

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
`Bash` call is a fresh process; only the file system persists between calls. A `cd` in one
call therefore does not carry over to the next.

## Behind the tools

The WASIX runtime is the first sandbox layer. Everything Claustrum itself runs natively on
the host (the runtime, the file tools, [host actions](host-actions/index.md)) is wrapped in
an operating-system sandbox as a second layer, and all network traffic passes one
[gate](network/index.md). [Two sandbox layers](security/index.md) explains this in detail.
