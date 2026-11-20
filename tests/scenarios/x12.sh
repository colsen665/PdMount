#!/usr/bin/env bash
# X12 (DESIGN.md §15.3, D076): the pending marker, two daemons on one fake
# drive. Device A, with `sync.pending_marker` on, runs out of grace with
# changes left and leaves .pdmount/<device id>.pending in the root. Device B
# shows `Incoming:` in status and gets one notification (a stub notification
# server on a private session bus);
# .pdmount is read-only in its mount. A's next mount drains and trashes the
# marker, and B's `Incoming:` goes. A flush that finishes after leaving the
# marker trashes it in the same stop (D096). Fake with the network profile.
source "$(dirname "$0")/lib.sh"
[[ $BACKEND == fake ]] || { echo "x12 writes outside pdmount-test/ (D028); fake only, skipped for BACKEND=$BACKEND"; exit 0; }
need_dbus_daemon x12
x_live_only
ensure_dataset
ensure_fake_drive
make_workdir
x_config_save
grace=16
# B's poll interval (30 s) and a pass, with room for the network profile.
limit=90

x12_cleanup() {
    x_restore
    # B must not keep the stub's bus for the scenarios after this one.
    if is_mounted_second; then pd2 unmount "$MNT2" >/dev/null 2>&1 || true; fi
    scenario_cleanup
}
trap x12_cleanup EXIT

# Waits until <command> succeeds, at most <seconds>; sets waited to the time.
wait_until() { # <seconds> <what> <command...>
    local limit=$1 what=$2 start=$SECONDS
    shift 2
    until "$@"; do
        ((SECONDS - start < limit)) || fail "$what: not within $limit s"
        sleep 1
    done
    waited=$((SECONDS - start))
    echo "$what after $waited s"
}
incoming_b() { pd2 status 2>/dev/null | grep -E '^Incoming: +dev-a: [0-9]+ changes? \(' ; }
no_incoming_b() { ! pd2 status 2>/dev/null | grep -q '^Incoming:'; }

step "B mounted with a notification stub on its own session bus"
if is_mounted_second; then pd2 unmount "$MNT2"; fi
notify_log=$work/notify.log
: >"$notify_log"
start_session_bus x12
start_notify_stub "$bus_address" "$notify_log"
DBUS_SESSION_BUS_ADDRESS=$bus_address mount_second
ls "$MNT2" >/dev/null

step "A: the marker on, a grace of $grace s"
x_config '[upload]' 'debounce = "60s"' '[timeouts]' "shutdown_grace = \"${grace}s\"" \
    '[sync]' 'pending_marker = true'
if is_mounted; then pd unmount "$MNT"; fi
mount_now
scenario_area x12
wait_queue_empty
id_a=$(device_uid 1)
id_a=${id_a#pdmount-}
[[ -n $id_a ]] || fail "no device id for A"
marker=$id_a.pending
# A leftover from an earlier run is trashed by A's drain; B must not show it.
wait_until "$limit" "no old marker on B" no_incoming_b
"$PDMOUNT" fake profile --rtt 80 --up 1000000 "$FAKE_DIR"
mark=$(fake_change_mark)

step "24 MiB queued at 1 MB/s, SIGTERM: the grace runs out, the marker is left"
log_mark
write_files "$area" x12 24 1024
term_daemon
((stop_ms <= (grace + 3) * 1000)) || fail "the stop took $stop_ms ms; the grace is $grace s"
expect_log 'left a pending marker for other devices' "the marker"
expect_log 'the grace ran out' "the flush"
expect_log '[0-9]+ entr(y|ies) left for the next mount' "the stop"
marker_link=$(fake_changes "$mark" | awk -v name="$marker" '$2 == "created-file" && $4 == name { print $3 }' | tail -1)
if [[ -z $marker_link ]]; then
    # A marker from an earlier run got a new revision instead.
    marker_link=$(fake_changes | awk -v name="$marker" '$2 == "created-file" && $4 == name { print $3 }' | tail -1)
fi
[[ -n $marker_link ]] || fail "no $marker on the drive: $(fake_changes "$mark" | head)"
"$PDMOUNT" fake profile clear "$FAKE_DIR"

step "B shows Incoming: and one notification"
wait_until "$limit" "Incoming: on B" incoming_b
incoming_b
json=$(cat "$MNT2/.pdmount/$marker")
echo "marker: $json"
python3 -I - "$json" <<'PY' || fail "the marker's JSON is not as specified"
import json, sys
m = json.loads(sys.argv[1])
assert set(m) == {"version", "device", "time", "entries", "bytes"}, m
assert m["version"] == 1 and m["device"] == "dev-a", m
assert m["entries"] >= 1 and m["bytes"] >= 1024, m
PY
grep -q 'f-' <<<"$json" && fail "the marker names a file"
notes=$(grep -c 'changes\? on their way from dev-a' "$notify_log" || true)
expect_eq "notifications on B" "$notes" 1

step ".pdmount is read-only on B"
for attempt in "touch $MNT2/.pdmount/x" "rm -f $MNT2/.pdmount/$marker" "rmdir $MNT2/.pdmount" \
    "mv $MNT2/.pdmount $MNT2/moved" "mkdir $MNT2/.pdmount/d"; do
    if out=$($attempt 2>&1); then fail "$attempt succeeded"; fi
    grep -q 'Read-only file system' <<<"$out" || fail "$attempt: $out"
done
[[ -f "$MNT2/.pdmount/$marker" ]] || fail "the marker is gone on B"

step "A's next mount drains and trashes the marker; B's Incoming: goes"
x_restore
log_mark
mount_now
wait_queue_empty
marker_trashed() { fake_changes "$mark" | grep -q "^[0-9]* trashed $marker_link\$"; }
wait_until 30 "the marker in the trash" marker_trashed
expect_log "trashed this device's pending marker" "the drain"
wait_until "$limit" "Incoming: gone on B" no_incoming_b
# The kernel's entry for the marker is invalidated by the notify thread, a moment after
# B's engine applied the event (and `status` changed), so the name may linger briefly.
marker_gone_b() { [[ ! -e "$MNT2/.pdmount/$marker" ]]; }
wait_until 5 "the marker gone on B" marker_gone_b
notes=$(grep -c 'changes\? on their way from dev-a' "$notify_log" || true)
expect_eq "notifications on B" "$notes" 1

step "nothing was lost"
pd unmount "$MNT"
expect_on_drive "$area" x12
check_hot_path

# D096: a grace of 10 s or less writes the marker at the start of the flush;
# when the flush then uploads everything, the same stop trashes it.
step "A: a grace of 5 s; the marker goes at the start and is trashed when the flush ends"
x_config '[upload]' 'debounce = "60s"' '[timeouts]' 'shutdown_grace = "5s"' \
    '[sync]' 'pending_marker = true'
pd unmount "$MNT"
mount_now
wait_queue_empty
mark=$(fake_change_mark)
log_mark
write_files "$area/short" x12-short 3 16
term_daemon
((stop_ms <= 5000 + 3000)) || fail "the stop took $stop_ms ms; the grace is 5 s"
expect_log 'left a pending marker for other devices' "the marker"
expect_log 'flushed [0-9]+ entr(y|ies) in' "the flush"
expect_log "trashed this device's pending marker" "the trash in the same stop"
if log_since_mark | grep -Eq 'entr(y|ies) left for the next mount'; then fail "entries were left"; fi
short_link=$(fake_changes "$mark" | awk -v name="$marker" '$2 == "created-file" && $4 == name { print $3 }' | tail -1)
[[ -n $short_link ]] || fail "no new $marker on the drive: $(fake_changes "$mark" | head)"
fake_changes "$mark" | grep -q "^[0-9]* trashed $short_link\$" || fail "the marker was not trashed"
x_restore
mount_now
# The drive's root as before; another client may remove the folder.
"$PDMOUNT" fake trash --path /.pdmount "$FAKE_DIR"
echo "x12: PASS"
