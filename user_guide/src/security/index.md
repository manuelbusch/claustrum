# Two sandbox layers

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
                                          │      exec only itself; no network
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
| **worker** | its binary, packages, configuration, read-only extra mounts, the user-wide module cache | workspace, writable extra mounts, its workspace's cache and network log, private temp dir | its own binary only | none; outbound in `allowlist`/`audit` mode or with `[packages] online`; unrestricted in `host` mode |
| **broker** | everything (unconfined) | everything (unconfined); for the worker only this workspace's plan files | actions only, each in its own profile | proxy on `127.0.0.1` |
| **action** | everything except credential stores and `deny_read` | workspace, fresh `$TMPDIR`, `writable` list | anything | `localhost:<proxy port>` only (`host` mode: unrestricted) |

## The module cache

Compiled modules are native code that Wasmer loads without further checks, so the worker
never writes the user-wide module cache: it reads it and caches what is missing in a
per-workspace directory of the user cache (`workspaces/<key>`), together with downloaded
packages. That cache is only ever loaded by the worker of the same workspace.
`claustrum pkg sync` and `pkg precompile`, which run unconfined, fill the user-wide cache.

## Credential stores

Credential stores (`~/.ssh`, `~/.aws`, `~/.gnupg`, `~/.kube`, `~/.netrc`, cloud CLI
configurations, keychains, browser profiles, `~/.claude`, ...) stay unreadable for confined
processes even when they lie inside a mount. Further paths can be added with
[`[sandbox] deny_read`](../configuration/reference.md#sandbox).

The configuration files stay read-only for all of them, with one exception on Linux: a
workspace `claustrum.toml` that does not exist yet cannot be kept from being created by the OS
profile. The WASIX layer refuses to create it, a new file is not used before you
[trust](../configuration/trust.md) it, and one created by a host action is moved aside
afterwards.

How strict the second layer is, and which backend each platform uses, is described in
[OS confinement](confinement.md).
