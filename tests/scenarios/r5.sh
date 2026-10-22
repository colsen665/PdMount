#!/usr/bin/env bash
# R5 (DESIGN.md §15.3): `touch` some files only and re-run rsync: their
# mtimes are right afterwards in every mode (metadata-only revisions).
source "$(dirname "$0")/lib.sh"
prepare_mount
make_workdir
scenario_area r5
make_tree "$work/src" "${R_FILES:-100}" 0
rsync -a "$work/src/" "$area/dst/"
wait_queue_empty
mapfile -t files < <(cd "$work/src" && find . -name 'f-*.dat' | sort | awk 'NR % 9 == 0')
for file in "${files[@]}"; do touch -d @1811111111 "$work/src/$file"; done

step "R5 [$VERIFY_MODE]: rsync -a after touching ${#files[@]} files"
changed=$(rsync -ai "$work/src/" "$area/dst/" | grep -cE '^[.>]f' || true)
[[ "$changed" == "${#files[@]}" ]] || fail "rsync updated $changed files, expected ${#files[@]}"
verify_cycle
assert_same_tree "$work/src" "$TEST_ROOT/r5/dst"
check_hot_path
echo "R5 [$VERIFY_MODE]: PASS"
