#!/usr/bin/env bash
# X11 (DESIGN.md §15.3, D091): logind's delay lock and PrepareForShutdown,
# against a stub logind (`pdmount fake logind`) on a private bus exported as
# DBUS_SYSTEM_BUS_ADDRESS, and a notify-send stub on PATH. Fake with the
# network profile. Skipped when dbus-daemon is not installed.
source "$(dirname "$0")/lib.sh"
[[ $BACKEND == fake ]] || { echo "x11 needs the fake and a stub logind; skipped for BACKEND=$BACKEND"; exit 0; }
command -v dbus-daemon >/dev/null || { echo "x11: no dbus-daemon; skipped"; exit 0; }
x_live_only
ensure_dataset
prepare_mount
make_workdir
scenario_area x11
x_config_save
grace=12

# The private bus: anyone may own and call anything, as on a session bus.
bus=$work/bus
cat >"$work/bus.conf" <<EOF
<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:path=$bus</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
EOF
dbus-daemon --config-file="$work/bus.conf" --nofork --nopidfile &
bus_pid=$!
until [[ -S $bus ]]; do
    kill -0 "$bus_pid" 2>/dev/null || fail "dbus-daemon did not start"
    sleep 0.1
done
stub_pid=
stop_stub() {
    if [[ -n $stub_pid ]]; then
        kill "$stub_pid" 2>/dev/null || true
        wait "$stub_pid" 2>/dev/null || true
        stub_pid=
    fi
}
saved_path=$PATH saved_session_bus=${DBUS_SESSION_BUS_ADDRESS-} saved_system_bus=$DBUS_SYSTEM_BUS_ADDRESS
x11_cleanup() {
    stop_stub
    kill "$bus_pid" 2>/dev/null || true
    x_restore
    # The next scenario's daemon must not inherit the stubs.
    if is_mounted; then
        PATH=$saved_path DBUS_SESSION_BUS_ADDRESS=$saved_session_bus DBUS_SYSTEM_BUS_ADDRESS=$saved_system_bus
        export PATH DBUS_SESSION_BUS_ADDRESS DBUS_SYSTEM_BUS_ADDRESS
        pd unmount "$MNT" >/dev/null 2>&1 || true
    fi
    scenario_cleanup
}
trap x11_cleanup EXIT
export DBUS_SYSTEM_BUS_ADDRESS=unix:path=$bus

# notify-send records its arguments (the stub lives in the repository, since
# SCENARIO_ROOT may be noexec); a session bus address only has to be set.
touch "$work/notify.log"
export PATH=$repo/tests/scenarios/stubs:$PATH X11_NOTIFY_LOG=$work/notify.log \
    DBUS_SESSION_BUS_ADDRESS=unix:path=$work/no-session-bus

stub_log=$work/logind.log
start_stub() { # [--refuse]
    : >"$stub_log"
    # zbus warns that the stub serves no object server; its output is noise here.
    "$PDMOUNT" fake logind serve --log "$stub_log" "$@" 2>>"$work/stub.err" &
    stub_pid=$!
    wait_stub ready 10
}
# Waits until the stub's log has a line matching <regex>; sets stub_ms to its time.
wait_stub() { # <regex> <seconds>
    local deadline=$((SECONDS + $2)) line
    while :; do
        line=$(grep -E "^[0-9]+ $1\$" "$stub_log" | tail -1 || true)
        if [[ -n $line ]]; then
            stub_ms=${line%% *}
            return 0
        fi
        ((SECONDS < deadline)) || fail "the stub logind did not log '$1' within $2 s: $(cat "$stub_log")"
        sleep 0.1
    done
}
prepare() { # true|false
    "$PDMOUNT" fake logind prepare "$1"
    wait_stub "prepare $1" 5
    prepare_ms=$stub_ms
}
# The shutdown ones; a mount after a stop with entries left sends its own (M5.2).
notifications() { grep -c 'before shutdown' "$work/notify.log" || true; }

step "the lock is taken at mount"
start_stub
x_config '[upload]' 'debounce = "60s"' '[timeouts]' "shutdown_grace = \"${grace}s\""
pd unmount "$MNT"
mount_now
wait_stub 'inhibit 1 shutdown delay' 10
"$PDMOUNT" fake profile --rtt 80 --up 2000000 "$FAKE_DIR"

step "PrepareForShutdown with a queue: one notification, a flush, the lock released at its end"
# The folder is uploaded first, so the queue holds the 8 files alone.
mkdir "$area/a"
wait_queue_empty
log_mark
write_files "$area/a" a 8 1024
prepare true
wait_stub 'released 1' "$grace"
((stub_ms - prepare_ms >= 2000)) || fail "the lock was released $((stub_ms - prepare_ms)) ms after PrepareForShutdown; 8 MiB at 2 MB/s take 4 s"
expect_log 'flushed 8 entries in' "the flush"
expect_eq "notifications" "$(notifications)" 1
grep -q 'uploading 8 changes before shutdown' "$work/notify.log" || fail "notification: $(cat "$work/notify.log")"
expect_eq "queue after the flush" "$(pd queue list "$area")" "Nothing is queued."
is_mounted || fail "the daemon stopped after PrepareForShutdown"

step "cancelled: the lock is taken again"
prepare false
wait_stub 'inhibit 2 shutdown delay' 10

step "PrepareForShutdown with an empty queue: released at once, no notification"
wait_queue_empty
prepare true
wait_stub 'released 2' 5
((stub_ms - prepare_ms < 2000)) || fail "the lock was released only $((stub_ms - prepare_ms)) ms after PrepareForShutdown"
expect_eq "notifications" "$(notifications)" 1
prepare false
wait_stub 'inhibit 3 shutdown delay' 10

step "SIGTERM during the flush continues it: the grace is not restarted"
mkdir "$area/b"
wait_queue_empty
log_mark
write_files "$area/b" b 40 1024
prepare true
sleep 6
term_daemon
# The daemon has stopped; lock 3 went with the flush's end at the latest.
wait_stub 'released 3' 3
since_prepare=$(($(now_ms) - prepare_ms))
((since_prepare <= (grace + 3) * 1000)) ||
    fail "the daemon stopped $since_prepare ms after PrepareForShutdown; the grace of $grace s was restarted"
expect_log 'continuing the flush that PrepareForShutdown started' "SIGTERM"
expect_log 'the grace ran out' "the flush"
expect_eq "notifications" "$(notifications)" 2

step "no logind: SIGTERM alone flushes"
"$PDMOUNT" fake profile clear "$FAKE_DIR"
stop_stub
log_mark
mount_now
wait_queue_empty
"$PDMOUNT" fake profile --rtt 80 --up 20000000 "$FAKE_DIR"
write_files "$area/c" c 5 64
term_daemon
expect_log 'no logind on the system bus' "the start"
expect_log 'flushed [0-9]+ entr(y|ies) in' "the flush"

step "the lock refused: SIGTERM alone flushes"
start_stub --refuse
log_mark
mount_now
wait_stub refused 10
write_files "$area/d" d 5 64
term_daemon
expect_log 'logind refused a shutdown delay lock' "the start"
expect_log 'flushed [0-9]+ entr(y|ies) in' "the flush"
expect_eq "notifications" "$(notifications)" 2

step "everything is on the drive"
stop_stub
"$PDMOUNT" fake profile clear "$FAKE_DIR"
x_restore
mkdir -p "$work/all"
for part in a b c d; do cp -r "$work/$part" "$work/all/$part"; done
expect_on_drive "$area" all
check_hot_path
echo "x11: PASS"
