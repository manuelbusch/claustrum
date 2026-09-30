# Packages

The commands inside the sandbox come from packages: `.webc` files from the
[Wasmer registry](https://wasmer.io/explore). Only commands provided by a loaded package
exist in `/bin` and `/usr/bin`; `claustrum pkg commands` lists them.

## Default packages

| Package | Provides | Notes |
| --- | --- | --- |
| `wasmer/bash` | `bash`, `sh` | the shell behind the `Bash` tool |
| `wasmer/coreutils` | `ls`, `cat`, `cp`, `mv`, `sort`, `wc`, ... | WASIX build of coreutils |
| `python/python` | `python`, `python3`, `pip` | CPython 3.13 with the standard library, about 60 MB; compiles for close to a minute on first use, which is why `pkg sync` precompiles |
| `syrusakbary/jq` | `jq` | jq 1.6 |

The packages are downloaded from the Wasmer registry at runtime and are not part of
Claustrum; they keep their own licenses (bash, for example, is GPL).

## Downloading and pinning

`claustrum pkg sync` downloads the configured packages into the user data directory (or
[`[packages] dir`](configuration/reference.md#packages)) and records each package's
identity in a `.webc.id` file next to it, which is how dependencies between packages (bash
depends on coreutils) resolve offline. Files obtained some other way can be given an explicit
`id` in the configuration.

The default packages are pinned to fixed versions (`wasmer/coreutils@=1.0.25`, ...), so a new
release in the registry does not change the sandbox. In your own `[[packages.package]]`
entries, pin with `@=<version>`: a bare `@<version>` is a semver requirement and takes the
newest compatible release. To upgrade, change the version and run
`claustrum pkg sync --force`; without `--force`, packages already present are kept.

```toml
[[packages.package]]
file = "python.webc"
source = "python/python@=3.13.20"
```

Configuring your own list replaces the defaults, so list every package you want, including
bash and coreutils. The full default list is in
[`claustrum.example.toml`](https://github.com/manuelbusch/claustrum/blob/main/claustrum.example.toml).

Other package commands:

- `claustrum pkg add <spec>` downloads a single package, e.g. `python/python`.
- `claustrum pkg list` shows the configured packages and whether they are installed.
- `claustrum pkg precompile` compiles all installed packages into the user-wide module
  cache; `pkg sync` does this already.

## Self-built tools

Self-built tools can be added without the `wasmer` CLI: compile a Rust program with
[`cargo wasix`](https://github.com/wasix-org/cargo-wasix) and register the result with
`claustrum pkg add-wasm <name> <file.wasm>`, which writes a directory package (`wasmer.toml`
plus the module) into the packages directory. `--alias cmd` adds further command names
served by the same module. Then add it to the configuration:

```toml
[[packages.package]]
file = "jaq"
```

[`scripts/build-jaq.sh`](https://github.com/manuelbusch/claustrum/blob/main/scripts/build-jaq.sh)
does this for [jaq](https://github.com/01mf02/jaq), a current jq implementation in Rust.

## git is not available yet

No WASIX build of git is published anywhere; the only known recipe is the
[wasinix](https://github.com/wasix-org/wasinix) Nix flake, which builds on x86_64 Linux only
and needs a patched runtime. gitoxide does not target WASIX either. Until that changes, git
operations happen on the host, for example through a [host action](host-actions/index.md);
the [Rust example](host-actions/rust-example.md) contains a set of local git actions.
