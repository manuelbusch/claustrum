# Troubleshooting

## Claude reports `[network: refused ...]`

The destination is not allowed by the [network configuration](network/index.md); by default
nothing is. Add it to `[network] allow` if you agree and restart the session. To find all
destinations a project needs at once, use [audit mode](network/allowlist.md). Remember that
local and private addresses need an explicit IP or CIDR entry, even when an allowed name
resolves to them.

## Claustrum refuses to start because the configuration is not trusted

A project's `claustrum.toml` is only used after you accepted it. In a terminal,
`claustrum run` asks; without one (another MCP client starting `claustrum serve`, CI) it
refuses. Run `claustrum trust` in the project directory, or pass the file with `--config`.
Any change to the file, such as a `git pull`, asks again. See
[Trusting a project's configuration](configuration/trust.md).

## Claude cannot change `claustrum.toml` or `.claude/settings.json`

That is intended: these files are [protected](configuration/protected-files.md) inside the
sandbox. Edit them from outside.

## A new directory in `.claude` cannot be created (Linux)

Under bubblewrap, `.claude` itself is bound read-only so that settings files cannot be
created. Create new entries directly in `.claude` (such as an `agents` directory) from
outside the sandbox; everything below existing entries stays writable.

## The first Python call takes a long time

Python compiles for close to a minute on first use. `claustrum pkg sync` precompiles all
packages; run `claustrum pkg precompile` if packages were added another way.

## A command is not found in the sandbox

Only commands from the loaded [packages](packages.md) exist; `claustrum pkg commands` lists
them. git in particular is not available yet. Tools that only exist on the host can be
offered as [host actions](host-actions/index.md).

## A warning about missing OS confinement

The platform has no backend (Windows, Linux without Landlock), or on Linux unprivileged user
namespaces are disabled and the weaker Landlock-only backend is used. See
[OS confinement](security/confinement.md). Set `confinement = "required"` to refuse to start
in that case.

## `[claude] args` is refused

The flag would override the sandbox that `claustrum run` sets up; see the
[list of refused flags](configuration/trust.md#flags-claude-args-may-not-contain). Use
`[claude] tools` for built-in tools and the options of `claustrum run` (such as
`--permission-mode`) for the rest.

## More output

Set `CLAUSTRUM_LOG=debug` to see what Claustrum does, including allowed network connections.
