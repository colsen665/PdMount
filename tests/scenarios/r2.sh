#!/usr/bin/env bash
# R2 (DESIGN.md §15.3): change content and mtime of k files and re-run
# rsync: exactly k files are transferred, one new remote revision each.
source "$(dirname "$0")/lib.sh"
prepare_mount
make_workdir
scenario_area r2
make_tree "$work/src" "${R_FILES:-200}" 0
rsync -a "$work/src/" "$area/dst/"
wait_queue_empty
k=7
mapfile -t files < <(cd "$work/src" && find . -name 'f-*.dat' -size +0 | sort | head -n "$k")
for file in "${files[@]}"; do
    printf 'changed %s\n' "$file" >>"$work/src/$file"
    touch -d @1800000000 "$work/src/$file"
done
mark=$(fake_change_mark)

step "R2 [$VERIFY_MODE]: rsync -a after changing $k files"
transferred=$(rsync -ai "$work/src/" "$area/dst/" | grep -c '^>f' || true)
[[ "$transferred" == "$k" ]] || fail "rsync transferred $transferred files, expected $k"
wait_queue_empty
if [[ $BACKEND == fake ]]; then
    revisions=$(fake_changes "$mark" | awk '$2 == "revision"' | wc -l)
    [[ "$revisions" == "$k" ]] || fail "$revisions new remote revisions, expected $k"
fi
verify_cycle
assert_same_tree "$work/src" "$TEST_ROOT/r2/dst"
check_hot_path
echo "R2 [$VERIFY_MODE]: PASS"
