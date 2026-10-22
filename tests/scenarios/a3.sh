#!/usr/bin/env bash
# A3 (DESIGN.md §15.3): `access set rw <dir>` or `access unset` at runtime:
# everything succeeds again without a remount, and the original modes
# reappear.
source "$(dirname "$0")/lib.sh"
source "$(dirname "$0")/access-lib.sh"
prepare_mount
scenario_area a3
dir=$area/dir
mkdir -p "$dir"
printf x >"$dir/file"
chmod 0640 "$dir/file"

step "A3 [$VERIFY_MODE]: back to rw at runtime"
pd access set ro "$dir"
[[ "$(stat -c %a "$dir/file")" == 440 ]] || fail "write bits shown in the ro subtree"
pd access set rw "$dir"
[[ "$(stat -c %a "$dir/file")" == 640 ]] || fail "mode $(stat -c %a "$dir/file") after rw, expected 640"
expect_ok "write after rw" sh -c "printf y >>'$dir/file'"
pd access set ro "$dir"
pd access unset "$dir"
expect_ok "write after unset" sh -c "printf z >>'$dir/file'"
verify_cycle
dir=$TEST_ROOT/a3/dir
[[ "$(cat "$dir/file")" == xyz ]] || fail "content is $(cat "$dir/file"), expected xyz"
[[ "$(stat -c %a "$dir/file")" == 640 ]] || fail "mode $(stat -c %a "$dir/file") after $VERIFY_MODE"
check_hot_path
echo "A3 [$VERIFY_MODE]: PASS"
