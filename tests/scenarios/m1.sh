#!/usr/bin/env bash
# M1 (DESIGN.md §15.3, §10.5): edit/edit. Device B has a file open for writing
# and has written to it; device A writes the same file and uploads. B learns
# of the new revision while the file is open (event stream), and its upload
# after the close finds another revision. The server version stays at the
# name, B's content becomes a conflict copy, B lists one `edit_edit`. Both
# devices end with the same two files. Live mode, two profiles (lib.sh).
source "$(dirname "$0")/lib.sh"
prepare_two
make_workdir
scenario_area m1
b=$ROOT_B/m1
printf base >"$area/f"
wait_queue_empty
wait_content "$b/f" base

step "M1: B holds f open and has written; A writes f and uploads"
hold_open b "$b/f" b-content
printf a-content >"$area/f"
reach_b_while_held "$area" "$b" m1

for dir in "$area" "$b"; do
    expect_eq "f in $dir" "$(cat "$dir/f")" a-content
    copies=$(conflict_copies "$dir")
    [[ $(wc -l <<<"$copies") == 1 && -n "$copies" ]] || fail "expected one conflict copy in $dir, found: $copies"
    expect_eq "the conflict copy in $dir" "$(cat "$dir/$copies")" b-content
    [[ "$(ls "$dir" | wc -l)" == 2 ]] || fail "unexpected entries in $dir: $(ls "$dir")"
done
expect_conflicts 2 "$b" edit_edit
echo "M1: PASS"
