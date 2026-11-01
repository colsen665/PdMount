#!/usr/bin/env bash
# Local CI (TODO.md M0.7; hosted CI is deferred by decision D023).
#
# Runs formatting, lints, build and tests for the whole workspace. The SDK
# fork is fetched from the local bare repo through the file:// URL in
# Cargo.toml. Building proton-crypto needs Go, clang and protoc.
#
# Usage: scripts/ci.sh [--quick | --full]
#   --quick skips the release build and the scenario scripts.
#   --full  runs every scenario in every verify mode, `second` included (the
#           M scenarios and e2 only in `live`), and the property tests at
#           10 000 cases.
# By default a representative set of scenarios runs in `live` and `crash`
# mode, four of them also in `second` mode (a second device receives what the
# first one did), and the two-device scenarios m1..m7 and e1/e2 in `live`
# mode. Scenarios run over the fake backend (BACKEND=fake, D030), with
# profiles, mountpoints and a fake drive of their own in tmpfs (SCENARIO_ROOT,
# default /dev/shm/pdmount-tests).
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
    step "property tests, 10 000 cases"
    PROPTEST_CASES=10000 cargo test -p pdmount-core --locked proptests
fi

step "script self-tests"
tests/scenarios/selftest-lib.sh

if ((quick == 0)); then
    step "release build"
    cargo build --workspace --release --locked

    step "scenarios over the fake backend"
    cargo build -p pdmount --features fake-backend --locked
    export BACKEND=fake PDMOUNT=$PWD/target/debug/pdmount
    if ((full)); then
        MODES="live remount crash fresh second" tests/scenarios/run-all.sh
    else
        R_FILES=200 R_BIG_MIB=10 MODES="live crash" \
            tests/scenarios/run-all.sh r1 r3 r4 r6 r7 g3 g4 g6 g6_read n1 a1 a6 sha
        R_FILES=200 R_BIG_MIB=10 G3_COMMITS=30 MODES="second" tests/scenarios/run-all.sh r1 r3 g3 a1
        MODES="live" tests/scenarios/run-all.sh e1 e2 m1 m3 m4 m5 m6 m7 queue ops
    fi
    kill9_log=$(mktemp "${TMPDIR:-/tmp}/pdmount-kill9.XXXXXX")
    tests/scenarios/kill9.sh >"$kill9_log" 2>&1 || { echo "scenario kill9 failed; log: $kill9_log" >&2; exit 1; }
    rm -f "$kill9_log"
    echo "kill9: PASS"
    # Removing the runtime directory, as a reboot does, makes the next run's first mount create it.
    (source tests/scenarios/lib.sh && { pd unmount >/dev/null; pd2 unmount >/dev/null 2>&1 || true; } &&
        rm -rf "$XDG_RUNTIME_DIR")
fi

step "ok"
