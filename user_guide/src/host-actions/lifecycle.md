# Lifecycle and guarantees

## Lifecycle of one action

```
 guest: host test-crate claustrum-cli
   │
   ▼
 shim writes args + cwd to /.claustrum/cmd/host ──▶ worker: parse, forward to broker
                                                        │
                                                        ▼
                                              broker: look up "test-crate"
                                                bind inputs: length, control chars,
                                                leading "-", then pattern / choices /
                                                path (inside workspace) / integer
                                                      │ refused ──▶ exit 2 + reason
                                                      ▼ ok
                                                snapshot protected files
                                                start program in its own OS profile:
                                                  argv fixed, one element per placeholder
                                                  clean env (PATH, HOME, LANG, TERM,
                                                    NO_COLOR + env)
                                                  stdin closed, own process group
                                                  proxy variables set (unless host mode)
                                                  private $TMPDIR (confined actions)
                                                      │
                                                      ▼
                                                wait: timeout or killed Bash call
                                                  ──▶ SIGKILL the whole group
                                                on exit: SIGKILL what is left in the group,
                                                  read remaining output for at most 2 s
                                                restore protected files if changed
                                                      │
   ◀────────── exit code, bounded stdout/stderr, network notes ──────────┘
```

The worker and the broker are the two halves of `claustrum serve`, see
[Two sandbox layers](../security/index.md).

## What holds, by construction

- **No free-form commands.** The argv is fixed in the configuration, which is read-only
  inside the sandbox. No shell is involved on the host; `command[0]` is resolved once at
  startup.
- **Every input is validated** before it is substituted into a single argv element.
- **The process is contained in time and output.** Clean environment, closed stdin, a working
  directory inside the workspace, its own process group, a timeout, capped output, one action
  at a time. An action is one job: when it exits, background processes it left in its group
  are killed. A process that detached into a session of its own (`setsid`, a daemon) is out
  of reach there; Claustrum then stops reading the output after two seconds, notes it on
  stderr and moves on, so it cannot hold up later actions. Under bubblewrap such a process
  dies with the action's PID namespace; elsewhere it keeps running, so do not declare
  programs that leave daemons behind unless you want them.
- **The configuration stays what it was.** Protected files are snapshotted before and restored
  after every action.
- **The program is confined** by the OS sandbox: writes stay in the workspace, `$TMPDIR` and
  the action's `writable` list, credential stores are unreadable, and the network is only
  reachable through the [proxy](../network/gate.md). A single action can opt out with
  `confine = "none"`.
