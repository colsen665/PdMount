#!/usr/bin/env bash
# The daemon under a supervisor (DESIGN.md §14.4, D093): readiness through
# NOTIFY_SOCKET and --notify-fd, `unmount` refusing under a service,
# --clear-stale after kill -9 and its gate. The foreground daemons here are
# started by hand, as a service manager would.
source "$(dirname "$0")/lib.sh"
[[ $BACKEND == fake ]] || { echo "svc starts daemons by hand over the fake; skipped for BACKEND=$BACKEND"; exit 0; }
[[ $VERIFY_MODE == live ]] || { echo "svc: daemons started by hand, live mode only; skipped in $VERIFY_MODE mode"; exit 0; }
ensure_dataset
VERIFY_MODE=live prepare_mount
make_workdir
daemon=
listener=
stop_daemon() {
    if [[ -n $daemon ]]; then
        kill -TERM "$daemon" 2>/dev/null || true
        wait "$daemon" 2>/dev/null || true
        daemon=
    fi
}
svc_cleanup() {
    stop_daemon
    [[ -z $listener ]] || kill "$listener" 2>/dev/null || true
    scenario_cleanup
}
trap svc_cleanup EXIT

# Starts `mount --foreground` by hand in the background; $daemon is its pid.
# The environment passes NOTIFY_SOCKET.
start_fg() { # <mount options...>
    "$PDMOUNT" --profile "$PROFILE" "${secrets_flag[@]}" mount --foreground \
        "${backend_flag[@]}" "$@" "$MNT" >>"$work/daemon.log" 2>&1 &
    daemon=$!
}
wait_for() { # <what> <seconds> <command...>
    local what=$1 limit=$2 start=$SECONDS
    shift 2
    until "$@"; do
        ((SECONDS - start < limit)) || fail "timed out waiting for $what"
        sleep 0.2
    done
}
daemon_gone() { ! kill -0 "$daemon" 2>/dev/null; }
mount_answers() { is_mounted && ls "$MNT" >/dev/null 2>&1; }

step "a plain daemon to start from: unmount the one prepare_mount made"
pd unmount "$MNT"
if is_mounted; then fail "still mounted"; fi

step "NOTIFY_SOCKET: READY=1 arrives once the mount answers (path and abstract sockets)"
sock=$work/notify.sock
for address in "$sock" "@pdmount-svc-$$"; do
    rm -f "$work/notify.out" "$work/notify.bound"
    python3 -I - "$address" "$work/notify.out" "$work/notify.bound" <<'PY' &
import socket, sys
address, out, bound = sys.argv[1:4]
s = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
s.bind("\0" + address[1:] if address.startswith("@") else address)
s.settimeout(60)
open(bound, "w").close()
data = s.recv(64)
open(out, "wb").write(data)
PY
    listener=$!
    wait_for "the listener" 10 test -e "$work/notify.bound"
    NOTIFY_SOCKET=$address start_fg --service openrc
    wait "$listener" || fail "no datagram arrived on $address"
    listener=
    expect_eq "datagram on $address" "$(cat "$work/notify.out")" "READY=1"
    mount_answers || fail "READY=1 came before the mount answered"
    if [[ $address == "$sock" ]]; then
        step "unmount under a service refuses, and names the stop command"
        if out=$(pd unmount "$MNT" 2>&1); then fail "unmount under a service exits 0: $out"; fi
        grep -q "rc-service --user pdmount.$PROFILE stop" <<<"$out" || fail "the refusal does not name the service command: $out"
        mount_answers || fail "the refused unmount changed the mount"
        kill -0 "$daemon" || fail "the refused unmount stopped the daemon"
    fi
    stop_daemon
    if is_mounted; then fail "the mount is still there after SIGTERM"; fi
done

step "--notify-fd: a newline, then the descriptor is closed"
fifo=$work/fd.fifo
mkfifo "$fifo"
cat "$fifo" >"$work/fd.out" &
listener=$!
"$PDMOUNT" --profile "$PROFILE" "${secrets_flag[@]}" mount --foreground --notify-fd 3 \
    "${backend_flag[@]}" "$MNT" 3>"$fifo" >>"$work/daemon.log" 2>&1 &
daemon=$!
# cat ends when every write end is closed, so this also shows the close.
wait "$listener" || fail "the notification descriptor was not closed"
listener=
expect_eq "bytes on the descriptor" "$(od -An -c <"$work/fd.out" | tr -d ' ')" '\n'
mount_answers || fail "the newline came before the mount answered"
step "no --service: unmount works as before"
pd unmount "$MNT"
wait_for "the daemon to exit" 60 daemon_gone
daemon=

step "--notify-fd on a descriptor that is not a pipe: left untouched (a respawned daemon, D109)"
printf 'log\n' >"$work/fd.file"
"$PDMOUNT" --profile "$PROFILE" "${secrets_flag[@]}" mount --foreground --notify-fd 3 \
    "${backend_flag[@]}" "$MNT" 3>>"$work/fd.file" >>"$work/daemon.log" 2>&1 &
daemon=$!
wait_for "the mount" 60 mount_answers
expect_eq "the file on descriptor 3" "$(cat "$work/fd.file")" "log"
grep -q 'not a pipe' "$work/daemon.log" || fail "the log does not say why nothing was written to descriptor 3"
pd unmount "$MNT"
wait_for "the daemon to exit" 60 daemon_gone
daemon=

step "--clear-stale leaves a mount alone while its daemon answers"
start_fg
wait_for "the mount" 60 mount_answers
first=$daemon
if "$PDMOUNT" --profile "$PROFILE" "${secrets_flag[@]}" mount --foreground --clear-stale \
    "${backend_flag[@]}" "$MNT" >"$work/second.log" 2>&1; then fail "a second daemon started over the first"; fi
grep -qE 'is still running|already mounted' "$work/second.log" || fail "the second start does not say why it refused: $(cat "$work/second.log")"
mount_answers || fail "the failed second start damaged the mount"
kill -0 "$first" || fail "the first daemon is gone"

step "after kill -9 the mount is stale; a start without --clear-stale refuses"
kill -9 "$first"
wait "$first" 2>/dev/null || true
daemon=
if ls "$MNT" >/dev/null 2>&1; then fail "the mount still answers after kill -9"; fi
if "$PDMOUNT" --profile "$PROFILE" "${secrets_flag[@]}" mount --foreground \
    "${backend_flag[@]}" "$MNT" >"$work/third.log" 2>&1; then fail "a start over a stale mount worked"; fi
grep -q 'stale' "$work/third.log" || fail "the refusal does not mention the stale mount: $(cat "$work/third.log")"

step "--clear-stale detaches it and mounts again"
start_fg --clear-stale --service systemd
wait_for "the mount" 60 mount_answers
grep -q 'removing a stale mount' "$work/daemon.log" || fail "the log does not show the stale mount being removed"

step "unmount --force under a service: stops the daemon and names the service command"
out=$(pd unmount --force "$MNT" 2>&1) || fail "unmount --force failed: $out"
grep -q "systemctl --user start pdmount@$PROFILE" <<<"$out" || fail "the note does not name the start command of systemd's unit: $out"
grep -q "stays stopped" <<<"$out" || fail "the note does not say that systemd's unit stays stopped: $out"
wait_for "the daemon to exit" 60 daemon_gone
daemon=
if is_mounted; then fail "still mounted after unmount --force"; fi

# What the systemd unit's ExecStopPost runs (D094): a stale mount only.
mount_listed() { grep -qF " $MNT " /proc/self/mountinfo; }

step "unmount --if-stale: no mount at all is not an error and changes nothing"
out=$(pd unmount --if-stale "$MNT" 2>&1) || fail "unmount --if-stale on an unmounted path failed: $out"
if mount_listed; then fail "a mount appeared"; fi

step "unmount --if-stale leaves a live daemon's mount alone (a failed second start runs it)"
start_fg --service systemd
wait_for "the mount" 60 mount_answers
code=0
"$PDMOUNT" --profile "$PROFILE" "${secrets_flag[@]}" mount --foreground --service systemd --clear-stale \
    "${backend_flag[@]}" "$MNT" >"$work/unit.log" 2>&1 || code=$?
# Failed before readiness: the code that the unit's RestartPreventExitStatus= names (D109).
expect_eq "exit code of a start that failed before it was ready" "$code" 78
out=$(pd unmount --if-stale "$MNT" 2>&1) || fail "unmount --if-stale failed over a live daemon: $out"
mount_answers || fail "unmount --if-stale detached a live daemon's mount: $out"
kill -0 "$daemon" || fail "the live daemon is gone"

step "unmount --if-stale and --force or --wait exclude each other"
if pd unmount --if-stale --force "$MNT" >/dev/null 2>&1; then fail "--if-stale --force was accepted"; fi
if pd unmount --if-stale --wait "$MNT" >/dev/null 2>&1; then fail "--if-stale --wait was accepted"; fi

step "unmount --if-stale detaches the mount of a kill -9'd daemon"
kill -9 "$daemon"
wait "$daemon" 2>/dev/null || true
daemon=
mount_listed || fail "the mount is gone after kill -9 already"
out=$(pd unmount --if-stale "$MNT" 2>&1) || fail "unmount --if-stale failed on a stale mount: $out"
grep -q 'stale mount' <<<"$out" || fail "no word about the stale mount: $out"
if mount_listed; then fail "the stale mount is still there after unmount --if-stale"; fi

step "two profiles on one mountpoint (V-6, D127): the mount names its profile"
start_fg --service systemd
wait_for "the mount" 60 mount_answers
grep -F " $MNT " /proc/self/mountinfo | grep -qF " - fuse.pdmount pdmount:$PROFILE " ||
    fail "the mount's source does not name profile $PROFILE: $(grep -F " $MNT " /proc/self/mountinfo)"
conflict="another PdMount mount (profile $PROFILE) is at $MNT"

step "the second profile's --clear-stale start refuses, exit 78, and leaves the live mount alone"
# Other scenarios use the second profile too; its daemon may still be stopping.
second_daemon_gone() {
    local f=$XDG_RUNTIME_DIR/pdmount/$PROFILE2.pid
    [[ ! -s $f ]] || ! kill -0 "$(cat "$f")" 2>/dev/null
}
if is_mounted_second; then pd2 unmount "$MNT2" >/dev/null 2>&1 || true; fi
wait_for "the second profile's daemon to end" 60 second_daemon_gone
code=0
"$PDMOUNT" --profile "$PROFILE2" "${secrets_flag[@]}" mount --foreground --service systemd --clear-stale \
    "${backend_flag[@]}" "$MNT" >"$work/other.log" 2>&1 || code=$?
expect_eq "exit code of the second profile's start" "$code" 78
grep -qF "$conflict" "$work/other.log" || fail "the refusal does not name the conflict: $(cat "$work/other.log")"
mount_answers || fail "the second profile's start detached the first profile's mount"
kill -0 "$daemon" || fail "the first profile's daemon is gone"

step "the second profile's unmount --if-stale, unmount and unmount --force refuse too"
code=0
out=$(pd2 unmount --if-stale "$MNT" 2>&1) || code=$?
expect_eq "exit code of the second profile's unmount --if-stale" "$code" 1
grep -qF "$conflict" <<<"$out" || fail "unmount --if-stale does not name the conflict: $out"
if out=$(pd2 unmount "$MNT" 2>&1); then fail "the second profile's unmount exits 0: $out"; fi
grep -qF "$conflict" <<<"$out" || fail "unmount does not name the conflict: $out"
if out=$(pd2 unmount --force "$MNT" 2>&1); then fail "the second profile's unmount --force exits 0: $out"; fi
grep -qF "pdmount --profile $PROFILE unmount --force" <<<"$out" || fail "unmount --force does not name the owner's command: $out"
mount_answers || fail "the second profile detached the first profile's mount"
kill -0 "$daemon" || fail "the first profile's daemon is gone"

step "a dead mount is stale for the second profile too and is cleared"
kill -9 "$daemon"
wait "$daemon" 2>/dev/null || true
daemon=
mount_listed || fail "the mount is gone after kill -9 already"
out=$(pd2 unmount --if-stale "$MNT" 2>&1) || fail "unmount --if-stale failed on a dead mount: $out"
grep -q 'stale mount' <<<"$out" || fail "no word about the stale mount: $out"
if mount_listed; then fail "the dead mount is still there after the second profile's unmount --if-stale"; fi

step "back to a normal mount for the next scenario"
mount_now
mount_answers || fail "the normal mount does not answer"
echo "svc: PASS"
