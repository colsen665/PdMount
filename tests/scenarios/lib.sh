# shellcheck shell=bash
# Shared setup for the scenario scripts (DESIGN.md §15.3). Source it.
#
# A scenario calls prepare_mount, makes its changes, calls verify_cycle, then
# checks the result. verify_cycle does what VERIFY_MODE asks for between the
# two:
#   live     nothing: verify on the same mount (default)
#   remount  unmount (draining the queue), mount
#   crash    kill -9 the daemon (with the queue non-empty when the scenario
#            wrote), unmount --force, mount, wait until the queue is empty
#   fresh    wait until the queue is empty, unmount, `reset --keep-login`,
#            mount: everything is read from the backend again
#   second   a second profile (device B) is mounted too, before the scenario's
#            changes. wait until A's queue is empty, then until a marker file
#            written on A shows on B (within the visibility SLO, D037), then
#            wait for B's queue and switch PROFILE, MNT, TEST_ROOT and REMOTE to
#            B: the checks after verify_cycle read what B received through its
#            event stream
#
# Environment:
#   BACKEND      proton (default): the real account; fake: a persisted fake drive
#                (needs a build with `--features fake-backend`)
#   VERIFY_MODE  live, remount, crash, fresh or second (see above)
#   PDMOUNT      pdmount binary (default: target/release/pdmount, else target/debug/pdmount;
#                with BACKEND=fake: target/debug/pdmount)
#   PROFILE      profile (default: default; with BACKEND=fake: fake-scenarios)
#   SECRETS      force (default): --force-file-secrets; allow: --allow-file-secrets; keyring: no flag
#                (D029; only keyring tests use the keyring)
#   SCENARIO_ROOT  work directories and, with BACKEND=fake, everything else below (default:
#                /dev/shm/pdmount-tests if /dev/shm is writable, else ~/.cache/pdmount-scenarios).
#                With BACKEND=fake, XDG_{CONFIG,DATA,CACHE,STATE}_HOME and XDG_RUNTIME_DIR point into it
#                as well, so the test profile's store, staging, cache, log and socket stay in tmpfs
#   CACHE_MAX    with BACKEND=fake: the test profile's `[cache] max_bytes` (default: 1GiB)
#   MNT          mountpoint (default: ~/ProtonDrive; with BACKEND=fake: $SCENARIO_ROOT/fake-mnt)
#   PROFILE2, MNT2  device B, for the `second` mode and the M scenarios (real account: profile
#                `second`, ~/ProtonDrive-second; fake: fake-scenarios-2, $SCENARIO_ROOT/fake-mnt-2).
#                The fake profiles are named dev-a and dev-b (`[device] name`)
#   FAKE_DIR     the fake drive (default: $SCENARIO_ROOT/fake-drive); seeded with the dataset on
#                first use. PDMOUNT_FAKE_KEEP_REVISIONS defaults to 4 here
#   DATASET      local copy of the M1 dataset, regenerated if missing (default:
#                ~/.cache/pdmount-scenarios/m1; with BACKEND=fake: $SCENARIO_ROOT/m1-nobig, without the
#                2 GiB file; see scripts/testdata/make-m1-dataset.sh)
#   SCENARIO_SYSTEM_BUS  with BACKEND=fake: DBUS_SYSTEM_BUS_ADDRESS for the daemons (default: a
#                socket that does not exist, so they take no delay lock from the machine's logind)
#   TARGET       my-files (default) or share (BACKEND=fake only, M5.14a, D114): the scenario
#                writes into a share's tree instead of My Files. A fake drive of its own
#                ($SCENARIO_ROOT/fake-drive-share) holds the share `pdmount-share` of another
#                user with the drive's user as an editor; the profiles fake-share and
#                fake-share-2 mount it in a Shared root at $SCENARIO_ROOT/fake-shared(-2), and
#                TEST_ROOT, ROOT_A and ROOT_B point into it (<share>/pdmount-test). Path commands
#                (`access`, `conflicts`, ...) reach My Files only (D110), so the A scenarios
#                skip, and the M scenarios read their conflicts from the daemon's log
set -euo pipefail

repo=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)

# A directory only this user can enter: created 0700, refused when it is a
# symlink or another user's, since another user can create any name in
# /dev/shm first (S-8). Also used for the soak and bench roots.
private_root() { # <dir>
    # Only the root itself must be private; parents keep their modes.
    # shellcheck disable=SC2174
    mkdir -p -m 0700 "$1"
    if [[ -L $1 || ! -O $1 ]]; then
        echo "refusing $1: it is a symlink or belongs to another user" >&2
        exit 1
    fi
    chmod 0700 "$1"
}

# Removes a fake drive, and nothing else: an empty directory or one with the
# fake's own layout (snapshot.json, or a .seeded* marker). FAKE_DIR may be
# set to any directory (S-7).
remove_fake_dir() { # <dir>
    local dir=$1
    [[ -e $dir || -L $dir ]] || return 0
    if [[ -L $dir || ! -d $dir ]]; then
        echo "refusing to remove FAKE_DIR=$dir: not a directory" >&2
        exit 1
    fi
    if [[ -n $(ls -A "$dir") && ! -f $dir/snapshot.json ]] && ! compgen -G "$dir/.seeded*" >/dev/null; then
        echo "refusing to remove FAKE_DIR=$dir: not empty and not a fake drive (no snapshot.json or .seeded* marker)" >&2
        exit 1
    fi
    rm -rf "$dir"
}

BACKEND=${BACKEND:-proton}
VERIFY_MODE=${VERIFY_MODE:-live}
# Test data lives in tmpfs when there is one, to spare the disks.
if [[ -z "${SCENARIO_ROOT:-}" ]]; then
    if [[ -d /dev/shm && -w /dev/shm ]]; then
        SCENARIO_ROOT=/dev/shm/pdmount-tests
    else
        SCENARIO_ROOT=$HOME/.cache/pdmount-scenarios
    fi
fi
private_root "$SCENARIO_ROOT"
# TARGET=share (fake only) runs a scenario in a share's tree instead of My
# Files: profiles, mountpoints and a fake drive of its own (see the end).
TARGET=${TARGET:-my-files}
case $TARGET in
    my-files) ;;
    share)
        [[ $BACKEND == fake ]] || { echo "TARGET=share needs BACKEND=fake" >&2; exit 2; }
        PROFILE=${PROFILE:-fake-share}
        PROFILE2=${PROFILE2:-fake-share-2}
        MNT=${MNT:-$SCENARIO_ROOT/fake-mnt-share}
        MNT2=${MNT2:-$SCENARIO_ROOT/fake-mnt-share-2}
        FAKE_DIR=${FAKE_DIR:-$SCENARIO_ROOT/fake-drive-share}
        ;;
    *) echo "TARGET must be my-files or share" >&2; exit 2 ;;
esac
case $BACKEND in
    proton)
        PROFILE=${PROFILE:-default}
        MNT=${MNT:-$HOME/ProtonDrive}
        PROFILE2=${PROFILE2:-second}
        MNT2=${MNT2:-$HOME/ProtonDrive-second}
        # The local copy of the uploaded dataset (2.3 GiB) stays on disk; it is only read.
        DATASET=${DATASET:-$HOME/.cache/pdmount-scenarios/m1}
        dataset_flags=()
        backend_flag=()
        ;;
    fake)
        PROFILE=${PROFILE:-fake-scenarios}
        MNT=${MNT:-$SCENARIO_ROOT/fake-mnt}
        FAKE_DIR=${FAKE_DIR:-$SCENARIO_ROOT/fake-drive}
        DATASET=${DATASET:-$SCENARIO_ROOT/m1-nobig}
        dataset_flags=(--no-big)
        backend_flag=(--backend "fake:$FAKE_DIR")
        PDMOUNT=${PDMOUNT:-$repo/target/debug/pdmount}
        # The test profile's state, staging, cache, log and socket go to SCENARIO_ROOT too. A
        # runtime directory, as desktop sessions have: the sandbox user has none.
        export XDG_CONFIG_HOME=$SCENARIO_ROOT/xdg/config XDG_DATA_HOME=$SCENARIO_ROOT/xdg/data \
            XDG_CACHE_HOME=$SCENARIO_ROOT/xdg/cache XDG_STATE_HOME=$SCENARIO_ROOT/xdg/state \
            XDG_RUNTIME_DIR=$SCENARIO_ROOT/xdg/run
        PROFILE2=${PROFILE2:-fake-scenarios-2}
        MNT2=${MNT2:-$SCENARIO_ROOT/fake-mnt-2}
        # One config per profile; an older one gets the device name added. The mountpoint is set
        # so that commands without one (`reset`, `status`) look at the test's, not at
        # ~/ProtonDrive, where a real drive may be mounted.
        fake_config() { # <profile> <device name> <mountpoint>
            local config=$XDG_CONFIG_HOME/pdmount/$1.toml
            if [[ ! -f "$config" ]]; then
                mkdir -p "${config%/*}"
                printf '[cache]\nmax_bytes = "%s"\nram_bytes = "32MiB"\n' "${CACHE_MAX:-1GiB}" >"$config"
            fi
            grep -q '^\[device\]' "$config" || printf '\n[device]\nname = "%s"\n' "$2" >>"$config"
            # The bulk-read guard spaces scan downloads 100 ms apart (D111); on the fake, which
            # has no network latency, that spacing alone would make cold reads of many files
            # several times slower. The guard stays on, with a short spacing (SCAN_INTERVAL).
            grep -q '^\[scan\]' "$config" ||
                printf '\n[scan]\ninterval = "%s"\n' "${SCAN_INTERVAL:-10ms}" >>"$config"
            # The first one is My Files'; those of [[roots]] (TARGET=share) stay.
            if grep -q '^mountpoint *=' "$config"; then
                sed -i "0,/^mountpoint *=/{s|^mountpoint *=.*|mountpoint = \"$3\"|}" "$config"
            else
                sed -i "1i mountpoint = \"$3\"" "$config"
            fi
        }
        fake_config "$PROFILE" dev-a "$MNT"
        fake_config "$PROFILE2" dev-b "$MNT2"
        # The fake keeps a few revisions per file (it lives in the daemon).
        export PDMOUNT_FAKE_KEEP_REVISIONS=${PDMOUNT_FAKE_KEEP_REVISIONS:-4}
        # Test daemons take no delay lock from the machine's logind (D091): the
        # system bus is one that does not exist, unless a scenario starts its own (x11).
        export DBUS_SYSTEM_BUS_ADDRESS=${SCENARIO_SYSTEM_BUS:-unix:path=$SCENARIO_ROOT/no-system-bus}
        ;;
    *) echo "BACKEND must be proton or fake" >&2; exit 2 ;;
esac
SECRETS=${SECRETS:-force}
case $SECRETS in
    force) secrets_flag=(--force-file-secrets) ;;
    allow) secrets_flag=(--allow-file-secrets) ;;
    keyring) secrets_flag=() ;;
    *) echo "SECRETS must be force, allow or keyring" >&2; exit 2 ;;
esac
if [[ -z "${PDMOUNT:-}" ]]; then
    PDMOUNT=$repo/target/release/pdmount
    [[ -x "$PDMOUNT" ]] || PDMOUNT=$repo/target/debug/pdmount
fi
REMOTE=$MNT/pdmount-m1
LOCAL=$DATASET/pdmount-m1

pd() { "$PDMOUNT" --profile "$PROFILE" "${secrets_flag[@]}" "$@"; }
step() { printf '\n==> %s\n' "$*"; }
fail() { echo "FAIL: $*" >&2; exit 1; }

pd2() { "$PDMOUNT" --profile "$PROFILE2" "${secrets_flag[@]}" "$@"; }

# `status` exits 1 without an active session (always so for the fake).
status_text() { pd status 2>/dev/null || true; }
is_mounted() { status_text | grep -q '^Mount: *mounted'; }
mount_now() { pd mount "${backend_flag[@]}" "$MNT"; }
is_mounted_second() { pd2 status 2>/dev/null | grep -q '^Mount: *mounted'; }
mount_second() { pd2 mount "${backend_flag[@]}" "$MNT2"; }

# The SDK client UID of a device (the fake's `--client`): `pdmount-<id>` from
# the `Device:` line of its status. Argument: 1 (default) or 2.
device_uid() { # [1|2]
    local text
    if [[ ${1:-1} == 2 ]]; then text=$(pd2 status 2>/dev/null || true); else text=$(status_text); fi
    sed -n 's/^Device: .*(\([0-9a-f-]*\))$/pdmount-\1/p' <<<"$text"
}
daemon_pid() { status_text | sed -n 's/.*daemon pid \([0-9]*\).*/\1/p'; }

# kill -9 the daemon and wait until the process is gone.
kill_daemon() {
    local pid
    pid=$(daemon_pid)
    [[ -n "$pid" ]] || fail "no daemon pid in pdmount status"
    kill -9 "$pid"
    while kill -0 "$pid" 2>/dev/null; do sleep 0.1; done
}

# Only the generator's own output, $DATASET/pdmount-m1, is ever removed:
# DATASET may be set to any directory (S-7).
ensure_dataset() {
    if [[ ! -f "$LOCAL/MANIFEST.sha256" ]]; then
        if [[ -d $DATASET && ! -e $LOCAL && -n $(ls -A "$DATASET") ]]; then
            fail "DATASET=$DATASET is not empty and holds no pdmount-m1; set DATASET to a new or empty directory"
        fi
        step "generating the local dataset in $DATASET"
        rm -rf "$LOCAL"
        mkdir -p "$DATASET"
        "$repo/scripts/testdata/make-m1-dataset.sh" "${dataset_flags[@]}" "$DATASET"
    fi
}

# The fake drive starts as a copy of the dataset. Reseeding it makes the
# profile's stored state stale, so that is reset too.
ensure_fake_drive() {
    [[ $BACKEND == fake ]] || return 0
    "$PDMOUNT" fake --help >/dev/null 2>&1 ||
        fail "$PDMOUNT has no fake backend; build it with \`cargo build --features fake-backend\`"
    if [[ ! -f "$FAKE_DIR/.seeded-m1" ]]; then
        step "seeding the fake drive $FAKE_DIR from $DATASET"
        if is_mounted; then pd unmount "$MNT"; fi
        if is_mounted_second; then pd2 unmount "$MNT2"; fi
        ensure_dataset
        remove_fake_dir "$FAKE_DIR"
        "$PDMOUNT" fake seed --from "$DATASET" "$FAKE_DIR"
        touch "$FAKE_DIR/.seeded-m1"
        pd reset --keep-login >/dev/null
        pd2 reset --keep-login >/dev/null
    fi
}

# Mounts if needed. Scenarios that read the M1 dataset call require_m1 after it.
prepare_mount() {
    case "$VERIFY_MODE" in
        live | remount | crash | fresh | second) ;;
        *) fail "unknown VERIFY_MODE=$VERIFY_MODE (live, remount, crash, fresh, second)" ;;
    esac
    ensure_fake_drive
    is_mounted || mount_now
    # B is mounted before the scenario's changes, so they reach it as events.
    if [[ $VERIFY_MODE == second ]]; then
        is_mounted_second || mount_second
        # B must have applied the events of the scenarios before, or the
        # visibility check below would time their backlog (the fake's trash
        # purges, for one, come in pages of 50).
        wait_queue_empty
        mkdir -p "$ROOT_A"
        ls "$ROOT_B" >/dev/null 2>&1 || true
        send_marker "$ROOT_A" "$ROOT_B" barrier "${BARRIER_TIMEOUT:-900}"
        # And A must have applied what B did meanwhile: checks on B can write
        # (`git status` refreshes the index), and A deleting such a file would
        # revive it (M4, `local_delete_remote_edit`).
        wait_queue_empty2
        send_marker "$ROOT_B" "$ROOT_A" barrier "${BARRIER_TIMEOUT:-900}"
        wait_queue_empty
        # Changes from here on are what B has to receive (see second_marker).
        second_mark=$(fake_change_mark)
    fi
}

# Waits until the daemon's event stream has caught up with everything that
# happened before (BACKEND=fake): a file made by another client in the test
# root shows on the mount, and events arrive in order. Without it a scenario
# that times remote changes starts behind the events of the scenarios before.
event_barrier() {
    local name=.barrier-$RANDOM$RANDOM
    rm -f "$TEST_ROOT"/.barrier-* 2>/dev/null || true
    printf barrier >"$work/$name"
    "$PDMOUNT" fake create --path "/pdmount-test/$name" --from "$work/$name" "$FAKE_DIR"
    local start=$SECONDS
    until [[ -e "$TEST_ROOT/$name" ]]; do
        ((SECONDS - start < ${BARRIER_TIMEOUT:-600})) || fail "the event stream did not catch up"
        sleep 1
    done
    echo "events caught up after $((SECONDS - start)) s"
}

# Both devices, for the M scenarios (live mode only).
prepare_two() {
    # Two live mounts are the whole point; the other modes would only restart them.
    [[ $VERIFY_MODE == live ]] || { echo "$(basename "$0" .sh): two devices, live mode only; skipped in $VERIFY_MODE mode"; exit 0; }
    ensure_fake_drive
    is_mounted || mount_now
    is_mounted_second || mount_second
}

require_m1() {
    [[ -d "$REMOTE" ]] || fail "$REMOTE not found; upload pdmount-m1 to the root of My Files first"
}

# Waits until no upload or namespace change is pending (`pdmount status`
# reports `Queue: 0 pending`, and `0 pending` on each tree line of a further
# root). Daemons without a queue line pass at once.
wait_queue_of() { # <status function>
    local deadline=$((SECONDS + ${QUEUE_TIMEOUT:-600})) line text trees
    while :; do
        text=$("$1")
        line=$(grep '^Queue:' <<<"$text" || true)
        trees=$(sed -nE 's/^  .*: (writable|read-only)[^,]*, ([0-9]+) pending, .*/\2/p' <<<"$text" |
            awk '{ sum += $1 } END { print sum + 0 }')
        [[ $trees == 0 ]] || line="$line; further trees: $trees pending"
        [[ -z "$line" || "$line" =~ ^Queue:\ +0\ pending[^\;]*$ ]] && return 0
        ((SECONDS < deadline)) || fail "the queue did not drain within ${QUEUE_TIMEOUT:-600} s: $line"
        sleep 1
    done
}
status_text2() { pd2 status 2>/dev/null || true; }
wait_queue_empty() { wait_queue_of status_text; }
wait_queue_empty2() { wait_queue_of status_text2; }

verify_cycle() {
    case "$VERIFY_MODE" in
        live) ;;
        remount)
            step "remount: unmount (draining the queue) and mount"
            pd unmount --wait "$MNT"
            mount_now
            ;;
        crash)
            step "crash: kill -9 the daemon, unmount --force, mount, drain"
            kill_daemon
            pd unmount --force "$MNT"
            mount_now
            wait_queue_empty
            ;;
        fresh)
            step "fresh: drain, unmount, reset local state, mount"
            wait_queue_empty
            pd unmount --wait "$MNT"
            pd reset --keep-login
            mount_now
            ;;
        second)
            step "second: wait for device B, then check on B"
            wait_queue_empty
            second_marker
            ;;
    esac
}

# A marker written in <from> shows in <to> within poll_interval + 10 s of its
# commit (D037): the clock starts when the writer's queue is empty, since the
# upload debounce is not part of the bound. The event stream delivers what came
# before it, so a device that sees the marker has applied the changes made
# earlier.
send_marker() { # <from test root> <to test root> <tag> [limit in seconds]
    local name start limit=${4:-${SECOND_SLO:-40}}
    name=.marker-$3-$RANDOM$RANDOM
    rm -f "$1"/.marker-* 2>/dev/null || true
    mkdir -p "$1"
    printf marker >"$1/$name"
    if [[ $1 == "$MNT2"/* ]]; then wait_queue_empty2; else wait_queue_empty; fi
    start=$SECONDS
    until [[ -e "$2/$name" ]]; do
        ((SECONDS - start < limit)) ||
            fail "the marker $3 is not visible after $limit s (visibility SLO, D037)"
        sleep 1
    done
    echo "marker $3 visible after $((SECONDS - start)) s"
}

# The limit is the SLO of one event page (poll_interval + 10 s = 40 s). B
# applies a page of 50 events in about 12 s (listings and lookups are paced,
# D027), so each further page of the scenario's changes (fake only; the real
# account counts none) adds 15 s.
second_marker() {
    local events pages=1
    wait_queue_empty
    # A's access rules are local state; lift them so the next run can clean up.
    release_access_rules
    if [[ $BACKEND == fake ]]; then
        events=$(fake_changes "${second_mark:-0}" | wc -l)
        pages=$(((events + 49) / 50))
        ((pages >= 1)) || pages=1
    fi
    send_marker "$TEST_ROOT" "$MNT2/pdmount-test" "$(basename "$0" .sh)" \
        $((${SECOND_SLO:-40} + 15 * (pages - 1)))
    PROFILE=$PROFILE2
    MNT=$MNT2
    TEST_ROOT=$MNT2/pdmount-test
    REMOTE=$MNT2/pdmount-m1
    wait_queue_empty
}

# Removes the access rules the scenarios set, those under pdmount-test/
# (`pdmount access list`: MODE PATH SET-AS SOURCE STATUS). Rules elsewhere
# belong to the user (D028).
release_access_rules() {
    local path
    while read -r path; do
        [[ -n "$path" ]] && { pd access unset "$MNT$path" >/dev/null 2>&1 || true; }
    done < <(pd access list 2>/dev/null |
        awk '($1 == "ro" || $1 == "rw") && ($2 == "/pdmount-test" || index($2, "/pdmount-test/") == 1) { print $2 }')
}

check_hot_path() {
    local calls
    calls=$(pd stats | awk '$1 == "hot_path_network_calls" { print $2 }')
    echo "hot_path_network_calls = $calls"
    [[ "$calls" == "0" ]] || fail "hot_path_network_calls is $calls, expected 0"
}

# Sets $work to a scratch directory removed on exit. Not for use in $(...):
# the trap must be set in the script's own shell.
make_workdir() {
    work=$(mktemp -d "$SCENARIO_ROOT/work.XXXXXX")
    trap scenario_cleanup EXIT
}

# At exit (make_workdir): lifts a fault the scenario injected, so a failed
# scenario does not leave a device offline for the ones after it, and
# removes the work directory.
scenario_cleanup() {
    if [[ -n ${fault_active:-} ]]; then fake_fault clear >/dev/null 2>&1 || true; fi
    local pid
    for pid in "${scenario_pids[@]}"; do
        kill "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
    done
    rm -rf "${work:-}"
}

# ---- Private session buses and the notification stub (M6.1, D138) ----
#
# Notifications go over D-Bus (notify.rs), so a scenario that checks them runs
# a private session bus with `pdmount fake notifications serve` on it, and
# starts the daemons with DBUS_SESSION_BUS_ADDRESS set to it. The log has one
# JSON line per call (summary, body, actions, replaces_id, id; the bus too).
# Both are stopped by scenario_cleanup, after the scenario's own unmounts.
scenario_pids=()

# Skips the scenario (exit 0) when dbus-daemon is not installed.
need_dbus_daemon() { # <scenario name>
    command -v dbus-daemon >/dev/null || { echo "$1: no dbus-daemon; skipped"; exit 0; }
}

# A private session bus: anyone may own and call anything, as on a session
# bus. Sets bus_address. <service dir>: where it finds activation files.
# Needs $work (make_workdir); run it in the scenario's own shell, not in $(...).
start_session_bus() { # <name> [<service dir>]
    local socket=$work/$1.sock conf=$work/$1.conf
    {
        cat <<EOF
<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:path=$socket</listen>
  <auth>EXTERNAL</auth>
EOF
        [[ -z ${2:-} ]] || echo "  <servicedir>$2</servicedir>"
        cat <<EOF
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
EOF
    } >"$conf"
    dbus-daemon --config-file="$conf" --nofork --nopidfile &
    local pid=$!
    scenario_pids+=("$pid")
    until [[ -S $socket ]]; do
        kill -0 "$pid" 2>/dev/null || fail "dbus-daemon did not start"
        sleep 0.1
    done
    bus_address=unix:path=$socket
}

# The notification stub on <bus address>, logging to <log> (appended). Waits
# until it owns its name. Sets notify_pid. Options: --no-actions,
# --no-markup, --delay-ms <n>.
start_notify_stub() { # <bus address> <log> [stub options...]
    local before
    : >>"$2"
    before=$(grep -c '"op":"ready"' "$2" || true)
    "$PDMOUNT" fake notifications serve --bus "$1" --log "$2" "${@:3}" &
    notify_pid=$!
    scenario_pids+=("$notify_pid")
    local start=$SECONDS
    until (($(grep -c '"op":"ready"' "$2" || true) > before)); do
        kill -0 "$notify_pid" 2>/dev/null || fail "the notification stub did not start"
        ((SECONDS - start < 20)) || fail "the notification stub is not ready"
        sleep 0.1
    done
}

# Stops the stub that start_notify_stub started last: the bus then has no
# notification server (a call to its name fails), until the next start.
stop_notify_stub() {
    kill "$notify_pid" 2>/dev/null || true
    wait "$notify_pid" 2>/dev/null || true
}

# A stub file manager (`pdmount fake notifications file-manager`, M6.2) on
# <bus>: owns org.freedesktop.FileManager1 and logs each ShowItems as a JSON
# line (`"op":"show_items"`, `uris`). Sets fm_pid; stop_file_manager_stub
# takes the name off the bus again.
start_file_manager_stub() { # <bus address> <log>
    local before
    : >>"$2"
    before=$(grep -c '"op":"fm_ready"' "$2" || true)
    "$PDMOUNT" fake notifications file-manager --bus "$1" --log "$2" &
    fm_pid=$!
    scenario_pids+=("$fm_pid")
    local start=$SECONDS
    until (($(grep -c '"op":"fm_ready"' "$2" || true) > before)); do
        kill -0 "$fm_pid" 2>/dev/null || fail "the file manager stub did not start"
        ((SECONDS - start < 20)) || fail "the file manager stub is not ready"
        sleep 0.1
    done
}
stop_file_manager_stub() {
    kill "$fm_pid" 2>/dev/null || true
    wait "$fm_pid" 2>/dev/null || true
}

# What a stub logged: the notifications (one JSON line each), their number.
notify_lines() { grep '"op":"notify"' "$1" || true; } # <log>
notify_count() { grep -c '"op":"notify"' "$1" || true; } # <log>

# ---- M2 write scenarios ----

# Writes happen only below pdmount-test/ (D028).
TEST_ROOT=$MNT/pdmount-test

# Sets $area to an empty scenario directory on the mount.
scenario_area() { # <name>
    area=$TEST_ROOT/$1
    mkdir -p "$TEST_ROOT"
    rm -rf "$area"
    mkdir -p "$area"
    # Only changes from here on count for remote_temp_names.
    fake_mark=$(fake_change_mark)
}

# A deterministic tree: nested directories, small files with modes
# 0644/0755/0600 and fixed mtimes, and one larger file.
make_tree() { # <dir> <small files> <big MiB>
    python3 -I - "$1" "$2" "$3" <<'PY'
import os, random, sys
root, files, big = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
rng = random.Random(20261020)
dirs = [""]
for i in range(max(1, files // 15)):
    parent = rng.choice(dirs)
    if parent.count("/") >= 3:
        parent = ""
    d = os.path.join(parent, f"dir-{i:03d}")
    dirs.append(d)
    os.makedirs(os.path.join(root, d), exist_ok=True)
for i in range(files):
    path = os.path.join(root, rng.choice(dirs), f"f-{i:04d}.dat")
    with open(path, "wb") as f:
        f.write(rng.randbytes(rng.choice([0, 1, 100, 4096, 4097, rng.randrange(0, 70000)])))
    os.chmod(path, rng.choice([0o644, 0o755, 0o600]))
    os.utime(path, (1700000000 + i * 37, 1700000000 + i * 37))
if big:
    with open(os.path.join(root, "big.bin"), "wb") as f:
        for _ in range(big):
            f.write(rng.randbytes(1 << 20))
    os.utime(os.path.join(root, "big.bin"), (1700000123, 1700000123))
PY
}

# Directory mtimes are local-only (D013): fresh mounts show creation times.
dir_time_flags() {
    if [[ $VERIFY_MODE == fresh || $VERIFY_MODE == second ]]; then echo --omit-dir-times; fi
}

# Fails unless rsync finds nothing to do, by mtime and size and by checksum.
assert_same_tree() { # <src> <dst>
    local flags out
    flags=$(dir_time_flags)
    # shellcheck disable=SC2086
    out=$(rsync -ani $flags "$1/" "$2/")
    [[ -z "$out" ]] || fail "rsync -ani $flags $1/ $2/ would change: $out"
    # shellcheck disable=SC2086
    out=$(rsync -ani --checksum $flags "$1/" "$2/")
    [[ -z "$out" ]] || fail "rsync -ani --checksum $1/ $2/ would change: $out"
}

# The fake's change log (BACKEND=fake only): `<number> <kind> <link> <name>`,
# only changes numbered above the first argument (default 0).
fake_changes() { # [after]
    [[ $BACKEND == fake ]] || return 0
    "$PDMOUNT" fake changes --after "${1:-0}" "$FAKE_DIR"
}

# The number of the latest change, to pass to fake_changes later.
fake_change_mark() {
    local last
    last=$(fake_changes | tail -n 1)
    echo "${last%% *}" | grep -E '^[0-9]+$' || echo 0
}

# Names created remotely that look like rsync or git temporaries and no
# longer exist on the mount: those must never have been uploaded (§7.3). A
# temporary that still exists (left by a process killed with the daemon) is
# a real file.
remote_temp_names() {
    local name
    fake_changes "${fake_mark:-0}" |
        awk '$2 == "created-file" || $2 == "renamed" { print $4 }' |
        grep -E '^\..+\.[A-Za-z0-9]{6}$|\.lock$|^tmp_obj_|^tmp_pack_' | sort -u |
        while read -r name; do
            [[ -n "$(find "$TEST_ROOT" -name "$name" -print -quit)" ]] || echo "$name"
        done || true
}

git_env() {
    export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1
    # No detached background maintenance: runs stay deterministic.
    export GIT_CONFIG_COUNT=2 GIT_CONFIG_KEY_0=gc.auto GIT_CONFIG_VALUE_0=0 \
        GIT_CONFIG_KEY_1=maintenance.auto GIT_CONFIG_VALUE_1=false
    export GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@t GIT_COMMITTER_NAME=t GIT_COMMITTER_EMAIL=t@t
    export GIT_AUTHOR_DATE='2026-10-20T00:00:00Z' GIT_COMMITTER_DATE='2026-10-20T00:00:00Z'
}

# ---- M scenarios: two devices (live mode, DESIGN.md §15.3) ----

# Device A is $MNT, device B is $MNT2.
ROOT_A=$MNT/pdmount-test
ROOT_B=$MNT2/pdmount-test

# Waits until both queues are empty and a marker has gone each way, so that
# every change made so far has reached both devices.
sync_devices() {
    wait_queue_empty
    wait_queue_empty2
    send_marker "$ROOT_A" "$ROOT_B" a2b
    wait_queue_empty2
    send_marker "$ROOT_B" "$ROOT_A" b2a
    wait_queue_empty
}

wait_visible() { # <path> [seconds]
    local limit=${2:-90} start=$SECONDS
    until [[ -e "$1" ]]; do
        ((SECONDS - start < limit)) || fail "$1 is not visible after $limit s"
        sleep 1
    done
}

# Waits until a file reads as the given content.
wait_content() { # <path> <content> [seconds]
    local limit=${3:-90} start=$SECONDS
    until [[ "$(cat "$1" 2>/dev/null)" == "$2" ]]; do
        ((SECONDS - start < limit)) || fail "$1 does not read '$2' after $limit s (reads '$(cat "$1" 2>&1)')"
        sleep 1
    done
}

expect_eq() { # <what> <got> <want>
    [[ "$2" == "$3" ]] || fail "$1: got '$2', expected '$3'"
}

# Opens <file> for writing (created or truncated), writes <content> and keeps
# it open until release_hold <name>. The write stays local while it is open.
declare -A hold_pid
hold_open() { # <name> <file> <content>
    local ready=$work/$1.ready
    python3 -I - "$2" "$3" "$ready" "$work/$1.release" <<'PY' &
import os, sys, time
path, content, ready, release = sys.argv[1:5]
fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o644)
os.write(fd, content.encode())
open(ready, "w").close()
# Ends when released, or when the scenario is gone (its work directory too).
deadline = time.time() + 900
while not os.path.exists(release) and os.path.exists(ready) and time.time() < deadline:
    time.sleep(0.1)
os.close(fd)
PY
    hold_pid[$1]=$!
    local start=$SECONDS
    until [[ -e "$ready" ]]; do
        ((SECONDS - start < 20)) || fail "could not open $2 for writing"
        sleep 0.1
    done
}

release_hold() { # <name>
    touch "$work/$1.release"
    wait "${hold_pid[$1]}" || fail "holding $1 failed"
}

# A changed <dir on A> while B holds a file open (hold_open b): waits until B
# has applied the change, then closes B's file and syncs both devices. The
# marker goes into the same directory, so the listing on B that shows the
# marker shows A's change too, and B notices it while its file is open.
reach_b_while_held() { # <dir on A> <dir on B> <tag>
    send_marker "$1" "$2" "$3"
    release_hold b
    sync_devices
}

# The kinds of the open conflicts of device 1 (A) or 2 (B) at or below a
# path, one per line. Conflicts stay open across scenarios, so each scenario
# looks at its own directory.
conflict_kinds() { # <1|2> <path>
    local text
    if [[ $1 == 2 ]]; then text=$(pd2 conflicts list "$2"); else text=$(pd conflicts list "$2"); fi
    awk 'NR > 1 && $1 ~ /^[0-9]+$/ { print $3 }' <<<"$text"
}

expect_conflicts() { # <1|2> <path> <kinds...> (none = no conflicts)
    local device=$1 path=$2 got want
    shift 2
    got=$(conflict_kinds "$device" "$path" | sort | paste -sd' ')
    want=$(printf '%s\n' "$@" | sort | paste -sd' ')
    expect_eq "open conflicts of device $device below $path" "$got" "$want"
}

# Entries of <dir> whose names look like conflict copies.
conflict_copies() { # <dir>
    find "$1" -maxdepth 1 -name '*(conflict *' -printf '%f\n' | sort
}

fake_fault() {
    "$PDMOUNT" fake fault "$@" "$FAKE_DIR"
    if [[ $1 == clear ]]; then fault_active=; else fault_active=1; fi
}

# Sets uid_a to device A's client UID (the fake's `--client`).
need_uid_a() {
    uid_a=$(device_uid 1)
    [[ -n "$uid_a" ]] || fail "no device id for A in pdmount status"
}

# Device A cannot reach the fake drive until a_back; extra arguments go to
# `fake fault offline` (e.g. `--for 90`).
a_offline() {
    need_uid_a
    fake_fault offline --client "$uid_a" "$@"
}

# B's changes are uploaded, A is back online and has drained its queue, and
# both devices have seen each other's changes.
a_back() {
    wait_queue_empty2
    fake_fault clear
    echo "A is back; waiting for its queue"
    wait_queue_empty
    sync_devices
}

# ---- X scenarios: the flush at a stop (M5, DESIGN.md §15.3, D075, D091) ----

# They stop the daemon themselves, so other verify modes add nothing.
x_live_only() {
    [[ $VERIFY_MODE == live ]] || { echo "$(basename "$0" .sh): live mode only; skipped in $VERIFY_MODE mode"; exit 0; }
}

# The test profile's config: x_config_save keeps it, x_config sets it to the
# kept one plus the given TOML lines, x_restore puts it back and clears the
# network profile (from an EXIT trap).
x_config_file() { echo "$XDG_CONFIG_HOME/pdmount/$PROFILE.toml"; }
x_config_save() { x_saved_config=$(cat "$(x_config_file)"); }
x_config() { # <toml line...>
    { printf '%s\n' "$x_saved_config"; printf '%s\n' "$@"; } >"$(x_config_file)"
}
x_restore() {
    "$PDMOUNT" fake profile clear "$FAKE_DIR" >/dev/null 2>&1 || true
    if [[ -n ${x_saved_config:-} ]]; then printf '%s\n' "$x_saved_config" >"$(x_config_file)"; fi
}

now_ms() { date +%s%3N; }

# The daemon's log, from the last log_mark on.
daemon_log() { echo "$XDG_STATE_HOME/pdmount/$PROFILE.log"; }
log_mark() { x_log_offset=$(stat -c %s "$(daemon_log)" 2>/dev/null || echo 0); }
log_since_mark() { tail -c +"$((${x_log_offset:-0} + 1))" "$(daemon_log)"; }
expect_log() { # <extended regex> <what>
    log_since_mark | grep -Eq "$1" || fail "$2: no log line matching '$1'; log: $(log_since_mark | tail -20)"
}

# Writes <n> files of <KiB> KiB to <dir> and keeps copies in $work/<name>.
write_files() { # <dir> <name> <n> <KiB>
    mkdir -p "$1" "$work/$2"
    local i
    for ((i = 0; i < $3; i++)); do
        head -c "$(($4 * 1024))" /dev/urandom >"$work/$2/f-$i"
        cp "$work/$2/f-$i" "$1/f-$i"
    done
}

# SIGTERM to the daemon; waits for it to exit and sets stop_ms to how long
# that took.
term_daemon() {
    local pid start
    pid=$(daemon_pid)
    [[ -n "$pid" ]] || fail "no daemon pid in pdmount status"
    start=$(now_ms)
    kill -TERM "$pid"
    while kill -0 "$pid" 2>/dev/null; do sleep 0.05; done
    stop_ms=$(($(now_ms) - start))
    echo "the daemon stopped $stop_ms ms after SIGTERM"
}

# Fails unless the fake drive holds what was written to <dir> (copies in
# $work/<name>): `reset --keep-login` refuses while anything is left to
# upload, and the fresh mount reads everything from the drive.
expect_on_drive() { # <dir> <name>
    pd reset --keep-login >/dev/null || fail "changes were left for the next mount"
    mount_now
    diff -r "$work/$2" "$1" >/dev/null || fail "$1 differs from what was written"
}

# ---- TARGET=share: the scenarios in a share's tree (M5.14a, D114) ----

if [[ $TARGET == share ]]; then
    SHARE_NAME=pdmount-share
    SHARE_MNT=${SHARE_MNT:-$SCENARIO_ROOT/fake-shared}
    SHARE_MNT2=${SHARE_MNT2:-$SCENARIO_ROOT/fake-shared-2}
    # Each profile mounts the Shared root beside My Files.
    share_root_config() { # <profile> <mountpoint>
        local config=$XDG_CONFIG_HOME/pdmount/$1.toml
        grep -q '^kind = "shared"' "$config" ||
            printf '\n[[roots]]\nkind = "shared"\nmountpoint = "%s"\n' "$2" >>"$config"
    }
    share_root_config "$PROFILE" "$SHARE_MNT"
    share_root_config "$PROFILE2" "$SHARE_MNT2"
    TEST_ROOT=$SHARE_MNT/$SHARE_NAME/pdmount-test
    ROOT_A=$TEST_ROOT
    ROOT_B=$SHARE_MNT2/$SHARE_NAME/pdmount-test
    REMOTE=$SHARE_MNT/$SHARE_NAME/pdmount-m1

    # A drive of its own: an empty My Files with pdmount-test/, and the
    # share of another user with the drive's user as an editor.
    ensure_fake_drive() {
        "$PDMOUNT" fake --help >/dev/null 2>&1 ||
            fail "$PDMOUNT has no fake backend; build it with \`cargo build --features fake-backend\`"
        [[ -f "$FAKE_DIR/.seeded-share" ]] && return 0
        step "seeding the fake drive $FAKE_DIR with the editor share $SHARE_NAME"
        if is_mounted; then pd unmount "$MNT"; fi
        if is_mounted_second; then pd2 unmount "$MNT2"; fi
        remove_fake_dir "$FAKE_DIR"
        local seed
        seed=$(mktemp -d "$SCENARIO_ROOT/share-seed.XXXXXX")
        mkdir -p "$seed/my-files/pdmount-test" "$seed/share/pdmount-test"
        "$PDMOUNT" fake seed --from "$seed/my-files" "$FAKE_DIR"
        "$PDMOUNT" fake root --name "$SHARE_NAME" --owner owner@example.test --role editor \
            --from "$seed/share" "$FAKE_DIR" >/dev/null
        rm -rf "$seed"
        touch "$FAKE_DIR/.seeded-share"
        pd reset --keep-login >/dev/null
        pd2 reset --keep-login >/dev/null
    }

    # The Shared root mounts in the background after the daemon is ready.
    wait_share() { # <Shared mountpoint>
        local start=$SECONDS
        until [[ -d $1/$SHARE_NAME/pdmount-test ]]; do
            ((SECONDS - start < 60)) || fail "the share did not show in $1 within 60 s"
            sleep 0.2
        done
    }
    mount_now() { pd mount "${backend_flag[@]}" "$MNT" && wait_share "$SHARE_MNT"; }
    mount_second() { pd2 mount "${backend_flag[@]}" "$MNT2" && wait_share "$SHARE_MNT2"; }

    # The daemon's stop drains a further tree only until its grace ends, so
    # the queue is drained first, as `unmount --wait` does for My Files. A
    # crash leaves the Shared root's mount behind as well.
    verify_cycle() {
        case "$VERIFY_MODE" in
            live) ;;
            remount)
                step "remount: drain, unmount and mount"
                wait_queue_empty
                pd unmount --wait "$MNT"
                mount_now
                ;;
            crash)
                step "crash: kill -9 the daemon, unmount --force both mounts, mount, drain"
                kill_daemon
                pd unmount --force "$MNT"
                pd unmount --force "$SHARE_MNT"
                mount_now
                wait_queue_empty
                ;;
            fresh)
                step "fresh: drain, unmount, reset local state, mount"
                wait_queue_empty
                pd unmount --wait "$MNT"
                pd reset --keep-login
                mount_now
                ;;
            second) fail "TARGET=share has no second mode" ;;
        esac
    }

    prepare_two() {
        [[ $VERIFY_MODE == live ]] || { echo "$(basename "$0" .sh): two devices, live mode only; skipped in $VERIFY_MODE mode"; exit 0; }
        ensure_fake_drive
        is_mounted || mount_now
        is_mounted_second || mount_second
        share_log_a=$(stat -c %s "$XDG_STATE_HOME/pdmount/$PROFILE.log" 2>/dev/null || echo 0)
        share_log_b=$(stat -c %s "$XDG_STATE_HOME/pdmount/$PROFILE2.log" 2>/dev/null || echo 0)
    }

    # `conflicts list` reaches My Files only (D110): the kinds of conflict a
    # further tree recorded since prepare_two, from the daemon's log, at or
    # below a path of the share, each once (the log has every record, the
    # registry's later updates of one conflict too, not only open ones).
    conflict_kinds() { # <1|2> <path>
        local log offset rel
        if [[ $1 == 2 ]]; then
            log=$XDG_STATE_HOME/pdmount/$PROFILE2.log offset=${share_log_b:-0}
            rel=${2#"$SHARE_MNT2/$SHARE_NAME"}
        else
            log=$XDG_STATE_HOME/pdmount/$PROFILE.log offset=${share_log_a:-0}
            rel=${2#"$SHARE_MNT/$SHARE_NAME"}
        fi
        tail -c +"$((offset + 1))" "$log" | grep 'conflict recorded in a further tree' |
            sed -nE 's/.*kind="?([a-z_]+)"? path="?([^" ]*)"?.*/\1 \2/p' |
            awk -v rel="$rel" '$2 == rel || index($2, rel "/") == 1 { print $1 }' | sort -u
    }
fi
