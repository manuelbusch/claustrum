# Use with care

Every action is a hole in the sandbox that you cut on purpose, and the validation only guards
the edges you declared. Confinement narrows what the program can do, but it still runs as
your user with read access to most of the file system.

| Risk | Example | Mitigation |
| --- | --- | --- |
| Programs execute code from the workspace, which Claude can write | `cargo` honours `.cargo/config.toml`, `build.rs`, proc macros; `git` honours `.git/config` and hooks; `npm` runs `package.json` scripts; `make` runs the Makefile | keep such actions confined; never `confine = "none"` for them |
| Validated inputs are still interpreted by the program | `ext::sh -c …` is a valid git URL, `@file` reads a file for curl, `key=value` after `git -c` changes behaviour | keep patterns to the characters the program needs; check how it treats them |
| Placeholders reaching an interpreter | `sh -c "… {x}"`, `python -c "{x}"` | refused at startup; put the code in a script and pass the input as an argument |
| Environment values are inputs too | `RUSTFLAGS = "{flags}"`, `env_passthrough` of `PATH`, `LD_PRELOAD`, `GIT_SSH_COMMAND` | warned at startup; avoid |
| Path inputs are checked before the program opens them | a file replaced by a symlink between check and open | do not rely on `path` inputs to keep a program away from host files |
| Side effects leave the sandbox | `git push`, `npm publish` ship whatever the workspace contains | only declare commands you would let Claude run on the host directly |
| Writable caches | `~/.cargo/bin`, `~/.cargo/config.toml`, shell profiles | never make a directory writable that the host executes from later |

Claustrum refuses placeholders that reach an interpreter at startup and warns about
placeholders in environment values, risky variables, patterns that accept almost anything and
`confine = "none"` for programs that run workspace code. The other risks in the table are not
detected; they are up to you. Without OS confinement (`confine = "none"`,
`confinement = "off"`, or a platform without a backend), an action is as trusted as the host
command behind it.

## Rules of thumb

- Declare no inputs when you can, and keep patterns as narrow as possible.
- Only declare commands you would let Claude run on the host directly.
- For programs that run workspace code, set
  [`[sandbox] confinement = "required"`](../security/confinement.md) so that a missing OS
  backend refuses to start instead of running them unconfined.
