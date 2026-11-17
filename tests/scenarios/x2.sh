#!/usr/bin/env bash
# X2 (DESIGN.md §15.3, D075): a queue larger than the grace can upload. The
# daemon exits within the grace after SIGTERM; the next mount uploads the
# rest; nothing is lost. A second SIGTERM during the flush stops the daemon
# at once, with a store commit (D092). Fake with the network profile.
source "$(dirname "$0")/lib.sh"
[[ $BACKEND == fake ]] || { echo "x2 needs the fake's network profile; skipped for BACKEND=$BACKEND"; exit 0; }
x_live_only
ensure_dataset
prepare_mount
make_workdir
scenario_area x2
x_config_save
trap 'x_restore; scenario_cleanup' EXIT
grace=8

step "a grace of $grace s, 1 MB/s up"
x_config '[upload]' 'debounce = "60s"' '[timeouts]' "shutdown_grace = \"${grace}s\""
pd unmount "$MNT"
mount_now
"$PDMOUNT" fake profile --rtt 80 --up 1000000 "$FAKE_DIR"

step "24 MiB queued, SIGTERM"
log_mark
write_files "$area" x2 24 1024
term_daemon
((stop_ms <= (grace + 3) * 1000)) || fail "the stop took $stop_ms ms; the grace is $grace s"
((stop_ms >= (grace - 1) * 1000)) || fail "the stop took only $stop_ms ms: the flush did not wait"
expect_log 'the grace ran out: flushed [0-9]+ of [0-9]+ entr(y|ies)' "the flush"
expect_log '[0-9]+ entr(y|ies) left for the next mount' "the stop"

step "the next mount uploads the rest"
"$PDMOUNT" fake profile clear "$FAKE_DIR"
x_restore
mount_now
wait_queue_empty
pd unmount "$MNT"
expect_on_drive "$area" x2

# D092: a second signal during the flush (OpenRC's shutdown sends one about a
# second before KILL) ends it, and the stop waits for neither the grace nor
# running uploads: the store is committed and the daemon exits at once.
step "a grace of 60 s, 1 MB/s up; SIGTERM, and a second one 2 s later"
x_config '[upload]' 'debounce = "60s"' '[timeouts]' 'shutdown_grace = "60s"'
pd unmount "$MNT"
mount_now
scenario_area x2-second
"$PDMOUNT" fake profile --rtt 80 --up 1000000 "$FAKE_DIR"
log_mark
# 4 MiB files: uploads still run long after the second signal.
write_files "$area" x2b 6 4096
pid=$(daemon_pid)
[[ -n "$pid" ]] || fail "no daemon pid in pdmount status"
kill -TERM "$pid"
sleep 2
kill -0 "$pid" 2>/dev/null || fail "the daemon exited before the second SIGTERM"
start=$(now_ms)
kill -TERM "$pid"
while kill -0 "$pid" 2>/dev/null; do sleep 0.02; done
stop_ms=$(($(now_ms) - start))
echo "the daemon stopped $stop_ms ms after the second SIGTERM"
((stop_ms <= 2000)) || fail "the stop took $stop_ms ms after the second SIGTERM"
expect_log 'second stop signal: ending the flush' "the second signal"
expect_log 'pdmount::daemon(::[a-z_]+)?: stopped$' "the store commit"
expect_log '[0-9]+ entr(y|ies) left for the next mount' "the stop"

step "the next mount uploads the rest"
"$PDMOUNT" fake profile clear "$FAKE_DIR"
x_restore
mount_now
wait_queue_empty
pd unmount "$MNT"
expect_on_drive "$area" x2b
check_hot_path
echo "x2: PASS"
