#!/usr/bin/env bash
# Local CI (TODO.md M0.7; hosted CI is deferred by decision D023).
#
# Runs formatting, lints, build and tests for the whole workspace. The SDK
# fork is fetched from the local bare repo through the file:// URL in
# Cargo.toml. Building proton-crypto needs Go, clang and protoc.
#
# Usage: scripts/ci.sh [--quick | --full]
#   --quick skips the release build and the scenario scripts.
#   --full  runs every scenario in every verify mode (about an hour) and the
#           journal property tests at 10 000 cases.
# By default a representative set of scenarios runs in `live` and `crash`
# mode. Scenarios run over the fake backend (BACKEND=fake, D030), with a
# profile, mountpoint and fake drive of their own in tmpfs (SCENARIO_ROOT, default
# /dev/shm/pdmount-tests).
set -euo pipefail

cd "$(dirname "$0")/.."

quick=0
full=0
case "${1:-}" in
    --quick) quick=1 ;;
    --full) full=1 ;;
    "") ;;
    *) echo "usage: $0 [--quick | --full]" >&2; exit 2 ;;
esac

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
cargo clippy -p pdmount --all-targets --features fake-backend --locked -- -D warnings

step "test"
cargo test --workspace --locked
if ((full)); then
    step "journal property tests, 10 000 cases"
    PROPTEST_CASES=10000 cargo test -p pdmount-core --locked proptests
fi

if ((quick == 0)); then
    step "release build"
    cargo build --workspace --release --locked

    step "scenarios over the fake backend"
    cargo build -p pdmount --features fake-backend --locked
    export BACKEND=fake PDMOUNT=$PWD/target/debug/pdmount
    if ((full)); then
        tests/scenarios/run-all.sh
    else
        R_FILES=200 R_BIG_MIB=10 MODES="live crash" \
            tests/scenarios/run-all.sh r1 r3 r4 r6 r7 g3 g4 g6 g6_read n1 a1 a6 sha
    fi
    tests/scenarios/kill9.sh >/dev/null || { echo "scenario kill9 failed" >&2; exit 1; }
    echo "kill9: PASS"
    (source tests/scenarios/lib.sh && pd unmount >/dev/null)
fi

step "ok"
