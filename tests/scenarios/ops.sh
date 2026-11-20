#!/usr/bin/env bash
# Operations (DESIGN.md §14.4, D062): a running upload finishes during the
# shutdown within `timeouts.shutdown_grace`, and is retried at the next mount
# when the grace is too short; `mount -o ro`; the status lines.
source "$(dirname "$0")/lib.sh"
[[ $BACKEND == fake ]] || { echo "ops needs the fake's network profile; skipped for BACKEND=$BACKEND"; exit 0; }
ensure_dataset
VERIFY_MODE=live prepare_mount
make_workdir
scenario_area ops
config=$XDG_CONFIG_HOME/pdmount/$PROFILE.toml
saved=$(cat "$config")
restore() {
    "$PDMOUNT" fake profile clear "$FAKE_DIR" || true
    printf '%s\n' "$saved" >"$config"
}
trap restore EXIT

# Writes a file of 1 MiB, starts its upload at 200 kB/s (about 5 s) and stops
# the daemon with SIGTERM while it runs.
term_during_upload() { # <name>
    local pid
    head -c 1048576 /dev/urandom >"$work/$1"
    cp "$work/$1" "$area/$1"
    "$PDMOUNT" fake profile --up 200000 "$FAKE_DIR"
    pd sync --timeout 2 "$area" >/dev/null && fail "the upload was not slow"
    pd queue list "$area" | grep -q 'running' || fail "no upload is running: $(pd queue list "$area")"
    pid=$(daemon_pid)
    kill -TERM "$pid"
    while kill -0 "$pid" 2>/dev/null; do sleep 0.1; done
    "$PDMOUNT" fake profile clear "$FAKE_DIR"
}

step "the upload finishes within the grace"
term_during_upload within
mount_now
expect_eq "queue after the grace" "$(pd queue list "$area")" "Nothing is queued."
cmp "$work/within" "$area/within" || fail "within differs"

step "a grace that is too short: the next mount uploads it"
printf '\n[timeouts]\nshutdown_grace = "200ms"\n' >>"$config"
pd unmount "$MNT"
mount_now
term_during_upload short
printf '%s\n' "$saved" >"$config"
mount_now
pd queue list "$area" | grep -q 'short$' || fail "the cut upload is not queued: $(pd queue list "$area")"
pd sync "$area"
pd unmount "$MNT"
pd reset --keep-login >/dev/null
mount_now
cmp "$work/short" "$area/short" || fail "short differs on the server"

step "status names the event stream"
for _ in {1..80}; do
    status_text | grep -q '^Events: *last poll [0-9]* s ago' && break
    sleep 0.5
done
status_text | grep -q '^Events: *last poll [0-9]* s ago' || fail "no event line: $(status_text)"

step "doctor's clock line has the mount's server time offset (D133)"
# The fake answers no Drive API requests, so the daemon has adopted none.
clock_line=$(pd doctor 2>/dev/null | grep '^\[[^]]*\] clock ' || true)
[[ $clock_line == *"the mount has adopted no server time offset"* ]] \
    || fail "doctor's clock line: ${clock_line:-none}"

step "mount -o ro"
pd unmount "$MNT"
pd mount "${backend_flag[@]}" -o ro "$MNT"
status_text | grep -q 'read-only' || fail "status does not say read-only"
if printf x 2>/dev/null >"$area/denied"; then fail "a write on a read-only mount worked"; fi
cmp "$work/within" "$area/within" || fail "a read on a read-only mount failed"
pd unmount "$MNT"
mount_now
check_hot_path
echo "ops: PASS"
