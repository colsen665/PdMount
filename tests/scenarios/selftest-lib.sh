#!/usr/bin/env bash
# Self-tests of tests/scenarios/lib.sh's safety checks (S-7, S-8): what the
# scripts may remove, and which roots they accept. Run by scripts/ci.sh.
# Needs no build and no mount.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
failed=0
check() { # <name> <expected: ok|refused> <command...>
    local name=$1 expected=$2
    shift 2
    local got=ok
    "$@" >"$tmp/out" 2>&1 || got=refused
    if [[ $got == "$expected" ]]; then
        echo "$name: PASS"
    else
        echo "$name: FAIL (expected $expected, got $got)"
        sed 's/^/    /' "$tmp/out"
        failed=1
    fi
}
holds() { # <name> <test...>
    local name=$1
    shift
    if "$@"; then echo "$name: PASS"; else echo "$name: FAIL"; failed=1; fi
}
# lib.sh in a subshell, with everything under $tmp.
with_lib() { # <SCENARIO_ROOT> <command...>
    local root=$1
    shift
    (
        export BACKEND=fake SCENARIO_ROOT=$root PDMOUNT=/nonexistent
        # shellcheck source=lib.sh
        source "$here/lib.sh"
        "$@"
    )
}

mkdir -p "$tmp/data"
echo keep >"$tmp/data/stray"
DATASET=$tmp/data check "a DATASET with other content is refused" refused with_lib "$tmp/root" ensure_dataset
holds "the stray file survives" test -f "$tmp/data/stray"

mkdir -p "$tmp/elsewhere"
ln -s "$tmp/elsewhere" "$tmp/link"
check "a SCENARIO_ROOT that is a symlink is refused" refused with_lib "$tmp/link" true
check "a new SCENARIO_ROOT is accepted" ok with_lib "$tmp/root" true
holds "the root is 0700" test "$(stat -c %a "$tmp/root")" = 700

mkdir -p "$tmp/notfake"
echo keep >"$tmp/notfake/file"
check "a FAKE_DIR that is not a fake drive is refused" refused with_lib "$tmp/root" remove_fake_dir "$tmp/notfake"
holds "its file survives" test -f "$tmp/notfake/file"
mkdir -p "$tmp/fake"
touch "$tmp/fake/snapshot.json"
check "a fake drive is removed" ok with_lib "$tmp/root" remove_fake_dir "$tmp/fake"
holds "it is gone" test ! -e "$tmp/fake"
exit "$failed"
