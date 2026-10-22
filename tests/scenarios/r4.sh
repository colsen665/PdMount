#!/usr/bin/env bash
# R4 (DESIGN.md §15.3): `rsync --inplace --no-whole-file` changes the
# middle of a file over 40 MiB; the result has the right sha256 in every
# mode (partial overwrites materialize only the blocks they touch).
source "$(dirname "$0")/lib.sh"
prepare_mount
make_workdir
scenario_area r4
make_tree "$work/src" 0 "${R4_MIB:-48}"
rsync -a "$work/src/" "$area/dst/"
wait_queue_empty
# 1 MiB in the middle, not block aligned.
head -c $((1 << 20)) /dev/urandom |
    dd of="$work/src/big.bin" bs=1 seek=$((20 * 1024 * 1024 + 12345)) conv=notrunc status=none

step "R4 [$VERIFY_MODE]: rsync --inplace --no-whole-file"
rsync -a --inplace --no-whole-file "$work/src/" "$area/dst/"
verify_cycle
expected=$(sha256sum <"$work/src/big.bin")
actual=$(sha256sum <"$TEST_ROOT/r4/dst/big.bin")
[[ "$actual" == "$expected" ]] || fail "big.bin has the wrong content after the in-place update"
assert_same_tree "$work/src" "$TEST_ROOT/r4/dst"
check_hot_path
echo "R4 [$VERIFY_MODE]: PASS"
