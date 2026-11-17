#!/usr/bin/env bash
# X3 (DESIGN.md §15.3, D075): no flush wait with an empty queue, nor while
# offline; what was queued offline uploads at the next mount. Fake.
source "$(dirname "$0")/lib.sh"
[[ $BACKEND == fake ]] || { echo "x3 needs the fake's fault rules; skipped for BACKEND=$BACKEND"; exit 0; }
x_live_only
ensure_dataset
prepare_mount
make_workdir
scenario_area x3
x_config_save
trap 'x_restore; scenario_cleanup' EXIT

step "an empty queue: SIGTERM stops at once"
wait_queue_empty
log_mark
term_daemon
((stop_ms < 3000)) || fail "the stop took $stop_ms ms with an empty queue"
expect_log 'nothing to flush' "the flush"
mount_now

step "offline with a queue: SIGTERM stops at once"
a_offline
log_mark
write_files "$area" x3 4 64
deadline=$((SECONDS + 60))
until status_text | grep -q '^Connection: *offline'; do
    ((SECONDS < deadline)) || fail "the daemon did not go offline: $(status_text | grep '^Connection:')"
    sleep 0.5
done
term_daemon
((stop_ms < 3000)) || fail "the stop took $stop_ms ms while offline"
expect_log 'no flush: the drive cannot be reached' "the flush"
expect_log '[0-9]+ entr(y|ies) left for the next mount' "the stop"

step "online again: the next mount uploads them"
fake_fault clear
mount_now
wait_queue_empty
pd unmount "$MNT"
expect_on_drive "$area" x3
check_hot_path
echo "x3: PASS"
