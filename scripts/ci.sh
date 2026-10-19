#!/usr/bin/env bash
# Local CI (TODO.md M0.7; hosted CI is deferred by decision D023).
#
# Runs formatting, lints, build and tests for the whole workspace. The SDK
# fork is fetched from the local bare repo through the file:// URL in
# Cargo.toml. Building proton-crypto needs Go, clang and protoc.
#
# Usage: scripts/ci.sh [--quick]   (--quick skips the release build)
set -euo pipefail

cd "$(dirname "$0")/.."

quick=0
[[ "${1:-}" == "--quick" ]] && quick=1

step() { printf '\n==> %s\n' "$*"; }

step "toolchain"
missing=()
for tool in cargo go clang protoc; do
    command -v "$tool" >/dev/null || missing+=("$tool")
done
if ((${#missing[@]})); then
    echo "missing build tools: ${missing[*]}" >&2
    exit 1
fi
cargo --version

step "format"
cargo fmt --all -- --check

step "clippy"
cargo clippy --workspace --all-targets --locked -- -D warnings

step "test"
cargo test --workspace --locked

if ((quick == 0)); then
    step "release build"
    cargo build --workspace --release --locked
fi

step "ok"
