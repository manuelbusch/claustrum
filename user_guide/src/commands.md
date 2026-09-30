# Commands

| Command | Purpose |
| --- | --- |
| `claustrum run [--workspace DIR] [--permission-mode MODE] [--claude PATH] [--dry-run] [-- claude args…]` | Launch Claude Code against the sandbox |
| `claustrum serve [--workspace DIR]` | Serve the tools over MCP on stdio (what `run` starts under the hood) |
| `claustrum pkg sync [--force]` | Download the configured packages and precompile them |
| `claustrum pkg add <spec>` | Download one package, e.g. `python/python` |
| `claustrum pkg add-wasm <name> <file.wasm> [--alias cmd]` | Register a self-built WASIX binary as a package |
| `claustrum pkg list` / `pkg commands` | Show installed packages and the commands they provide |
| `claustrum pkg precompile` | Compile all packages into the user-wide module cache ahead of time |
| `claustrum network report [--workspace DIR] [--all]` | Summarise refused connections and suggest `allow` entries |
| `claustrum plans [--workspace DIR]` | List the plan files Claude wrote for a workspace |
| `claustrum trust [--revoke]` | Review the project's `claustrum.toml` and trust it |

`claustrum run --permission-mode` accepts Claude Code's modes: `default`, `acceptEdits`,
`plan`, `dontAsk`, `bypassPermissions`. Everything after `--` goes to `claude` unchanged.

## Global options

| Option | Environment variable | Meaning |
| --- | --- | --- |
| `--config FILE` | `CLAUSTRUM_CONFIG` | use this configuration file instead of the [default lookup](configuration/index.md#where-claustrum-looks) |
| `--packages-dir DIR` | `CLAUSTRUM_PACKAGES_DIR` | directory holding the `.webc` packages |

## Environment variables

| Variable | Meaning |
| --- | --- |
| `CLAUSTRUM_LOG` | log filter for stderr, e.g. `CLAUSTRUM_LOG=debug`; falls back to `RUST_LOG`, default `warn` |
| `CLAUSTRUM_STATE_DIR` | overrides the user state directory (trust records, plan ledger, network logs) |
| `CLAUSTRUM_CACHE_DIR` | overrides the user cache directory (compiled modules, per-workspace caches) |

## Files and directories

| Purpose | macOS | Linux |
| --- | --- | --- |
| User configuration | `~/Library/Application Support/de.buschmanuel.claustrum/config.toml` | `~/.config/claustrum/config.toml` |
| Packages (data) | `~/Library/Application Support/de.buschmanuel.claustrum/packages` | `~/.local/share/claustrum/packages` |
| State: trust records, plan ledger, network logs | `~/Library/Application Support/de.buschmanuel.claustrum` | `~/.local/state/claustrum` |
| Cache: compiled modules, per-workspace caches | `~/Library/Caches/de.buschmanuel.claustrum` | `~/.cache/claustrum` |

On Linux the usual `XDG_*_HOME` variables move these directories.
