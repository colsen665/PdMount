#!/usr/bin/env bash
# M4 (DESIGN.md §15.3, §10.5): local delete against a remote edit, on the
# fake only (A has to be offline). A deletes a file while it cannot reach the
# drive, B writes the file and uploads. When A is back, the trash pre-check
# sees another revision: the file is not trashed and B's content returns on A
# (`local_delete_remote_edit`).
source "$(dirname "$0")/lib.sh"
[[ $BACKEND == fake ]] || { echo "M4 needs an offline device; skipped for BACKEND=$BACKEND"; exit 0; }
prepare_two
make_workdir
scenario_area m4
b=$ROOT_B/m4
printf base >"$area/f"
wait_queue_empty
wait_content "$b/f" base

step "M4: A offline deletes f; B writes f"
a_offline
rm "$area/f"
printf b-content >"$b/f"
a_back

for dir in "$area" "$b"; do
    expect_eq "entries of $dir" "$(ls "$dir")" f
    expect_eq "f in $dir" "$(cat "$dir/f")" b-content
done
expect_conflicts 1 "$area" local_delete_remote_edit
link=$(fake_changes "$fake_mark" | awk '$2 == "created-file" && $4 == "f" { print $3; exit }')
[[ -n "$link" ]] || fail "f was never created on the drive"
trashed=$(fake_changes "$fake_mark" | awk -v link="$link" '$2 == "trashed" && $3 == link')
[[ -z "$trashed" ]] || fail "f went to the trash: $trashed"
echo "M4: PASS"
