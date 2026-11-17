#!/usr/bin/env bash
# X13 (DESIGN.md §15.3, D102, D106): the desktop's session bus handed to the
# daemon, and a daemon that starts without a keyring. Two private session
# buses: A is the daemon's own (as the OpenRC user bus), without a Secret
# Service or a notification server; B is the desktop's, where gnome-keyring
# starts later and a notification server can be started (an activation
# file). A notify-send stub on PATH records the bus it was given.
# The keyring part: B's keyring first lacks the cache key (the daemon waits
# and deletes nothing), then has it; then a daemon started as a service on B
# with the collection locked (ready at once, no unlock dialog, a quick stop
# while a login's dialog is open) goes online when the collection unlocks.
# Then the opposite layout (D102 amended, a Gentoo system with i3): the
# keyring on A, the daemon's own bus, and a desktop bus C with the
# notification server and an activatable Secret Service that never answers;
# after an attach from C the keyring's write and read go to A, the
# notification to C, and A starts programs on C's display (a probe that D-Bus
# starts on A records it).
# A restart keeps the desktop's bus (D102 amended): the attach from B is
# saved, a daemon started again takes it without a new attach, and a saved
# attach of a bus that is gone, or of the daemon's own bus, is removed. An
# attach while no daemon runs is saved by the client and taken by the next
# start (D102 amended 2026-11-14). From that start on the profile has a
# further root (D110 amended): `status` says it waits for the keyring, a
# `mount` of it is refused, and it mounts by itself when the wait ends.
# Fake, live mode only, its own profile (keyring mode, the others use files).
# Skipped without dbus-daemon; the keyring part needs gnome-keyring-daemon,
# its locked part python3 with dbus and gi (scripts/repro/stub-prompter.py).
source "$(dirname "$0")/lib.sh"
[[ $BACKEND == fake ]] || { echo "x13 needs the fake and private buses; skipped for BACKEND=$BACKEND"; exit 0; }
command -v dbus-daemon >/dev/null || { echo "x13: no dbus-daemon; skipped"; exit 0; }
x_live_only
ensure_dataset
ensure_fake_drive
make_workdir

# A profile of its own: its cache key moves into B's keyring here.
PROFILE=fake-x13
MNT=$SCENARIO_ROOT/fake-mnt-x13
TEST_ROOT=$MNT/pdmount-test
REMOTE=$MNT/pdmount-m1
fake_config "$PROFILE" dev-x13 "$MNT"
# A further root added below; one an earlier run left in the config is not ours.
ROOT_MNT=$SCENARIO_ROOT/fake-root-x13
root_marker='# x13: a further root'
sed -i "/^$root_marker/,\$d" "$(x_config_file)"
x_config_save
mkdir -p "$MNT"
# The attach saved for the next start; one an earlier run left is not ours.
attach_file=$XDG_RUNTIME_DIR/pdmount/$PROFILE.attach
rm -f "$attach_file"

bus_conf() { # <socket> [<service dir>]
    cat <<EOF
<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:path=$1</listen>
  <auth>EXTERNAL</auth>
  ${2:+<servicedir>$2</servicedir>}
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
EOF
}
pids=()
# An activation file: D-Bus lists <name> as activatable on the bus and runs
# <command> when a call needs it.
activatable() { # <service dir> <name> <command>
    mkdir -p "$1"
    printf '[D-BUS Service]\nName=%s\nExec=%s\n' "$2" "$3" >"$1/$2.service"
}
start_bus() { # <name> [<service dir>]; sets bus_address
    local socket=$work/$1
    bus_conf "$socket" "${2:-}" >"$work/$1.conf"
    dbus-daemon --config-file="$work/$1.conf" --nofork --nopidfile &
    pids+=($!)
    until [[ -S $socket ]]; do sleep 0.1; done
    bus_address=unix:path=$socket
}
daemon=
x13_cleanup() {
    if [[ -n $daemon ]]; then
        kill -TERM "$daemon" 2>/dev/null || true
        wait "$daemon" 2>/dev/null || true
    fi
    if is_mounted; then pd unmount "$MNT" >/dev/null 2>&1 || true; fi
    if awk -v mnt="$ROOT_MNT" '$2 == mnt { found = 1 } END { exit !found }' /proc/self/mounts; then
        fusermount3 -u "$ROOT_MNT" 2>/dev/null || fusermount3 -uz "$ROOT_MNT" || true
    fi
    x_restore
    for pid in "${pids[@]}"; do kill "$pid" 2>/dev/null || true; done
    rm -f "$attach_file"
    # X13_KEEP_LOG=<file>: a copy of the waiting daemon's log, to read after the run.
    [[ -z ${X13_KEEP_LOG:-} ]] || cp "$work/daemon.log" "$X13_KEEP_LOG" 2>/dev/null || true
    scenario_cleanup
}
trap x13_cleanup EXIT
# A: a probe that D-Bus can start, which records the display it was started
# with (run by sh: the scenario root may be mounted noexec).
printf 'printf %%s "$DISPLAY $XAUTHORITY" >"%s.tmp" && mv "%s.tmp" "%s"\n' \
    "$work/probe-display" "$work/probe-display" "$work/probe-display" >"$work/display-probe"
activatable "$work/busA-services" org.pdmount.X13.DisplayProbe "$(command -v sh) $work/display-probe"
start_bus busA "$work/busA-services"
bus_a=$bus_address
# B: a notification server that D-Bus would start (the stub notify-send
# does not call it); without one, no notification goes to B (D102 amended).
activatable "$work/busB-services" org.freedesktop.Notifications "$(command -v false)"
start_bus busB "$work/busB-services"
bus_b=$bus_address

touch "$work/notify.log" "$work/notify-bus.log"
export X11_NOTIFY_LOG=$work/notify.log X13_NOTIFY_BUS_LOG=$work/notify-bus.log
stub_path=$repo/tests/scenarios/stubs:$PATH

wait_for() { # <what> <seconds> <command...>
    local what=$1 limit=$2 start=$SECONDS
    shift 2
    until "$@"; do
        ((SECONDS - start < limit)) || fail "timed out waiting for $what; daemon log: $(tail -20 "$work/daemon.log")"
        sleep 0.2
    done
}
mount_answers() { is_mounted && ls "$MNT" >/dev/null 2>&1; }
waits_for_keyring() { status_text | grep -q 'waits for the keyring'; }
online() { status_text | grep -q '^Connection: online'; }
daemon_says() { grep -Eq "$1" "$work/daemon.log"; }
attach_from() { # <bus address> [<env arguments>...]
    env "${@:2}" DBUS_SESSION_BUS_ADDRESS="$1" "$PDMOUNT" --profile "$PROFILE" session attach
}
launch_on_a() { # a daemon with A as its own bus
    PATH=$stub_path DBUS_SESSION_BUS_ADDRESS=$bus_a PDMOUNT_KEYRING_RETRY_SECS=2 PDMOUNT_KEYRING_TIMEOUT_SECS=10 \
        "$PDMOUNT" --profile "$PROFILE" -v mount --foreground "${backend_flag[@]}" "$MNT" >>"$work/daemon.log" 2>&1 &
    daemon=$!
    wait_for "the mount" 60 mount_answers
}
start_on_a() { # a daemon on A, which has no Secret Service: it waits
    launch_on_a
    wait_for "the waiting state" 20 waits_for_keyring
}
stop_daemon() {
    kill -TERM "$daemon"
    wait "$daemon" || fail "the daemon exited with an error"
    daemon=
}
notified() { wc -l <"$work/notify.log"; }
more_notified() { (($(notified) > $1)); }
# The wait's own notification, one per daemon; a restarted daemon also says
# that the last session's changes are still uploading (M5.2).
keyring_notices() { grep -c 'keyring needed' "$work/notify.log" || true; }
more_keyring_notices() { (($(keyring_notices) > $1)); }

step "the profile mounts once with its secrets in files, so it has mounted before"
if is_mounted; then pd unmount "$MNT"; fi
mount_now
scenario_area x13
printf before >"$area/before"
wait_queue_empty
# Blocks in the disk cache, sealed with the cache key: a wait must keep them.
big_file=$(find "$REMOTE" -type f -size +64k | head -1)
cat "$big_file" >/dev/null
pd unmount "$MNT"
blocks_dir=$XDG_CACHE_HOME/pdmount/$PROFILE/blocks
disk_blocks() { find "$blocks_dir" -type f | wc -l; }
blocks_before=$(disk_blocks)
((blocks_before > 0)) || fail "no blocks in the disk cache after a read"
[[ -f $XDG_DATA_HOME/pdmount/$PROFILE/cache.key ]] || fail "the cache key is not in a file"

step "a start in keyring mode on bus A, which has no Secret Service: the daemon mounts and waits"
PATH=$stub_path DBUS_SESSION_BUS_ADDRESS=$bus_a PDMOUNT_KEYRING_RETRY_SECS=2 PDMOUNT_KEYRING_TIMEOUT_SECS=10 \
    "$PDMOUNT" --profile "$PROFILE" -v mount --foreground "${backend_flag[@]}" "$MNT" >>"$work/daemon.log" 2>&1 &
daemon=$!
wait_for "the mount" 60 mount_answers
wait_for "the waiting state" 20 waits_for_keyring
status_text | grep -q '^Connection: needs login' || fail "not needs_login: $(status_text)"
daemon_says 'the keyring is unavailable; mounting from the stored state' || fail "no log line about the wait"
sleep 5
kill -0 "$daemon" || fail "the daemon exited while waiting for the keyring"
# The tree comes from the store; clean content is in the disk cache, which
# the cache key opens, or on the drive: neither is there yet (D106).
expect_eq "the stored tree" "$(ls "$area")" before
expect_eq "a stored size" "$(stat -c %s "$area/before")" 6
if timeout 30 cat "$area/before" >/dev/null 2>&1; then fail "clean content was read without the cache key"; fi
printf during >"$area/during"
status_text | grep -qE '^Queue: +[1-9]' || fail "the write is not queued: $(status_text)"
expect_eq "notifications without a notification server" "$(wc -l <"$work/notify.log")" 0

step "pdmount login asks the daemon to try the keyring, and says that it still cannot"
if out=$(DBUS_SESSION_BUS_ADDRESS=$bus_a PDMOUNT_KEYRING_TIMEOUT_SECS=10 "$PDMOUNT" --profile "$PROFILE" login 2>&1); then
    fail "login succeeded without a keyring: $out"
fi
grep -q 'still cannot read the keyring' <<<"$out" || fail "login does not say why: $out"

step "session attach: the daemon's own bus changes nothing, an unreachable or foreign address is refused"
out=$(attach_from "$bus_a") || fail "attach of the own bus failed: $out"
grep -q 'already uses this session' <<<"$out" || fail "own bus: $out"
if out=$(attach_from "unix:path=$work/no-bus" 2>&1); then fail "an unreachable bus was attached: $out"; fi
grep -q 'cannot use that bus' <<<"$out" || fail "unreachable bus: $out"
if out=$(attach_from "tcp:host=localhost,port=1" 2>&1); then fail "a tcp address was accepted: $out"; fi
expect_eq "notifications" "$(wc -l <"$work/notify.log")" 0

step "session attach from bus B: kept, and the one notification lands on B"
out=$(attach_from "$bus_b") || fail "attach failed: $out"
grep -q 'now has this session.s bus (keyring: none found; notifications: this session.s bus)' <<<"$out" ||
    fail "attach: $out"
wait_for "the notification" 20 test -s "$work/notify-bus.log"
daemon_says 'attached the desktop.s session bus' || fail "no log line about the attach"
expect_eq "the bus of the notification" "$(cat "$work/notify-bus.log")" "$bus_b"
grep -q 'keyring needed' "$work/notify.log" || fail "notification: $(cat "$work/notify.log")"
kill -0 "$daemon" || fail "the daemon exited"
waits_for_keyring || fail "B has no keyring yet, but the daemon stopped waiting"

step "the attach is saved; a restart uses B again without a new attach (D102 amended)"
[[ -f $attach_file && ! -L $attach_file ]] || fail "no saved attach after the attach from B"
expect_eq "the saved attach's mode" "$(stat -c %a "$attach_file")" 600
saved=$(cat "$attach_file")
grep -qF "\"address\":\"$bus_b\"" <<<"$saved" || fail "the saved attach: $saved"
before=$(notified)
notices=$(keyring_notices)
stop_daemon
start_on_a
daemon_says 'using the desktop.s session bus saved by the last attach' || fail "no log line about the saved bus"
st=$(status_text)
grep -q "^Bus: .*desktop's bus attached; keyring: none found, notifications: the desktop's bus" <<<"$st" ||
    fail "status after the restart: $st"
# The new wait's notification, and the one about the queued write, go to B.
wait_for "the keyring notification after the restart" 20 more_keyring_notices "$notices"
expect_eq "the bus of the notifications after the restart" \
    "$(tail -n "+$((before + 1))" "$work/notify-bus.log" | sort -u)" "$bus_b"
[[ -f $attach_file ]] || fail "the used saved attach was removed"

step "a saved attach of a bus that is gone is removed at the start; the next attach saves B again"
stop_daemon
printf '{"address":"unix:path=%s","display":{}}\n' "$work/no-bus" >"$attach_file"
notices=$(keyring_notices)
start_on_a
daemon_says 'saved by the last attach is not used; removed the file' || fail "no log line about the removal"
[[ ! -e $attach_file ]] || fail "the saved attach of a gone bus is still there"
st=$(status_text)
if grep -q '^Bus:' <<<"$st"; then fail "a bus is attached after a gone one: $st"; fi
# A has no notification server: the wait's notification waits for the attach.
out=$(attach_from "$bus_b") || fail "attach failed: $out"
wait_for "the keyring notification" 20 more_keyring_notices "$notices"
expect_eq "the bus of the notification" "$(tail -1 "$work/notify-bus.log")" "$bus_b"
[[ -f $attach_file ]] || fail "the attach from B is not saved"

step "session attach before the daemon starts: saved by the client, taken by the start (D102 amended)"
stop_daemon
rm -f "$attach_file"
# From here on a further root (D110 amended): it waits with the daemon and
# mounts when the wait ends. Added while no daemon holds the fake drive.
read -r root_volume root_link <<<"$("$PDMOUNT" fake root --name x13-root "$FAKE_DIR")"
x_config "$root_marker (D110 amended)" '[[roots]]' 'kind = "test"' "mountpoint = \"$ROOT_MNT\"" \
    '[[roots.tree]]' 'name = "x13-root"' "volume = \"$root_volume\"" "root = \"$root_link\""
root_state() { status_text | sed -n "s|^Root: *$ROOT_MNT (test): ||p"; }
root_mounted() { [[ $(root_state) == mounted ]]; }
# A desktop whose bus is the service's ($XDG_RUNTIME_DIR/bus): nothing is saved.
same_run=$SCENARIO_ROOT/x13-run
rm -rf "$same_run"
mkdir -m 700 "$same_run"
ln -s "${bus_a#unix:path=}" "$same_run/bus"
out=$(XDG_RUNTIME_DIR=$same_run attach_from "$bus_a") || fail "attach without a daemon failed: $out"
grep -q 'is running; nothing to attach' <<<"$out" || fail "the service's own bus: $out"
[[ ! -e $same_run/pdmount/$PROFILE.attach ]] || fail "an attach of the service's own bus was saved"
rm -rf "$same_run"
out=$(attach_from "$bus_b") || fail "attach without a daemon failed: $out"
grep -q 'runs; its next start takes this session.s bus' <<<"$out" || fail "attach without a daemon: $out"
[[ -f $attach_file && ! -L $attach_file ]] || fail "no saved attach after an attach without a daemon"
expect_eq "the saved attach's mode" "$(stat -c %a "$attach_file")" 600
uses=$(grep -c 'using the desktop.s session bus saved by the last attach' "$work/daemon.log")
notices=$(keyring_notices)
start_on_a
expect_eq "uses of the saved bus" \
    "$(grep -c 'using the desktop.s session bus saved by the last attach' "$work/daemon.log")" "$((uses + 1))"
st=$(status_text)
grep -q "^Bus: .*desktop's bus attached" <<<"$st" || fail "status after the start: $st"
expect_eq "the further root while waiting" "$(root_state)" "waits for the keyring"
if out=$(pd mount "$ROOT_MNT" 2>&1); then fail "a further root mounted while waiting: $out"; fi
grep -q 'the daemon waits for the keyring' <<<"$out" || fail "mount <root> while waiting: $out"
wait_for "the keyring notification" 20 more_keyring_notices "$notices"
expect_eq "the bus of the notification" "$(tail -1 "$work/notify-bus.log")" "$bus_b"

if command -v gnome-keyring-daemon >/dev/null; then
    gk=
    start_gk() { # [<bus> <dir name>]: gnome-keyring there (default B), its login collection unlocked
        local bus=${1:-$bus_b} dir=$work/${2:-gk}
        mkdir -p "$dir/home" "$dir/data" "$dir/run"
        chmod 700 "$dir/run"
        printf %s x13-throwaway | env HOME="$dir/home" XDG_DATA_HOME="$dir/data" \
            XDG_RUNTIME_DIR="$dir/run" DBUS_SESSION_BUS_ADDRESS="$bus" \
            gnome-keyring-daemon --foreground --components=secrets --unlock >>"$work/gk.log" 2>&1 &
        gk=$!
        pids+=($gk)
        timeout 10 gdbus wait --address "$bus" org.freedesktop.secrets || fail "gnome-keyring did not start on $bus"
    }
    stop_gk() {
        kill "$gk"
        wait "$gk" 2>/dev/null || true
        gk=
    }
    key_file=$XDG_DATA_HOME/pdmount/$PROFILE/cache.key
    no_cache_key() { status_text | grep -q 'no cache key'; }

    step "gnome-keyring starts on B without the profile's cache key: the daemon waits, deletes nothing, makes no key"
    mv "$key_file" "$work/cache.key"
    cp -p "$work/cache.key" "$work/cache.key.saved" # for the layout of C below
    start_gk
    wait_for "the missing cache key" 60 no_cache_key
    # Three timer intervals: a wait only the user can end is not retried by the timer.
    sleep 6
    expect_eq "attempts that found no cache key" "$(grep -c 'the start cannot finish' "$work/daemon.log")" 1
    expect_eq "blocks in the disk cache" "$(disk_blocks)" "$blocks_before"
    [[ ! -e $key_file ]] || fail "a new cache key was made"
    kill -0 "$daemon" || fail "the daemon exited"

    step "the key is back and the desktop attaches again: the mount goes online"
    mv "$work/cache.key" "$key_file"
    out=$(attach_from "$bus_b") || fail "attach failed: $out"
    wait_for "online" 60 online
    daemon_says 'the keyring answered' || fail "no log line about the keyring"
    [[ ! -e $key_file ]] || fail "the cache key did not move into B's keyring"
    if waits_for_keyring; then fail "status still says it waits"; fi
    # The further root mounts by itself when the wait ends (D110 amended).
    wait_for "the further root" 30 root_mounted
    ls "$ROOT_MNT" >/dev/null || fail "the further root does not answer"
    daemon_says 'the further roots mount now' || fail "no log line about the further roots"
    wait_queue_empty
    # Read in full first: `grep -q` stops at the first match, and the writer's
    # broken pipe then fails the pipeline (pipefail).
    changes=$("$PDMOUNT" fake changes "$FAKE_DIR")
    grep -q ' during$' <<<"$changes" || fail "the file written while waiting is not on the drive"
    expect_eq "content read after the unlock" "$(cat "$area/before")" before
    # The disk cache is open again: its blocks are read, and a read fills it.
    cat "$big_file" >/dev/null
    blocks=$(status_text | sed -n 's/^Cache: .* in \([0-9]*\) blocks$/\1/p')
    ((blocks > 0)) || fail "the disk cache stays empty after the unlock: $(status_text)"
    # One per wait: the first daemon's, the restarted one's, the one after the gone bus, and the
    # one started after an attach without a daemon.
    expect_eq "keyring notifications" "$(keyring_notices)" 4

    # The stub prompter (python3 with dbus and gi) stands in for an unlock dialog nobody answers.
    if python3 -I -c "import dbus, gi" 2>/dev/null; then
        step "a service start with B's collection locked: ready at once, no unlock dialog"
        kill -TERM "$daemon"
        wait "$daemon" || fail "the daemon exited with an error"
        daemon=
        : >"$work/prompter.log"
        DBUS_SESSION_BUS_ADDRESS=$bus_b python3 -I "$repo/scripts/repro/stub-prompter.py" "$work/prompter.log" &
        pids+=($!)
        timeout 10 gdbus wait --address "$bus_b" org.gnome.keyring.SystemPrompter || fail "the stub prompter did not start"
        gdbus call --address "$bus_b" --dest org.freedesktop.secrets --object-path /org/freedesktop/secrets \
            --method org.freedesktop.Secret.Service.Lock "['/org/freedesktop/secrets/collection/login']" >/dev/null ||
            fail "cannot lock B's login collection"
        service_start() { # a daemon as the service runs it, on bus B as its own
            PATH=$stub_path DBUS_SESSION_BUS_ADDRESS=$bus_b PDMOUNT_KEYRING_RETRY_SECS=2 PDMOUNT_KEYRING_TIMEOUT_SECS=60 \
                "$PDMOUNT" --profile "$PROFILE" -v mount --foreground --service openrc "${backend_flag[@]}" "$MNT" \
                >>"$work/daemon.log" 2>&1 &
            daemon=$!
        }
        started=$SECONDS
        service_start
        wait_for "the mount" 30 mount_answers
        wait_for "the waiting state" 20 waits_for_keyring
        ((SECONDS - started < 15)) || fail "the start took $((SECONDS - started)) s"
        # The attach from B, saved, names the daemon's own bus now.
        [[ ! -e $attach_file ]] || fail "a saved attach of the daemon's own bus was kept"
        daemon_says 'saved by the last attach is not used; removed the file' || fail "no log line about the removal"
        status_text | grep -q 'locked' || fail "the reason does not name the lock: $(status_text)"
        sleep 5
        if grep -q BeginPrompting "$work/prompter.log"; then fail "the start or the timer opened an unlock dialog"; fi

        step "a stop while a login waits for the unlock dialog ends at once"
        DBUS_SESSION_BUS_ADDRESS=$bus_b "$PDMOUNT" --profile "$PROFILE" login >"$work/login.log" 2>&1 &
        login_pid=$!
        wait_for "the unlock dialog of the login" 20 grep -q BeginPrompting "$work/prompter.log"
        # The client goes first: the daemon's attempt goes on without it.
        kill "$login_pid" 2>/dev/null || true
        wait "$login_pid" 2>/dev/null || true
        stopping=$SECONDS
        kill -TERM "$daemon"
        wait "$daemon" || fail "the daemon exited with an error"
        daemon=
        ((SECONDS - stopping < 15)) || fail "the stop took $((SECONDS - stopping)) s while a keyring call ran"
        if is_mounted; then fail "still mounted"; fi

        step "the collection unlocks: the timer finds it and the mount goes online"
        prompts=$(grep -c BeginPrompting "$work/prompter.log")
        service_start
        wait_for "the mount" 30 mount_answers
        wait_for "the waiting state" 20 waits_for_keyring
        stop_gk
        start_gk
        wait_for "online" 60 online
        expect_eq "content read after the unlock" "$(cat "$area/before")" before
        expect_eq "unlock dialogs" "$(grep -c BeginPrompting "$work/prompter.log")" "$prompts"
    else
        echo "x13: no python3 dbus/gi for the stub prompter; the locked-collection part is skipped"
    fi

    # The layout of a Gentoo system with i3 (D102 amended): PAM's
    # gnome-keyring on the service's bus A, the notification server on the
    # desktop's bus C, where D-Bus would start a gnome-keyring that never
    # answers (it waits for the one on A).
    step "C, a desktop bus with a notification server and a Secret Service that never answers"
    kill -TERM "$daemon"
    wait "$daemon" || fail "the daemon exited with an error"
    daemon=
    stop_gk
    activatable "$work/busC-services" org.freedesktop.Notifications "$(command -v false)"
    activatable "$work/busC-services" org.freedesktop.secrets "$(command -v sleep) 20"
    start_bus busC "$work/busC-services"
    bus_c=$bus_address
    cp -p "$work/cache.key.saved" "$key_file"
    # A new login: no attach saved by the daemon before (the restart is tested above).
    start_fresh_on_a() {
        rm -f "$attach_file"
        start_on_a
    }
    attach_c() { # the keyring has no owner yet: the activatable one on C; notifications on C
        local before
        before=$(notified)
        out=$(attach_from "$bus_c") || fail "attach of C failed: $out"
        grep -q '(keyring: this session.s bus; notifications: this session.s bus)' <<<"$out" || fail "attach of C: $out"
        wait_for "the notification on C" 20 more_notified "$before"
        expect_eq "the bus of the notification" "$(tail -1 "$work/notify-bus.log")" "$bus_c"
        status_text | grep -q "^Bus: .*keyring: the desktop's bus, notifications: the desktop's bus" ||
            fail "status: $(status_text)"
    }
    keyring_on_a() { daemon_says 'the keyring: using the daemon.s own session bus'; }

    step "a daemon on A, attached from C: when gnome-keyring starts on A, the cache key's write goes there"
    start_fresh_on_a
    attach_c
    start_gk "$bus_a" gk-a
    wait_for "online" 60 online
    keyring_on_a || fail "no log line about the keyring on A"
    [[ ! -e $key_file ]] || fail "the cache key did not move into A's keyring"
    status_text | grep -q "^Bus: .*keyring: the daemon's own bus, notifications: the desktop's bus" ||
        fail "status: $(status_text)"
    expect_eq "content read after the unlock" "$(cat "$area/before")" before

    step "an attach from C with the keyring on A: A starts programs on C's display"
    out=$(attach_from "$bus_c" -u WAYLAND_DISPLAY DISPLAY=:42 XAUTHORITY="$work/xauthority") ||
        fail "attach of C failed: $out"
    grep -q 'on this display (DISPLAY, XAUTHORITY)' <<<"$out" || fail "attach with a display: $out"
    daemon_says "on the desktop.s display from now on: DISPLAY=:42 XAUTHORITY=" || fail "no log line about the display"
    gdbus call --address "$bus_a" --dest org.freedesktop.DBus --object-path /org/freedesktop/DBus \
        --method org.freedesktop.DBus.StartServiceByName org.pdmount.X13.DisplayProbe 0 >/dev/null 2>&1 || true
    wait_for "the probe on A" 10 test -s "$work/probe-display"
    expect_eq "the display of a program A starts" "$(cat "$work/probe-display")" ":42 $work/xauthority"
    # A restart passes the saved display to A again (D102 amended); the keyring on A answers.
    uses=$(grep -c 'using the desktop.s session bus saved by the last attach' "$work/daemon.log")
    stop_daemon
    launch_on_a
    wait_for "online" 60 online
    expect_eq "uses of the saved bus" \
        "$(grep -c 'using the desktop.s session bus saved by the last attach' "$work/daemon.log")" "$((uses + 1))"
    expect_eq "displays passed to A" \
        "$(grep -c "on the desktop.s display from now on: DISPLAY=:42 XAUTHORITY=" "$work/daemon.log")" 2
    # Without a display, nothing is passed.
    out=$(attach_from "$bus_c" -u DISPLAY -u XAUTHORITY -u WAYLAND_DISPLAY) || fail "attach of C failed: $out"
    if grep -q 'on this display' <<<"$out"; then fail "a display was passed without one: $out"; fi

    step "again with A's keyring stopped: after the attach from C the cache key is read from A"
    kill -TERM "$daemon"
    wait "$daemon" || fail "the daemon exited with an error"
    daemon=
    stop_gk
    : >"$work/daemon.log"
    start_fresh_on_a
    attach_c
    start_gk "$bus_a" gk-a
    wait_for "online" 60 online
    keyring_on_a || fail "no log line about the keyring on A"
    [[ ! -e $key_file ]] || fail "a cache key file appeared"
    expect_eq "content read with the key from A" "$(cat "$area/before")" before
    # One notification per wait, both on C.
    expect_eq "notifications on C" "$(grep -c "^$bus_c\$" "$work/notify-bus.log")" 2
else
    echo "x13: no gnome-keyring-daemon; the keyring part is skipped"
fi

step "stop: the daemon exits cleanly"
kill -TERM "$daemon"
wait "$daemon" || fail "the daemon exited with an error"
daemon=
if is_mounted; then fail "still mounted"; fi
echo "x13: PASS"
