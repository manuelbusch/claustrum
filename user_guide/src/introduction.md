# Introduction

**Claustrum is a sandbox for [Claude Code](https://claude.com/claude-code).** It runs a POSIX
environment and a curated set of tools inside [Wasmer](https://wasmer.io/) using
[WASIX](https://wasix.org/) and exposes them to Claude through an
[MCP](https://modelcontextprotocol.io/) server. Claude Code starts with its built-in tools
removed and uses Claustrum's sandboxed tools instead, so nothing Claude does touches the host
outside the project directory.

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

## Why

Claude Code's built-in tools run with the full privileges of the user who started it.
Permission prompts help, but they rely on the user reviewing every command. Claustrum
replaces that trust boundary with a technical one: the only things Claude can affect are the
files that were explicitly mounted into the sandbox. This makes it practical to let Claude
work autonomously on a project without exposing the rest of the machine.

The boundary also covers Claustrum's own configuration. It usually sits in the project
directory, but Claude can only read it, never loosen its own sandbox for the next run.

## Status

Claustrum is early. The core loop works end to end (bash + coreutils + python + jq in the
sandbox, native file tools, Claude Code driving them over MCP, OS confinement on macOS and
Linux), but the tool set is small and the configuration format may still change.

## How to read this guide

- [Installation](installation.md) and [Your first session](getting-started.md) get you from
  nothing to a sandboxed Claude Code session.
- [How it works](how-it-works.md) and [What Claude sees](what-claude-sees.md) explain the
  moving parts and the environment inside the sandbox.
- The chapters on [configuration](configuration/index.md), the [network](network/index.md),
  [packages](packages.md) and [host actions](host-actions/index.md) show how to adapt the
  sandbox to a project.
- [Two sandbox layers](security/index.md) describes the security model in detail.
- [Commands](commands.md) and [Troubleshooting](troubleshooting.md) are for looking things up.
