#!/usr/bin/env bash
# E2 (DESIGN.md §15.3, §9.7, §12.3): offline and `needs_login`, on the fake.
#   1. The drive is unreachable for A for 90 s while rsync writes into the
#      mount. Reads of cached data work, a read of a file that was never
#      fetched fails within 3 s (`timeouts.offline_read`), `status` says
#      offline. When the drive is back, the queue drains.
#   2. The session is rejected: `status` shows `needs login`, local writes
#      are accepted. When the fault is cleared (the fake daemon's stand-in for
#      signing in again), the queue drains.
# Everything must be on the drive afterwards and nothing parked.
source "$(dirname "$0")/lib.sh"
[[ $BACKEND == fake ]] || { echo "E2 needs fault injection; skipped for BACKEND=$BACKEND"; exit 0; }
[[ $VERIFY_MODE == live ]] || { echo "E2 runs in live mode only; skipped in $VERIFY_MODE mode"; exit 0; }
prepare_mount
make_workdir
scenario_area e2
need_uid_a

# A file on the drive that A has listed but never read, too big to be prefetched.
head -c $((6 << 20)) /dev/urandom >"$work/remote-big"
"$PDMOUNT" fake create --path /pdmount-test/e2/remote-big --from "$work/remote-big" "$FAKE_DIR"
wait_visible "$area/remote-big"
printf cached >"$area/cached"
wait_queue_empty
[[ "$(cat "$area/cached")" == cached ]] || fail "cached does not read back"

step "E2: the drive is unreachable for 90 s"
fake_fault offline --client "$uid_a" --for 90
start=$SECONDS
make_tree "$work/src" 60 1
rsync -a "$work/src/" "$area/dst/"
until status_text | grep -q '^Connection: offline'; do
    ((SECONDS - start < 60)) || fail "status never showed offline: $(status_text)"
    sleep 1
done
echo "offline after $((SECONDS - start)) s"
[[ "$(cat "$area/cached")" == cached ]] || fail "a cached file does not read while offline"
[[ -n "$(find "$area/dst" -type f -print -quit)" && "$(cat "$area/dst/$(cd "$work/src" && find . -type f -size +0 | head -1)" | wc -c)" -gt 0 ]] ||
    fail "files written offline do not read back"
read_start=$(date +%s.%N)
if timeout 10 head -c 1 "$area/remote-big" >/dev/null 2>&1; then fail "an uncached file read while offline"; fi
read_time=$(echo "$(date +%s.%N) - $read_start" | bc)
echo "the uncached read failed after $read_time s"
(($(echo "$read_time < 3.5" | bc))) || fail "the uncached read took $read_time s, expected under 3 s"
until ((SECONDS - start >= 90)); do sleep 1; done
echo "the drive is back; waiting for the queue"
wait_queue_empty
until status_text | grep -q '^Connection: online'; do
    ((SECONDS - start < 300)) || fail "status never showed online: $(status_text)"
    sleep 1
done

step "E2: the session is rejected"
fake_fault unauthorized --client "$uid_a"
printf rejected >"$area/rejected"
start=$SECONDS
until status_text | grep -q '^Connection: needs login'; do
    ((SECONDS - start < 60)) || fail "status never showed needs login: $(status_text)"
    sleep 1
done
echo "needs login after $((SECONDS - start)) s"
printf written >"$area/while-rejected"
mkdir "$area/dir-while-rejected"
[[ "$(cat "$area/while-rejected")" == written && -d "$area/dir-while-rejected" ]] ||
    fail "local writes are not accepted in needs_login"
fake_fault clear
wait_queue_empty
until status_text | grep -q '^Connection: online'; do
    ((SECONDS - start < 120)) || fail "status never showed online after the fault: $(status_text)"
    sleep 1
done

step "E2: everything is on the drive"
status_text | grep -q '^Queue: *0 pending, 0 parked' || fail "queue not empty: $(status_text | grep '^Queue')"
VERIFY_MODE=fresh verify_cycle
VERIFY_MODE=fresh
assert_same_tree "$work/src" "$TEST_ROOT/e2/dst"
expect_eq "rejected" "$(cat "$TEST_ROOT/e2/rejected")" rejected
expect_eq "while-rejected" "$(cat "$TEST_ROOT/e2/while-rejected")" written
[[ -d "$TEST_ROOT/e2/dir-while-rejected" ]] || fail "the folder made while rejected is gone"
[[ "$(wc -c <"$TEST_ROOT/e2/remote-big")" == $((6 << 20)) ]] || fail "remote-big changed"
check_hot_path
echo "E2: PASS"
