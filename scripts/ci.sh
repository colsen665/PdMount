#!/usr/bin/env bash
# Local CI (TODO.md M0.7). The GitHub workflow ci.yml runs --quick on pushes and the full run weekly (D097).
#
# Runs formatting, lints, build and tests for the whole workspace. The SDK
# fork is fetched from GitHub at the commit pinned in
# Cargo.toml. Building proton-crypto needs Go, clang and protoc.
#
# Usage: scripts/ci.sh [--quick | --full] [--fail-fast]
#   --quick     skips the release build and the scenario scripts.
#   --full      runs every scenario in every verify mode, `second` included (the
#               M scenarios, e2 and the X scenarios only in `live`), and the property tests at
#               10 000 cases.
#   --fail-fast stops at the first failed step. Without it every step runs and the
#               summary at the end lists each result (D098); the exit code is 1 if any
#               step failed or was skipped because a step it needs failed.
# By default a representative set of scenarios runs in `live` and `crash`
# mode, four of them also in `second` mode (a second device receives what the
# first one did), and the two-device scenarios m1..m7 and e1/e2, queue, ops
# and the stop-flush scenarios x1..x4 and x11, the conflict notifications x6, the pending marker x12, the desktop bus x13, the further roots x14, the Shared mount x15, writes into shares x16, the Photos mount x17, unverified signatures x18, `cache wipe` in every mount x19 and the slow-download notification x20 in `live` mode, the test of `scripts/repro/m6-desktop.sh` (`m6desktop`), and r1 and g1 in a share's tree (TARGET=share; with --full r1 g1 n1 m1 m3 in every mode). Scenarios run over the fake backend (BACKEND=fake, D030), with
# profiles, mountpoints and a fake drive of their own in tmpfs (SCENARIO_ROOT).
# Unless SCENARIO_ROOT is set, each checkout gets its own root,
# /dev/shm/pdmount-ci-<checksum of the checkout's path>, so runs in different
# worktrees can go on at once; a second run in the same checkout is refused (D098).
# clean_up sources lib.sh in a subshell on purpose: its variables stay there.
# shellcheck disable=SC2031
set -uo pipefail

cd "$(dirname "$0")/.." || exit 2

quick=0
full=0
fail_fast=0
for arg in "$@"; do
    case "$arg" in
        --quick) quick=1 ;;
        --full) full=1 ;;
        --fail-fast) fail_fast=1 ;;
        *) echo "usage: $0 [--quick | --full] [--fail-fast]" >&2; exit 2 ;;
    esac
done
if ((quick && full)); then
    echo "usage: $0 [--quick | --full] [--fail-fast]" >&2
    exit 2
fi

step() { printf '\n==> %s\n' "$*"; }

names=()
declare -A result=() seconds=()

summary() {
    local name failed=()
    step "summary"
    for name in "${names[@]}"; do
        printf '%-4s %5s s  %s\n' "${result[$name]}" "${seconds[$name]}" "$name"
        [[ ${result[$name]} == PASS ]] || failed+=("$name")
    done
    if ((${#failed[@]})); then
        step "FAILED: ${failed[*]}"
        exit 1
    fi
    step "ok"
}

# run <name> <command...>: runs one step, records its result and goes on (or stops
# with --fail-fast).
run() {
    local name=$1 start code
    shift
    step "$name"
    names+=("$name")
    start=$SECONDS
    "$@"
    code=$?
    seconds[$name]=$((SECONDS - start))
    if ((code == 0)); then
        result[$name]=PASS
    else
        result[$name]=FAIL
        echo "step '$name' failed (exit $code)" >&2
        ((fail_fast)) && summary
    fi
    return 0
}

# run_after <needed step> <name> <command...>: like run, but skipped when the
# step it needs did not pass.
run_after() {
    local needed=$1 name=$2
    if [[ ${result[$needed]:-} != PASS ]]; then
        step "$name"
        echo "skipped: '$needed' did not pass" >&2
        names+=("$name")
        result[$name]=SKIP
        seconds[$name]=0
        ((fail_fast)) && summary
        return 0
    fi
    shift
    run "$@"
}

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

run "format" cargo fmt --all -- --check
run "clippy" cargo clippy --workspace --all-targets --locked -- -D warnings
run "clippy, fake backend" cargo clippy -p pdmount --all-targets --features fake-backend --locked -- -D warnings
run "test" cargo test --workspace --locked
if ((full)); then
    run "property tests, 10 000 cases" env PROPTEST_CASES=10000 cargo test -p pdmount-core --locked proptests
fi
run "script self-tests" tests/scenarios/selftest-lib.sh

kill9() {
    local log
    log=$(mktemp "${TMPDIR:-/tmp}/pdmount-kill9.XXXXXX")
    if tests/scenarios/kill9.sh >"$log" 2>&1; then
        rm -f "$log"
        echo "kill9: PASS"
    else
        echo "scenario kill9 failed; log: $log" >&2
        return 1
    fi
}

# Removing the runtime directory, as a reboot does, makes the next run's first mount create it.
clean_up() {
    # shellcheck source=tests/scenarios/lib.sh
    # The mountpoints are named: without one, `unmount` takes the profile's default, ~/ProtonDrive, and would
    # unmount a real drive mounted there. The profiles of the scenarios in a share's tree (TARGET=share) first,
    # while their sockets are there.
    (TARGET=share && source tests/scenarios/lib.sh &&
        { pd unmount "$MNT" >/dev/null 2>&1 || true; pd2 unmount "$MNT2" >/dev/null 2>&1 || true; })
    (source tests/scenarios/lib.sh && { pd unmount "$MNT" >/dev/null; pd2 unmount "$MNT2" >/dev/null 2>&1 || true; } &&
        rm -rf "$XDG_RUNTIME_DIR")
}

if ((quick == 0)); then
    if [[ -z ${SCENARIO_ROOT:-} ]]; then
        checkout=$(pwd -P | cksum | cut -d' ' -f1)
        if [[ -d /dev/shm && -w /dev/shm ]]; then
            SCENARIO_ROOT=/dev/shm/pdmount-ci-$checkout
        else
            SCENARIO_ROOT=${XDG_CACHE_HOME:-$HOME/.cache}/pdmount-ci-$checkout
        fi
    fi
    export SCENARIO_ROOT
    # The lock lives inside the root, made private first as lib.sh's
    # private_root does: a lock file beside it would be a name in /dev/shm
    # that another user could take first (V-13).
    # shellcheck disable=SC2174
    mkdir -p -m 0700 "$SCENARIO_ROOT"
    if [[ -L $SCENARIO_ROOT || ! -O $SCENARIO_ROOT ]]; then
        echo "refusing SCENARIO_ROOT=$SCENARIO_ROOT: it is a symlink or belongs to another user" >&2
        exit 1
    fi
    chmod 0700 "$SCENARIO_ROOT"
    exec 9>"$SCENARIO_ROOT/.lock"
    if ! flock -n 9; then
        echo "another scripts/ci.sh uses SCENARIO_ROOT=$SCENARIO_ROOT; wait for it or set SCENARIO_ROOT" >&2
        exit 1
    fi
    echo "SCENARIO_ROOT=$SCENARIO_ROOT"

    run "release build" cargo build --workspace --release --locked
    run "fake-backend build" cargo build -p pdmount --features fake-backend --locked
    export BACKEND=fake PDMOUNT=$PWD/target/debug/pdmount
    if ((full)); then
        run_after "fake-backend build" "scenarios, every mode" \
            env MODES="live remount crash fresh second" tests/scenarios/run-all.sh
        run_after "fake-backend build" "scenarios in a share, every mode" \
            env TARGET=share MODES="live remount crash fresh" tests/scenarios/run-all.sh r1 g1 n1 m1 m3
    else
        run_after "fake-backend build" "scenarios, live and crash" \
            env R_FILES=200 R_BIG_MIB=10 MODES="live crash" \
            tests/scenarios/run-all.sh r1 r3 r4 r6 r7 g3 g4 g6 g6_read n1 a1 a6 sha
        run_after "fake-backend build" "scenarios, second" \
            env R_FILES=200 R_BIG_MIB=10 G3_COMMITS=30 MODES="second" tests/scenarios/run-all.sh r1 r3 g3 a1
        run_after "fake-backend build" "scenarios, live" \
            env MODES="live" tests/scenarios/run-all.sh e1 e2 m1 m3 m4 m5 m6 m7 queue ops svc x1 x2 x3 x4 x6 x11 x12 x13 x14 x15 x16 x17 x18 x19 x20
        run_after "fake-backend build" "scenarios in a share, live" \
            env TARGET=share R_FILES=200 R_BIG_MIB=10 MODES="live" tests/scenarios/run-all.sh r1 g1
    fi
    run_after "fake-backend build" "m6-desktop script" tests/scenarios/m6desktop.sh
    run_after "fake-backend build" "kill9" kill9
    run_after "fake-backend build" "clean-up" clean_up
fi

summary
