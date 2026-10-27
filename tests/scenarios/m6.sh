#!/usr/bin/env bash
# M6 (DESIGN.md §15.3, §10.5): a folder deleted on one device, a new child on
# the other. B has d/new open for writing; A deletes d (with its clean files
# a and b) and uploads. B learns of it while the file is open and keeps the
# minimal chain: after the close both devices have d/ with only `new`.
source "$(dirname "$0")/lib.sh"
prepare_two
make_workdir
scenario_area m6
b=$ROOT_B/m6
mkdir "$area/d"
printf one >"$area/d/a"
printf two >"$area/d/b"
wait_queue_empty
wait_content "$b/d/b" two

step "M6: B holds d/new open; A deletes d"
hold_open b "$b/d/new" new-content
rm -r "$area/d"
reach_b_while_held "$area" "$b" m6

for dir in "$area" "$b"; do
    expect_eq "entries of $dir" "$(ls "$dir")" d
    expect_eq "entries of $dir/d" "$(ls "$dir/d")" new
    expect_eq "d/new in $dir" "$(cat "$dir/d/new")" new-content
done
echo "M6: PASS"
