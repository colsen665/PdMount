#!/usr/bin/env bash
# R3 (DESIGN.md §15.3): delete files and directories in the source and run
# `rsync -a --delete`. They are gone in every mode, and no temporary name
# was ever created remotely.
source "$(dirname "$0")/lib.sh"
prepare_mount
make_workdir
scenario_area r3
make_tree "$work/src" "${R_FILES:-200}" 0
rsync -a "$work/src/" "$area/dst/"
wait_queue_empty
(cd "$work/src" && find . -name 'f-*.dat' | sort | awk 'NR % 5 == 0' | xargs rm -f)
rm -rf "$work/src/dir-001" "$work/src/dir-004"

step "R3 [$VERIFY_MODE]: rsync -a --delete"
rsync -a --delete "$work/src/" "$area/dst/"
verify_cycle
assert_same_tree "$work/src" "$TEST_ROOT/r3/dst"
out=$(rsync -ani --delete $(dir_time_flags) "$work/src/" "$TEST_ROOT/r3/dst/")
[[ -z "$out" ]] || fail "deleted entries are back: $out"
[[ -z "$(remote_temp_names)" ]] || fail "temporary names reached the backend: $(remote_temp_names)"
check_hot_path
echo "R3 [$VERIFY_MODE]: PASS"
