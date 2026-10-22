#!/usr/bin/env bash
# R7 (DESIGN.md §15.3): interrupt rsync mid-transfer and re-run it. The
# final state is right, and no `.name.XXXXXX` temporary remains locally or
# was created remotely.
source "$(dirname "$0")/lib.sh"
prepare_mount
make_workdir
scenario_area r7
make_tree "$work/src" "${R_FILES:-300}" "${R7_MIB:-30}"

step "R7 [$VERIFY_MODE]: interrupt rsync, then re-run it"
timeout -s INT 2 rsync -a --bwlimit=4000 "$work/src/" "$area/dst/" || true
rsync -a "$work/src/" "$area/dst/"
verify_cycle
assert_same_tree "$work/src" "$TEST_ROOT/r7/dst"
leftover=$(find "$TEST_ROOT/r7/dst" -name '.*.??????' | head -5)
[[ -z "$leftover" ]] || fail "rsync temporaries remain: $leftover"
[[ -z "$(remote_temp_names)" ]] || fail "temporary names reached the backend: $(remote_temp_names)"
check_hot_path
echo "R7 [$VERIFY_MODE]: PASS"
