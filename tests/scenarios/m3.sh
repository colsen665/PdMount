#!/usr/bin/env bash
# M3 (DESIGN.md §15.3, §10.5): remote delete against a local edit. Device B
# has a file open for writing and has written to it; device A deletes the
# file and uploads. B learns of the delete while the file is open; its edit
# wins: after the close the file exists again on both devices with B's
# content, and B lists one `remote_delete_local_edit`. Live mode.
source "$(dirname "$0")/lib.sh"
prepare_two
make_workdir
scenario_area m3
b=$ROOT_B/m3
printf base >"$area/f"
wait_queue_empty
wait_content "$b/f" base

step "M3: B holds f open and has written; A deletes f"
hold_open b "$b/f" b-content
rm "$area/f"
reach_b_while_held "$area" "$b" m3

for dir in "$area" "$b"; do
    expect_eq "entries of $dir" "$(ls "$dir")" f
    expect_eq "f in $dir" "$(cat "$dir/f")" b-content
done
expect_conflicts 2 "$b" remote_delete_local_edit
echo "M3: PASS"
