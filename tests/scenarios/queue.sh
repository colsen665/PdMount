#!/usr/bin/env bash
# Queue tooling and `sync` through the CLI on a mount (DESIGN.md §8.2, §5.6,
# D061): list, park, sync, retry, drop.
source "$(dirname "$0")/lib.sh"
# `queue drop --all` and a mount-wide `sync` reach beyond pdmount-test (D028).
[[ $BACKEND == fake ]] || { echo "queue drops every pending change of the mount; skipped for BACKEND=$BACKEND"; exit 0; }
ensure_dataset
VERIFY_MODE=live prepare_mount
make_workdir
scenario_area queue

step "sync of a folder: a new file is uploaded"
printf v1 >"$area/f"
pd sync "$area"
expect_eq "queue under the folder" "$(pd queue list "$area")" "Nothing is queued."

step "park holds a change, sync says so, retry lets it go"
printf v2 >"$area/f"
pd queue park "$area/f"
pd queue list "$area" | grep -q 'parked by user' || fail "queue list does not show the park"
pd queue show "$area/f" | grep -q '^base rev' || fail "queue show has no base revision"
if out=$(pd sync "$area"); then fail "sync with a parked entry exits 0"; fi
grep -q 'parked by user' <<<"$out" || fail "sync does not name the parked entry: $out"
pd queue retry "$area/f"
pd sync "$area"
pd unmount "$MNT"
mount_now
expect_eq "f after a remount" "$(cat "$area/f")" v2

step "drop of a changed file: the remote content is back"
printf changed >"$area/f"
expect_eq "f before the drop" "$(cat "$area/f")" changed
if out=$(pd queue drop "$area/f"); then fail "drop without --yes exits 0"; fi
grep -q 'discard the local change' <<<"$out" || fail "drop does not say what it discards: $out"
expect_eq "f after the preview" "$(cat "$area/f")" changed
pd queue drop --yes "$area/f"
expect_eq "f after the drop" "$(cat "$area/f")" v2
expect_eq "queue under the folder" "$(pd queue list "$area")" "Nothing is queued."

step "drop of never-uploaded files: they are gone"
mkdir "$area/d"
printf x >"$area/d/g"
printf y >"$area/h"
pd queue drop --yes "$area/d"
[[ ! -e "$area/d/g" ]] || fail "the dropped file is still there"
[[ -e "$area/h" ]] || fail "a file outside the dropped folder went too"
pd queue drop --yes --all
[[ ! -e "$area/h" ]] || fail "the dropped file is still there"
expect_eq "queue" "$(pd queue list)" "Nothing is queued."
pd sync
check_hot_path
echo "queue: PASS"
