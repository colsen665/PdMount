#!/usr/bin/env bash
# The test of scripts/repro/m6-desktop.sh (M6.5): its steps without a desktop. A private
# session bus with the notification stub and a stub file manager plays the desktop's bus,
# a second private bus (the script's own) the bus without a notification service; HOME and
# the XDG directories are in the work directory, every question is answered "y", and the
# stub's log is checked where the user would look at the screen. Show is invoked through
# the stub after the script's `conflict`. Fake only, live mode only.
source "$(dirname "$0")/lib.sh"
[[ $BACKEND == fake ]] || { echo "m6desktop tests a fake-drive script; skipped for BACKEND=$BACKEND"; exit 0; }
need_dbus_daemon m6desktop
x_live_only
make_workdir

notify_log=$work/notify.log
fm_log=$work/fm.log
start_session_bus m6desktop
start_notify_stub "$bus_address" "$notify_log"
start_file_manager_stub "$bus_address" "$fm_log"

home=$work/home
mkdir -p "$home"
mkdir -m 0700 "$work/run"
script=$repo/scripts/repro/m6-desktop.sh
m6() { # <step>
    env HOME="$home" XDG_CONFIG_HOME="$home/.config" XDG_DATA_HOME="$home/.local/share" \
        XDG_STATE_HOME="$home/.local/state" XDG_CACHE_HOME="$home/.cache" XDG_RUNTIME_DIR="$work/run" \
        XDG_CURRENT_DESKTOP=stub DBUS_SESSION_BUS_ADDRESS="$bus_address" \
        PATH="$(dirname "$PDMOUNT"):$repo/tests/scenarios/stubs:$PATH" \
        PDMOUNT_TEST_XDG_OPEN_LOG="$work/xdg-open.log" M6_ANSWER=y M6_SHOW_WAIT=1 \
        "$script" "$1" </dev/null
}
mnt=$home/PdMount-m6
results=$home/.local/state/pdmount-m6/results.txt
m6_cleanup() {
    if mountpoint -q "$mnt"; then
        "$PDMOUNT" --profile m6 --force-file-secrets unmount --force "$mnt" >/dev/null 2>&1 || true
    fi
    [[ -f $home/.local/state/pdmount-m6/bus.pid ]] && kill "$(cat "$home/.local/state/pdmount-m6/bus.pid")" 2>/dev/null
    scenario_cleanup
}
trap m6_cleanup EXIT

# The notifications of the stub's log as `<summary>|<body>` lines (newlines as `\n`).
seen() {
    notify_lines "$notify_log" | python3 -I -c '
import json, sys
for line in sys.stdin:
    n = json.loads(line)
    print(n["summary"] + "|" + n["body"].replace("\n", "\\n"))'
}
count() { notify_count "$notify_log"; }
edit_text='Changed here and on another device; both copies kept.'

step "setup: a fake drive, the mount on the (stub) desktop's bus, doctor"
m6 setup
mountpoint -q "$mnt" || fail "not mounted after setup"
[[ -f $mnt/m6/clip.mp4 && -f $mnt/m6/readme.txt ]] || fail "the seeded files are missing: $(ls "$mnt/m6")"
grep -q 'notifications' "$results" || fail "no notifications line from doctor in the results"
expect_eq "notifications after setup" "$(count)" 0

step "conflict: one edit_edit, named, with Show; Show reaches the file manager"
m6 conflict
expect_eq "notifications" "$(count)" 1
[[ $(seen | tail -1) =~ ^PdMount:\ conflict\|/m6/conflict-[0-9]{6}\.txt\\n"$edit_text"$ ]] ||
    fail "the conflict notification: $(seen | tail -1)"
id=$(notify_lines "$notify_log" | tail -1 | python3 -I -c 'import json, sys; print(json.loads(sys.stdin.read())["id"])')
"$PDMOUNT" fake notifications invoke --bus "$bus_address" --id "$id" --action show
deadline=$((SECONDS + 15))
until grep -q '"op":"show_items"' "$fm_log"; do
    ((SECONDS < deadline)) || fail "no ShowItems: $(cat "$fm_log")"
    sleep 0.2
done
grep -q 'Q: Did it appear like that' "$results" || fail "the conflict question is not in the results"

step "missed: none while the daemon has no service; two at its next start, one after session attach"
m6 missed
expect_eq "notifications after missed" "$(count)" 4
seen | tail -3 | grep -c '^PdMount: conflict|/m6/missed-[abc]-' | grep -qx 3 || fail "the missed ones: $(seen | tail -3)"
grep -q 'Q: Did both appear' "$results" || fail "the missed questions are not in the results"
[[ ! -e $home/.local/state/pdmount-m6/bus.pid ]] || fail "the private bus was left running"

step "slow: one downloading notification, closed"
m6 slow
expect_eq "notifications after slow" "$(count)" 5
expect_eq "the download notification" "$(seen | tail -1)" \
    'PdMount: downloading|clip.mp4 (64 MiB) is downloading; it opens as its blocks arrive.'
id=$(notify_lines "$notify_log" | tail -1 | python3 -I -c 'import json, sys; print(json.loads(sys.stdin.read())["id"])')
grep -q "\"op\":\"close\".*\"id\":$id\b" "$notify_log" || grep -q "\"id\":$id,.*\"op\":\"close\"" "$notify_log" ||
    fail "the download notification was not closed: $(grep '"op":"close"' "$notify_log")"

step "settings: conflicts off (none), conflict_names off (no name), downloads off (none)"
m6 settings
expect_eq "notifications after settings" "$(count)" 6
expect_eq "the unnamed conflict" "$(seen | tail -1)" "PdMount: conflict|$edit_text"
mountpoint -q "$mnt" || fail "not mounted after settings"
grep -qx 'mountpoint = "~/PdMount-m6"' "$home/.config/pdmount/m6.toml" || fail "the config was not restored"
grep -q notify "$home/.config/pdmount/m6.toml" && fail "the config still has [notify]"

step "report and cleanup"
m6 report | grep -q '===== settings: notify.downloads = false' || fail "the report lacks the settings"
m6 cleanup
mountpoint -q "$mnt" && fail "still mounted after cleanup"
[[ ! -e $home/.config/pdmount/m6.toml && ! -e $home/.local/share/pdmount-m6-fake ]] ||
    fail "cleanup left the config or the fake drive"

echo "m6desktop: PASS"
