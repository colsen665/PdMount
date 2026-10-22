#!/usr/bin/env bash
# A2 (DESIGN.md §15.3): `access set rw <dir>/sub` under a `ro` parent:
# writes in sub succeed, writes elsewhere in <dir> still fail.
source "$(dirname "$0")/lib.sh"
source "$(dirname "$0")/access-lib.sh"
prepare_mount
scenario_area a2
dir=$area/dir
mkdir -p "$dir/sub"

step "A2 [$VERIFY_MODE]: rw below ro"
pd access set ro "$dir"
pd access set rw "$dir/sub"
expect_ok "write in sub" sh -c "printf inside >'$dir/sub/ok'"
expect_erofs "write beside sub" sh -c "printf beside >'$dir/no'"
verify_cycle
dir=$TEST_ROOT/a2/dir
[[ "$(cat "$dir/sub/ok")" == inside ]] || fail "the file written in sub is wrong"
[[ ! -e "$dir/no" ]] || fail "the refused file exists"
if rules_kept; then
    expect_erofs "write beside sub after $VERIFY_MODE" sh -c "printf beside >'$dir/no'"
    pd access unset "$dir/sub"
    pd access unset "$dir"
fi
check_hot_path
echo "A2 [$VERIFY_MODE]: PASS"
