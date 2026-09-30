# Installation

## Requirements

- Rust 1.95+ (edition 2024) to build Claustrum.
- A working `claude` (Claude Code) on your `PATH`, or its path in
  [`[claude] binary`](configuration/reference.md#claude).
- For OS confinement on Linux: Linux 5.13+ with Landlock enabled, ideally with unprivileged
  user namespaces so that bubblewrap can be used. macOS uses the built-in Seatbelt. See
  [OS confinement](security/confinement.md).

## Install the binary

From a checkout of the repository:

```sh
cargo install --path crates/claustrum-cli   # installs the `claustrum` binary
```

## Download the packages

The commands inside the sandbox (`bash`, the coreutils, `python`, `jq`) are WebAssembly
packages from the Wasmer registry. Download them once and compile them ahead of time:

```sh
claustrum pkg sync
```

`pkg sync` stores the packages in the user data directory and precompiles them into the
user-wide module cache. Precompiling matters: Python in particular compiles for close to a
minute on first use. Details, including how to add your own packages, are in
[Packages](packages.md).

Next: [Your first session](getting-started.md).
