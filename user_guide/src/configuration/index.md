# Configuration

Every setting is optional; without a configuration file Claustrum runs with no network, no
extra mounts, no host actions and the default packages.

## Where Claustrum looks

The first file found is used:

1. The file passed with `--config FILE` (or the `CLAUSTRUM_CONFIG` environment variable).
2. `claustrum.toml` in the current directory, usually the project's.
3. `config.toml` in the user configuration directory:
   `~/Library/Application Support/de.buschmanuel.claustrum/` on macOS,
   `~/.config/claustrum/` on Linux.
4. The built-in defaults.

Files are not merged: a project's `claustrum.toml` replaces the user configuration entirely.
Unknown keys are an error, so a typo does not silently fall back to a default.

A project's `claustrum.toml` is only used after you have
[trusted](trust.md) its exact content. Inside the sandbox, every configuration file is
[read-only](protected-files.md): Claude can read it, but never loosen its own sandbox.

## A small but complete example

```toml
[sandbox]
timeout_secs = 120            # per command, 0 = unlimited
confinement = "required"      # required (default) | best-effort | off

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

## Sections

| Section | Keys | Purpose |
| --- | --- | --- |
| `[sandbox]` | `workspace`, `timeout_secs`, `max_output_bytes`, `max_threads`, `max_memory_mb`, `confinement`, `deny_read`, `env`, `network` | resource limits, the second layer, guest environment; `network` is a shorthand for `[network] mode` |
| `[network]` | `mode`, `allow`, `log` | what the guest and host actions may reach, see [Network](../network/index.md) |
| `[packages]` | `dir`, `online`, `[[packages.package]]` (`file`, `source`, `id`) | which `.webc` files are loaded, see [Packages](../packages.md) |
| `[[mounts]]` | `guest`, `host`, `writable` | additional host directories, read-only by default |
| `[actions]` | `command`, `[[actions.action]]` | fixed host commands Claude may trigger, see [Host actions](../host-actions/index.md) |
| `[claude]` | `binary`, `tools`, `args`, `system_prompt`, `plans` | how `claude` is launched, [plan mode](../plan-mode.md) |

Every key is described in the [reference](reference.md). The repository also contains two
annotated files to start from:

- [`claustrum.example.toml`](https://github.com/manuelbusch/claustrum/blob/main/claustrum.example.toml)
  documents every key with its default.
- [`examples/rust.claustrum.toml`](https://github.com/manuelbusch/claustrum/blob/main/examples/rust.claustrum.toml)
  is a complete setup for a Rust project, walked through in
  [Example: a Rust project](../host-actions/rust-example.md).
