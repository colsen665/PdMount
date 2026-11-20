#!/usr/bin/env bash
# E1 (DESIGN.md §15.3, §9): while the mount is up, another client renames a
# directory, trashes a file and writes a new revision of a file that was
# read before. The mount shows all three without a remount, through the
# event stream (polled every 30 s, D033): the old names are gone from the
# kernel's caches and the file reads its new content. Second client: the
# fake's own CLI (BACKEND=fake only). In live mode, a page that keeps failing
# with an error that is not transient (node fetches broken, as for a node
# under a folder that cannot be decrypted, M6.0.3 V-3) is given up after
# three polls and the tree lists again: the change still shows.
source "$(dirname "$0")/lib.sh"
[[ $BACKEND == fake ]] || { echo "E1 needs a second client; skipped for BACKEND=$BACKEND"; exit 0; }
[[ $VERIFY_MODE != second ]] || { echo "E1 has one device; skipped in second mode"; exit 0; }
prepare_mount
make_workdir
scenario_area e1
mkdir -p "$area/dir"
printf old >"$area/dir/f"
printf gone >"$area/g"
printf before >"$area/c"
wait_queue_empty
event_barrier
# Into the kernel's caches: dentries, attributes, pages.
ls -l "$area" "$area/dir" >/dev/null
[[ "$(cat "$area/c")" == before ]] || fail "c does not read back"

step "E1 [$VERIFY_MODE]: remote changes while mounted"
printf after >"$work/c"
"$PDMOUNT" fake rename --from /pdmount-test/e1/dir --to /pdmount-test/e1/moved "$FAKE_DIR"
"$PDMOUNT" fake trash --path /pdmount-test/e1/g "$FAKE_DIR"
"$PDMOUNT" fake write --path /pdmount-test/e1/c --from "$work/c" "$FAKE_DIR"
start=$SECONDS
# poll_interval + 10 s (D037).
until [[ -d "$area/moved" && ! -e "$area/dir" && ! -e "$area/g" && "$(cat "$area/c")" == after ]]; do
    ((SECONDS - start < 40)) || fail "remote changes not visible after 40 s: $(ls "$area")"
    sleep 1
done
echo "visible after $((SECONDS - start)) s"
[[ "$(cat "$area/moved/f")" == old ]] || fail "the moved directory lost its content"
c_final=after

if [[ $VERIFY_MODE == live ]]; then
    step "E1 [live]: a page that keeps failing is given up"
    need_uid_a
    log_mark
    fake_fault broken-nodes --client "$uid_a"
    printf again >"$work/c"
    "$PDMOUNT" fake write --path /pdmount-test/e1/c --from "$work/c" "$FAKE_DIR"
    start=$SECONDS
    # Three failed polls, 30 s apart (§9.2: back-off 1, 1), then a refresh.
    until [[ "$(cat "$area/c")" == again ]]; do
        ((SECONDS - start < 150)) || fail "the change did not show after 150 s with node fetches broken"
        sleep 2
    done
    echo "visible after $((SECONDS - start)) s"
    log_since_mark | grep -q "an event page keeps failing" || fail "no warning about the page that was given up"
    fake_fault clear
    c_final=again
fi

verify_cycle
area=$TEST_ROOT/e1
[[ "$(ls "$area")" == "$(printf 'c\nmoved')" ]] || fail "unexpected entries after $VERIFY_MODE: $(ls "$area")"
[[ "$(cat "$area/c")$(cat "$area/moved/f")" == "${c_final}old" ]] || fail "content changed after $VERIFY_MODE"
check_hot_path
echo "E1 [$VERIFY_MODE]: PASS"
