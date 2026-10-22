#!/usr/bin/env bash
# R1 (DESIGN.md §15.3): `rsync -a` a mixed tree into the mount. Afterwards
# rsync finds nothing to do, by mtime and size and by checksum. In fresh
# mode this checks that size and mtime come back right from the backend.
source "$(dirname "$0")/lib.sh"
prepare_mount
make_workdir
scenario_area r1
make_tree "$work/src" "${R_FILES:-1000}" "${R_BIG_MIB:-100}"

step "R1 [$VERIFY_MODE]: rsync -a into the mount"
time rsync -a "$work/src/" "$area/dst/"
verify_cycle
assert_same_tree "$work/src" "$TEST_ROOT/r1/dst"
[[ -z "$(remote_temp_names)" ]] || fail "temporary names reached the backend: $(remote_temp_names)"
check_hot_path
echo "R1 [$VERIFY_MODE]: PASS"
