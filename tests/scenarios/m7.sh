#!/usr/bin/env bash
# M7 (DESIGN.md §15.3, §10.6, D016): the same name made on both devices, and
# an app's edit. Three parts, in this order.
#   file:   B has a new c.txt open for writing while A creates and uploads a
#           c.txt. The server's c.txt stays; B's file becomes a conflict copy
#           (B lists `create_create`); both devices end with both contents.
#           Real account too.
#   folder: (fake) A, offline, and B both make a folder dd with different
#           children. The folders never conflict (D016): one dd with both
#           children.
#   app:    (fake) an official app (no mode, no SHA-1) writes a new revision
#           of a clean file. Both devices adopt the content; no conflict.
source "$(dirname "$0")/lib.sh"
prepare_two
make_workdir
scenario_area m7
b=$ROOT_B/m7
mkdir "$area/file" "$area/folder" "$area/app"
printf v1 >"$area/app/app.txt"
wait_queue_empty
wait_content "$b/app/app.txt" v1

step "M7 file: B holds a new c.txt open; A creates c.txt"
hold_open b "$b/file/c.txt" b-content
printf a-content >"$area/file/c.txt"
reach_b_while_held "$area/file" "$b/file" m7
for dir in "$area/file" "$b/file"; do
    expect_eq "c.txt in $dir" "$(cat "$dir/c.txt")" a-content
    copies=$(conflict_copies "$dir")
    [[ -n "$copies" && $(wc -l <<<"$copies") == 1 ]] || fail "expected one conflict copy in $dir, found: $copies"
    expect_eq "the conflict copy in $dir" "$(cat "$dir/$copies")" b-content
    expect_eq "entries of $dir" "$(ls "$dir" | wc -l)" 2
done
expect_conflicts 2 "$b" create_create
expect_conflicts 1 "$area"

if [[ $BACKEND == fake ]]; then
    step "M7 folder: both devices make dd"
    a_offline
    mkdir "$area/folder/dd"
    printf a >"$area/folder/dd/a"
    mkdir "$b/folder/dd"
    printf b >"$b/folder/dd/b"
    a_back
    for dir in "$area/folder" "$b/folder"; do
        expect_eq "entries of $dir" "$(ls "$dir")" dd
        expect_eq "entries of $dir/dd" "$(ls "$dir/dd" | paste -sd' ')" "a b"
        expect_eq "dd/a in $dir" "$(cat "$dir/dd/a")" a
        expect_eq "dd/b in $dir" "$(cat "$dir/dd/b")" b
    done
    expect_conflicts 2 "$b" create_create
    expect_conflicts 1 "$area"

    step "M7 app: an official app writes a new revision"
    printf v2 >"$work/app.txt"
    "$PDMOUNT" fake write --app --path /pdmount-test/m7/app/app.txt --from "$work/app.txt" "$FAKE_DIR"
    wait_content "$area/app/app.txt" v2
    wait_content "$b/app/app.txt" v2
    sync_devices
    expect_conflicts 2 "$b" create_create
    expect_conflicts 1 "$area"
else
    echo "M7 folder and app parts need the fake; skipped for BACKEND=$BACKEND"
fi
echo "M7: PASS"
