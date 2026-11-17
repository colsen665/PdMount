#!/usr/bin/env bash
# X4 (DESIGN.md §15.3, M5.2, D075, D095): the mount after a stop that left
# changes warns once, in the log and on the desktop, and says when they are
# uploaded; after a stop that left none it says nothing; and no "uploaded"
# follows when the first notification did not go out. The stops: SIGTERM
# with a grace too short for the queue (as X2), and kill -9. Fake with the
# network profile and the notify-send stub (stubs/notify-send).
source "$(dirname "$0")/lib.sh"
[[ $BACKEND == fake ]] || { echo "x4 needs the fake's network profile; skipped for BACKEND=$BACKEND"; exit 0; }
x_live_only
ensure_dataset
prepare_mount
make_workdir
scenario_area x4
x_config_save
trap 'x_restore; scenario_cleanup' EXIT
grace=4

# The stub lives in the repository, since SCENARIO_ROOT may be noexec; a
# session bus address only has to be set. The daemon inherits both.
touch "$work/notify.log"
export PATH=$repo/tests/scenarios/stubs:$PATH X11_NOTIFY_LOG=$work/notify.log \
    X11_NOTIFY_FAIL=$work/notify.fail DBUS_SESSION_BUS_ADDRESS=unix:path=$work/no-session-bus
notifications() { wc -l <"$work/notify.log"; }
# Waits until the stub has logged <n> notifications.
wait_notifications() { # <n> <seconds>
    local deadline=$((SECONDS + $2))
    until (($(notifications) >= $1)); do
        ((SECONDS < deadline)) || fail "expected $1 notifications within $2 s, got: $(cat "$work/notify.log")"
        sleep 0.2
    done
}
wait_log() { # <extended regex> <seconds> <what>
    local deadline=$((SECONDS + $2))
    until log_since_mark | grep -Eq "$1"; do
        ((SECONDS < deadline)) || fail "$3: no log line matching '$1' within $2 s"
        sleep 0.2
    done
}
expect_no_log() { # <extended regex> <what>
    if log_since_mark | grep -Eq "$1"; then fail "$2: a log line matches '$1'"; fi
}

step "a grace of $grace s, 1 MB/s up; 24 MiB queued, SIGTERM"
x_config '[upload]' 'debounce = "60s"' '[timeouts]' "shutdown_grace = \"${grace}s\""
pd unmount "$MNT"
mount_now
"$PDMOUNT" fake profile --rtt 80 --up 1000000 "$FAKE_DIR"
mkdir "$area/a"
wait_queue_empty
write_files "$area/a" a 24 1024
term_daemon
# The profile stays, so the changes are still uploading at the next mount.
x_config '[timeouts]' "shutdown_grace = \"${grace}s\""

step "the next mount warns once and says when the changes are uploaded"
log_mark
mount_now
expect_log '[0-9]+ changes? from the last session (was|were) not uploaded' "the start"
wait_notifications 1 5
grep -Eq 'PdMount: [0-9]+ changes from the last session are still uploading' "$work/notify.log" ||
    fail "first notification: $(cat "$work/notify.log")"
expect_eq "notifications while uploading" "$(notifications)" 1
"$PDMOUNT" fake profile clear "$FAKE_DIR"
wait_queue_empty
wait_notifications 2 15
grep -Eq 'PdMount: the [0-9]+ changes from the last session are uploaded' "$work/notify.log" ||
    fail "second notification: $(cat "$work/notify.log")"
wait_log 'the changes of the last session are done' 10 "the end"
sleep 1
expect_eq "notifications" "$(notifications)" 2

step "an empty queue at the next mount: no warning, no notification"
pd unmount --wait "$MNT"
log_mark
mount_now
sleep 3
expect_no_log 'from the last session (was|were) not uploaded' "the start"
expect_eq "notifications" "$(notifications)" 2

step "kill -9 with a queue: the next mount warns the same way"
x_config '[upload]' 'debounce = "60s"' '[timeouts]' "shutdown_grace = \"${grace}s\""
pd unmount "$MNT"
mount_now
"$PDMOUNT" fake profile --rtt 80 --up 1000000 "$FAKE_DIR"
mkdir "$area/b"
wait_queue_empty
write_files "$area/b" b 10 1024
kill_daemon
pd unmount --force "$MNT"
x_config '[timeouts]' "shutdown_grace = \"${grace}s\""
log_mark
mount_now
expect_log '[0-9]+ changes? from the last session (was|were) not uploaded' "the start"
wait_notifications 3 5
grep -Eq 'still uploading' <(tail -1 "$work/notify.log") || fail "notification: $(tail -1 "$work/notify.log")"
"$PDMOUNT" fake profile clear "$FAKE_DIR"
wait_queue_empty
wait_notifications 4 15
grep -Eq 'are uploaded' <(tail -1 "$work/notify.log") || fail "notification: $(tail -1 "$work/notify.log")"

step "the first notification did not go out: no second one"
x_config '[upload]' 'debounce = "60s"' '[timeouts]' "shutdown_grace = \"${grace}s\""
pd unmount "$MNT"
mount_now
"$PDMOUNT" fake profile --rtt 80 --up 1000000 "$FAKE_DIR"
mkdir "$area/c"
wait_queue_empty
write_files "$area/c" c 6 1024
kill_daemon
pd unmount --force "$MNT"
x_config '[timeouts]' "shutdown_grace = \"${grace}s\""
touch "$work/notify.fail"
log_mark
mount_now
expect_log '[0-9]+ changes? from the last session (was|were) not uploaded' "the start"
"$PDMOUNT" fake profile clear "$FAKE_DIR"
wait_queue_empty
rm -f "$work/notify.fail"
wait_log 'the changes of the last session are done' 10 "the end"
sleep 3
expect_eq "notifications" "$(notifications)" 4

step "everything is on the drive"
x_restore
pd unmount "$MNT"
mkdir -p "$work/all"
for part in a b c; do cp -r "$work/$part" "$work/all/$part"; done
expect_on_drive "$area" all
check_hot_path
echo "x4: PASS"
