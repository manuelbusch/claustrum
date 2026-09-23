#!/usr/bin/env bash
# Build jaq (a jq clone written in Rust, https://github.com/01mf02/jaq) for
# WASIX and register it with Claustrum as the `jq` command.
#
# The registry package `syrusakbary/jq` (jq 1.6 from 2019) works out of the
# box and is what `claustrum pkg sync` installs; use this script when you want
# a current jq implementation instead.
#
# Requirements: Rust, `cargo install cargo-wasix` (downloads the WASIX
# toolchain on first use, ~1 GB), git. Takes a few minutes.
#
# Usage: scripts/build-jaq.sh [claustrum args…]   e.g. --packages-dir packages

set -euo pipefail

JAQ_VERSION="${JAQ_VERSION:-v3.1.1}"
WORK="${WORK:-$(mktemp -d)}"
CLAUSTRUM="${CLAUSTRUM:-claustrum}"

echo "building jaq $JAQ_VERSION in $WORK"
git clone --quiet --depth 1 --branch "$JAQ_VERSION" https://github.com/01mf02/jaq.git "$WORK/jaq"
cd "$WORK/jaq"

# The committed lock file pins crates that the WASIX overlay registry replaces.
rm -f Cargo.lock
# rustyline's file history pulls in fd-lock, which does not build for WASIX.
# Only the interactive REPL history is affected.
sed -i.bak 's/^rustyline = { \(.*\)features = \[[^]]*\]/rustyline = { \1features = []/' jaq/Cargo.toml
sed -i.bak '/rl.load_history(h)/d; /rl.append_history(h)/d' jaq/src/funs.rs

# mimalloc (default feature) needs C headers the bundled clang lacks.
CARGO_HTTP_MULTIPLEXING=false cargo wasix build --release -p jaq --no-default-features

"$CLAUSTRUM" "$@" pkg add-wasm jaq target/wasm32-wasmer-wasi/release/jaq.wasm --alias jq
echo
echo 'Add to claustrum.toml (and remove the jq.webc entry if present):'
echo
echo '[[packages.package]]'
echo 'file = "jaq"'
