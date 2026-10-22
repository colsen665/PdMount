#!/usr/bin/env bash
# E1 (DESIGN.md §15.3, §9): while the mount is up, another client renames a
# directory, trashes a file and writes a new revision of a file that was
# read before. The mount shows all three without a remount, through the
# event stream (polled every 30 s, D033): the old names are gone from the
# kernel's caches and the file reads its new content. Second client: the
# fake's own CLI (BACKEND=fake only).
source "$(dirname "$0")/lib.sh"
[[ $BACKEND == fake ]] || { echo "E1 needs a second client; skipped for BACKEND=$BACKEND"; exit 0; }
prepare_mount
make_workdir
scenario_area e1
mkdir -p "$area/dir"
printf old >"$area/dir/f"
printf gone >"$area/g"
printf before >"$area/c"
wait_queue_empty
# Into the kernel's caches: dentries, attributes, pages.
ls -l "$area" "$area/dir" >/dev/null
[[ "$(cat "$area/c")" == before ]] || fail "c does not read back"

step "E1 [$VERIFY_MODE]: remote changes while mounted"
printf after >"$work/c"
"$PDMOUNT" fake rename --from /pdmount-test/e1/dir --to /pdmount-test/e1/moved "$FAKE_DIR"
"$PDMOUNT" fake trash --path /pdmount-test/e1/g "$FAKE_DIR"
"$PDMOUNT" fake write --path /pdmount-test/e1/c --from "$work/c" "$FAKE_DIR"
start=$SECONDS
# Two poll intervals and the pacing of the listings (D027).
until [[ -d "$area/moved" && ! -e "$area/dir" && ! -e "$area/g" && "$(cat "$area/c")" == after ]]; do
    ((SECONDS - start < 75)) || fail "remote changes not visible after 75 s: $(ls "$area")"
    sleep 1
done
echo "visible after $((SECONDS - start)) s"
[[ "$(cat "$area/moved/f")" == old ]] || fail "the moved directory lost its content"

verify_cycle
area=$TEST_ROOT/e1
[[ "$(ls "$area")" == "$(printf 'c\nmoved')" ]] || fail "unexpected entries after $VERIFY_MODE: $(ls "$area")"
[[ "$(cat "$area/c")$(cat "$area/moved/f")" == afterold ]] || fail "content changed after $VERIFY_MODE"
check_hot_path
echo "E1 [$VERIFY_MODE]: PASS"
