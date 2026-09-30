# Network

Every connection Claustrum starts, from the guest and from
[host actions](../host-actions/index.md), passes one gate that allows only declared
destinations. By default nothing is reachable.

## Modes

| Mode | Guest and actions may reach | Logged on stderr |
| --- | --- | --- |
| `disabled` (default) | nothing | refusals (warn) |
| `allowlist` | the `allow` entries only | refusals (warn) |
| `audit` | everything | everything the allowlist would refuse (info) |
| `host` | everything | only allowed connections (debug) |

Allowed connections are logged at debug level in every mode, and every decision is written
to the network log (see [Building an allowlist](allowlist.md)).

## Allow entries

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

## What Claude sees when a connection is refused

Refused connections are logged and appended to the tool result as
`[network: refused dns example.com:443 (example.com is not in the allowlist)]`, so Claude can
ask for the destination instead of trying workarounds. Add the destination to `allow` if you
agree, and restart the session.

The rules the gate applies are described in [How the gate decides](gate.md). To find out
which destinations a project needs, see [Building an allowlist](allowlist.md).
