#!/usr/bin/env bash
# Canonical verification entrypoint: every gate CI enforces, as one
# argument-free command. PR descriptions should cite this script (or the
# exact commands actually executed) as verification evidence, never
# reconstructed command lines.
set -euo pipefail
cd "$(dirname "$0")/.."

image=mcp-matrix-rs:verify

cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
docker build -t "$image" .
smoke/image.sh "$image"

echo "verify: all gates passed"
