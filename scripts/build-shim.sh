#!/usr/bin/env bash
# Rebuild the guest shim that forwards host commands (git, ...) to Claustrum.
# Requires: rustup target add wasm32-wasip1
set -euo pipefail
cd "$(dirname "$0")/../shim"
cargo build --release --target wasm32-wasip1
mkdir -p ../crates/claustrum-sandbox/assets
cp target/wasm32-wasip1/release/hostcmd.wasm ../crates/claustrum-sandbox/assets/hostcmd.wasm
ls -la ../crates/claustrum-sandbox/assets/hostcmd.wasm
