# OS confinement

## Choosing the strictness

`[sandbox] confinement` selects how strict the second layer is:

| Value | Behaviour |
| --- | --- |
| `"required"` (default) | refuse to start without OS confinement |
| `"best-effort"` | confine where the platform supports it; elsewhere run unconfined, with the reason on stderr |
| `"off"` | single process, single layer; host actions run with your full rights |

`claustrum run` and `serve` print the active confinement (and why it is off, if it is),
network mode and enabled actions on every start. Keep `"required"` whenever you declare
[host actions](../host-actions/index.md) for programs that run code from the workspace; the
trust prompt flags a project that lowers it.

## Platforms

| Platform | Backend | Status |
| --- | --- | --- |
| macOS | Seatbelt (`sandbox-exec`, generated SBPL profile) | implemented |
| Linux | bubblewrap (user, mount, PID, IPC, UTS, cgroup, network namespaces) + Landlock + seccomp | implemented; needs Linux 5.13+ with Landlock enabled |
| Linux without user namespaces | Landlock + seccomp | implemented, weaker (see below) |
| Windows | AppContainer + Job object | planned; runs unconfined with a warning until then |

## Linux

On Linux, bubblewrap builds the file system view (root read-only, writable trees bound
read/write, credential stores hidden behind empty mounts, the configuration bound read-only
over itself, a private network namespace when the process may not use the network). A helper
stage (`claustrum __confine-exec`) then applies Landlock (file access, TCP ports, abstract
sockets, signals) and a seccomp filter that refuses new namespaces, `ptrace` and
`process_vm_*`, `mount` and the new mount API, `bpf`, `io_uring`, `perf_event_open`,
`userfaultfd`, kernel modules and keyrings, the x32 system call ABI, and every socket family
but IP (Unix, netlink, packet, vsock, ...). Actions may only open TCP sockets. Confined actions reach the proxy through a relay inside
their network namespace. Without a kernel that supports Landlock there is no Linux backend;
Claustrum then refuses to start, and `best-effort` runs unconfined with a warning.

### Linux without user namespaces

Where unprivileged user namespaces are disabled (Ubuntu 24.04+ restricts them through
AppArmor), Claustrum falls back to Landlock and seccomp and logs a warning on start;
seccomp then also refuses IP sockets to processes without network. Landlock can
only grant, not deny, so two guarantees get weaker there: a file inside a writable tree cannot
be made read-only (the configuration is then protected by the WASIX layer and restored after
each action only), and the proxy port is reachable on every address, not only on `localhost`.
A process that escapes the WASIX layer could then also create `.claude/settings.json` or
`settings.local.json` in the workspace and so plant Claude Code hooks, which run on the host.
Enable unprivileged user namespaces where you can, so that bubblewrap is used.

`deny_read` paths are weaker there as well. Their contents stay unreadable, but Landlock
cannot refuse listing a directory below one it lets be listed, and every confined process may
list `/` and everything below it (so that it can find its way to the paths it may read). The
names of the files in `~/.ssh` and the other denied directories are therefore visible, as are
existence, size and modification time of every file, which Landlock does not restrict at all.
Under bubblewrap, denied directories are covered by empty ones and denied files by
`/dev/null`; only a denied directory that is created after the start is listable there.
