# How the gate decides

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
- **Only outgoing TCP is supported.** In `disabled` and `allowlist` mode, UDP, listening
  sockets and TCP sockets bound before connecting are refused, because their later
  destinations would not pass the gate (`audit` allows and logs them, except binds to the
  wildcard address). DNS resolution happens on the host.
- **Guest traffic cannot bypass the gate.** The guest has no sockets of its own; every socket
  call goes through the WASIX runtime, which Claustrum wraps.
- **Host actions go through a local proxy.** Actions start with `HTTP_PROXY`, `HTTPS_PROXY`
  and the variants cargo and npm read, pointing at a proxy on `127.0.0.1` that applies the
  same allowlist and checks the host name of every `CONNECT`. It requires a per-session
  credential, so other local processes cannot use it. Confined actions cannot reach anything
  but the proxy, so git over SSH, raw sockets and programs that ignore the proxy variables
  simply fail. Unconfined actions can ignore the proxy.

## Grant lifetime

A grant lasts 15 minutes after the name last resolved to the address (longer than a Bash call
may run), so an address a name no longer points to stops being reachable.

## Known limits

The guest gate decides on address and port, not on the TLS server name. Two names served from
the same CDN address share their grant. The action proxy sees the host name of each `CONNECT`
and checks it.
