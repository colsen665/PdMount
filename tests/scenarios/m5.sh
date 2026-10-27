#!/usr/bin/env bash
# M5 (DESIGN.md §15.3, §10.5): rename/rename, on the fake only (A has to be
# offline). A renames a file while it cannot reach the drive, B renames the
# same file to another name and uploads. The first committer wins: only B's
# name exists on both devices, and A lists one `rename_rename`.
source "$(dirname "$0")/lib.sh"
[[ $BACKEND == fake ]] || { echo "M5 needs an offline device; skipped for BACKEND=$BACKEND"; exit 0; }
prepare_two
make_workdir
scenario_area m5
b=$ROOT_B/m5
printf content >"$area/f"
wait_queue_empty
wait_content "$b/f" content

step "M5: A offline renames f to g; B renames f to h"
a_offline
mv "$area/f" "$area/g"
mv "$b/f" "$b/h"
a_back

for dir in "$area" "$b"; do
    expect_eq "entries of $dir" "$(ls "$dir")" h
    expect_eq "h in $dir" "$(cat "$dir/h")" content
done
expect_conflicts 1 "$area" rename_rename
echo "M5: PASS"
