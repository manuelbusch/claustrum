# Example: a Rust project

[`examples/rust.claustrum.toml`](https://github.com/manuelbusch/claustrum/blob/main/examples/rust.claustrum.toml)
is a complete configuration for developing a Rust project. Copy it to `claustrum.toml` next
to the project's `Cargo.toml` and trust it with `claustrum trust`.

## The picture

Claude edits the sources inside the sandbox, where neither cargo nor rustc exist (they cannot
run under WASIX). Building and testing happen on the host through actions, each running
confined and with a fixed argv. The dependency sources are mounted read-only so Claude can
read the code of the crates it uses.

## The pieces

**Confinement is required.** cargo executes code Claude can write: `build.rs`, proc macros
and `.cargo/config.toml` (`rustc-wrapper`, `runner`). With `confinement = "required"` a
missing backend refuses to start instead of running cargo unconfined.

**The network is limited to crates.io.** cargo fetches the sparse index and the `.crate`
files from two hosts:

```toml
[network]
mode = "allowlist"
allow = ["index.crates.io", "static.crates.io"]
```

`cargo add`, `cargo update` and the first build after a dependency change need them; a
project with a complete `Cargo.lock` and a warm registry cache also works with
`mode = "disabled"`. Git dependencies need their forge as well.

**Dependency sources are mounted read-only**, so Claude can look up how a crate works instead
of guessing:

```toml
[[mounts]]
guest = "/cargo/registry"
host = "~/.cargo/registry/src"
```

**cargo actions**: `check`, `build` (input `profile`: `dev` or `release`), `test`,
`test-crate` (inputs `crate` and an optional test name `filter`), `clippy`, `fmt`,
`fmt-check`, `doc`, and for dependencies `add-dep` and `update`. They may write the workspace,
their private `$TMPDIR` and `~/.cargo/registry`; `~/.cargo/bin` and `~/.cargo/config.toml`
stay read-only, so no action can install a binary or a rustc wrapper that later runs on the
host.

**git actions** cover the local workflow: `status`, `diff`, `diff-staged`, `log`,
`branches`, `add`, `unstage`, `commit`, `commit-file`, `switch`, `switch-new`, `merge`,
`merge-abort`. Nothing talks to a remote: push, fetch and pull are deliberately absent,
because they would ship whatever the workspace contains. Every git action starts with fixed
`-c` overrides (`core.hooksPath=/dev/null`, `core.fsmonitor=false`, and
`commit.gpgsign=false` where it commits), which win over `.git/config`, so hooks that Claude
could point at its own scripts never run.

Inputs cannot contain line breaks, so `commit-file` reads a multi-line commit message from a
file in the workspace:

```sh
host commit-file target/commit-msg.txt
```

**The system prompt** tells Claude that cargo, rustc and git are not available in the
sandbox and which actions replace them.

## Adapting it

- Add `"~/.cargo/git"` to `writable` (and the forge to `allow`) for git dependencies.
- Mount the standard library sources (`rustup component add rust-src`) as a second read-only
  mount; the file contains a commented template.
- Remove the git actions if you prefer to commit yourself.
