#!/usr/bin/env bash
# A5 (DESIGN.md §15.3): rename a `ro` directory from a second client and
# delete another directory with a rule: the rule follows the rename (list
# shows the new path, active) and the deleted one shows orphaned. The changes
# are made while unmounted, so they arrive from the stored event cursor at
# the next mount (§9.1; E1 covers changes while mounted). Second client: the
# fake's own CLI (BACKEND=fake only).
source "$(dirname "$0")/lib.sh"
source "$(dirname "$0")/access-lib.sh"
[[ $BACKEND == fake ]] || { echo "A5 needs a second client; skipped for BACKEND=$BACKEND"; exit 0; }
[[ $VERIFY_MODE != fresh ]] || { echo "A5: rules are local state; nothing to check in fresh mode"; exit 0; }
prepare_mount
scenario_area a5
mkdir -p "$area/keep" "$area/gone"
pd access set ro "$area/keep"
pd access set ro "$area/gone"
wait_queue_empty

step "A5 [$VERIFY_MODE]: remote rename and delete of ruled directories"
verify_cycle
pd unmount "$MNT"
"$PDMOUNT" fake rename --from /pdmount-test/a5/keep --to /pdmount-test/a5/kept "$FAKE_DIR"
"$PDMOUNT" fake trash --path /pdmount-test/a5/gone "$FAKE_DIR"
mount_now
# The event task applies the remote changes in the background.
for _ in $(seq 1 60); do
    [[ -d "$TEST_ROOT/a5/kept" && ! -d "$TEST_ROOT/a5/gone" ]] && break
    sleep 1
done
list=$(pd access list)
echo "$list"
grep -qE '^ro +/pdmount-test/a5/kept +/pdmount-test/a5/keep +cli +active$' <<<"$list" || fail "the rule did not follow the rename"
grep -qE '^ro +- +/pdmount-test/a5/gone +cli +orphaned$' <<<"$list" || fail "the deleted directory's rule is not orphaned"
expect_erofs "write in the renamed ro directory" touch "$TEST_ROOT/a5/kept/x"
pd access unset "$TEST_ROOT/a5/kept"
pd access unset pdmount-test/a5/gone
check_hot_path
echo "A5 [$VERIFY_MODE]: PASS"
