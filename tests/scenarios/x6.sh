#!/usr/bin/env bash
# X6 (DESIGN.md §15.3, §10.8, D077, M6.2): conflict notifications, on a
# private session bus with the notification stub and a stub file manager
# (lib.sh). One device; the other device is `pdmount fake write|create|trash`
# while the file is held open here, so the upload after the close conflicts.
#   1. Open conflicts are named and shown once: an edit_edit, and a
#      create_create whose name has `<b>&` (escaped in the body), each with
#      a Show action; a remote_delete_local_edit (a notice) is not shown.
#   2. Show, invoked through the stub: the stub file manager gets ShowItems
#      with the conflict copy's file:// URI; without a file manager, xdg-open
#      (a stub in PATH) opens the copy's folder with the notification's bus.
#   3. Summing: after a restart, five conflicts at once give three
#      notifications of their own and "2 more conflicts" once 30 s have
#      passed since the first.
#   4. notify.conflict_names = false: the body names no file; then
#      notify.conflicts = false: no notification at all.
#   5. (M6.3) Conflicts missed while no notification service ran: made with
#      the stub stopped, they are shown once it starts (one resolved before
#      is not, nor a conflict made with notify.conflicts = false), and not
#      again after a restart; more than three are shown in one summary.
#   6. The desktop's bus attached (`session attach` from a second bus with
#      a stub): a conflict's notification and its Show (xdg-open) go there;
#      the bus then goes away with that notification open: the daemon goes
#      on, the next conflict is shown on its own bus.
#   7. A further tree (a `kind = "test"` root, `fake root`): its conflict's
#      body starts with the tree's name, and Show gives the file manager its
#      copy: at the root's mountpoint while the tree is the root's only one,
#      in the tree's folder once the root has two.
# Fake, live mode only.
source "$(dirname "$0")/lib.sh"
[[ $BACKEND == fake ]] || { echo "x6 needs the fake to play the other device; skipped for BACKEND=$BACKEND"; exit 0; }
need_dbus_daemon x6
x_live_only
prepare_mount
make_workdir
scenario_area x6
remote=/pdmount-test/x6
# Step 7's further root.
team_mnt=$SCENARIO_ROOT/fake-root-x6
x_config_save
x6_cleanup() {
    x_restore
    # The next scenario's daemon must not keep this one's bus or stubs.
    if is_mounted; then pd unmount "$MNT" >/dev/null 2>&1 || true; fi
    if awk -v mnt="$team_mnt" '$2 == mnt { found = 1 } END { exit !found }' /proc/self/mounts; then
        fusermount3 -u "$team_mnt" 2>/dev/null || fusermount3 -uz "$team_mnt" || true
    fi
    scenario_cleanup
}
trap x6_cleanup EXIT

# The bus with the stubs, and an xdg-open (stubs/, since $work may be on a
# noexec tmpfs) that logs its argument and bus.
notify_log=$work/notify.log
fm_log=$work/fm.log
opened=$work/xdg-open.log
start_session_bus x6
start_notify_stub "$bus_address" "$notify_log"
start_file_manager_stub "$bus_address" "$fm_log"
export DBUS_SESSION_BUS_ADDRESS=$bus_address PATH=$repo/tests/scenarios/stubs:$PATH
export PDMOUNT_TEST_XDG_OPEN_LOG=$opened

notifications() { notify_count "$notify_log"; }
wait_notifications() { # <n> <seconds>
    local deadline=$((SECONDS + $2))
    until (($(notifications) >= $1)); do
        ((SECONDS < deadline)) || fail "expected $1 notifications within $2 s, got: $(cat "$notify_log")"
        sleep 0.2
    done
}
# A field of the <n>th notification (1 = first), as text.
field() { # <n> <field>
    notify_lines "$notify_log" | sed -n "$1p" | python3 -I -c '
import json, sys
value = json.loads(sys.stdin.read())[sys.argv[1]]
print(value if isinstance(value, str) else json.dumps(value))' "$2"
}
expect_notification() { # <n> <summary> <body> <actions as JSON>
    expect_eq "summary of notification $1" "$(field "$1" summary)" "$2"
    expect_eq "body of notification $1" "$(field "$1" body)" "$3"
    expect_eq "actions of notification $1" "$(field "$1" actions)" "$4"
}
wait_log() { # <extended regex> <count> <seconds>
    local deadline=$((SECONDS + $3))
    until (($(log_since_mark | grep -Ec "$1") >= $2)); do
        ((SECONDS < deadline)) || fail "expected $2 log lines matching '$1' within $3 s; log: $(log_since_mark | tail -20)"
        sleep 0.2
    done
}
restart() {
    pd unmount --wait "$MNT"
    mount_now
}
printf there >"$work/there"
# Another device's revision of <name> while it is held open here with new
# content; the upload after release_hold <key> conflicts (edit_edit).
edit_conflict() { # <key> <name>
    printf base >"$area/$2"
    wait_queue_empty
    hold_open "$1" "$area/$2" here
    "$PDMOUNT" fake write --path "$remote/$2" --from "$work/there" "$FAKE_DIR"
}
# <name> made here (held open) and on another device (create_create).
create_conflict() { # <key> <name>
    hold_open "$1" "$area/$2" here
    "$PDMOUNT" fake create --path "$remote/$2" --from "$work/there" "$FAKE_DIR"
}
edit_text='Changed here and on another device; both copies kept.'
create_text='Created here and on another device; both copies kept.'
show='["show", "Show"]'

step "the daemon on the private bus"
restart

step "1. an edit_edit is named and shown once, with Show"
log_mark
edit_conflict e e.txt
release_hold e
wait_notifications 1 30
expect_notification 1 "PdMount: conflict" "$remote/e.txt
$edit_text" "$show"

step "1. a create_create of a name with <b>&: escaped"
create_conflict a 'a <b>&.txt'
release_hold a
wait_notifications 2 30
expect_notification 2 "PdMount: conflict" "$remote/a &lt;b&gt;&amp;.txt
$create_text" "$show"

step "1. a remote_delete_local_edit (notice): logged, not shown"
printf base >"$area/n.txt"
wait_queue_empty
hold_open n "$area/n.txt" here
"$PDMOUNT" fake trash --path "$remote/n.txt" "$FAKE_DIR"
release_hold n
wait_log 'conflict recorded.*kind="?remote_delete_local_edit' 1 30
wait_queue_empty
sleep 2
expect_eq "notifications after the notice" "$(notifications)" 2
pd conflicts list --all "$area" | grep -q remote_delete_local_edit || fail "no notice in conflicts list: $(pd conflicts list --all "$area")"

step "2. Show: the file manager gets the copy's URI"
"$PDMOUNT" fake notifications invoke --bus "$bus_address" --id "$(field 1 id)" --action show
deadline=$((SECONDS + 15))
until grep -q '"op":"show_items"' "$fm_log"; do
    ((SECONDS < deadline)) || fail "no ShowItems: $(cat "$fm_log"); log: $(log_since_mark | tail -5)"
    sleep 0.2
done
shown=$(grep '"op":"show_items"' "$fm_log" | head -1 | python3 -I -c '
import json, sys, urllib.parse
uris = json.loads(sys.stdin.read())["uris"]
assert len(uris) == 1 and uris[0].startswith("file://"), uris
print(urllib.parse.unquote(uris[0][len("file://"):]))')
[[ $(dirname "$shown") == "$area" ]] || fail "ShowItems names $shown, not a file in $area"
[[ $(basename "$shown") == "e (conflict "*").txt" && -f $shown ]] || fail "ShowItems names $shown, not e.txt's conflict copy"
expect_eq "the conflict copy" "$(cat "$shown")" here
[[ ! -e $opened ]] || fail "xdg-open ran although the file manager answered: $(cat "$opened")"

step "2. Show without a file manager: xdg-open on the folder, with the bus"
stop_file_manager_stub
"$PDMOUNT" fake notifications invoke --bus "$bus_address" --id "$(field 2 id)" --action show
deadline=$((SECONDS + 15))
until [[ -s $opened ]]; do
    ((SECONDS < deadline)) || fail "xdg-open did not run; log: $(log_since_mark | tail -5)"
    sleep 0.2
done
expect_eq "xdg-open's folder and bus" "$(cat "$opened")" "$area $bus_address"
expect_eq "ShowItems calls" "$(grep -c '"op":"show_items"' "$fm_log")" 1

step "3. five at once after a restart: three of their own, then '2 more conflicts'"
restart
log_mark
for n in 1 2 3 4 5; do create_conflict "c$n" "c$n.txt"; done
start=$SECONDS
for n in 1 2 3 4 5; do release_hold "c$n"; done
wait_notifications 5 30
first=$SECONDS
sleep 3
expect_eq "notifications within the window" "$(notifications)" 5
for n in 3 4 5; do
    expect_eq "summary of notification $n" "$(field "$n" summary)" "PdMount: conflict"
    [[ $(field "$n" body) =~ ^$remote/c[1-5]\.txt$'\n'"$create_text"$ ]] || fail "body of notification $n: $(field "$n" body)"
done
wait_notifications 6 40
((SECONDS - first >= 20)) || fail "the sum came $((SECONDS - first)) s after the third, not at the window's end"
expect_notification 6 "PdMount: conflicts" '2 more conflicts; see `pdmount conflicts list`' '[]'
wait_log 'conflict recorded.*kind="?create_create' 5 5
echo "summing took $((SECONDS - start)) s"

step "4. notify.conflict_names = false: no file in the body"
x_config '[notify]' 'conflict_names = false'
restart
edit_conflict f f.txt
release_hold f
wait_notifications 7 30
expect_notification 7 "PdMount: conflict" "$edit_text" "$show"

step "4. notify.conflicts = false: no notification"
x_config '[notify]' 'conflicts = false'
restart
log_mark
edit_conflict g g.txt
release_hold g
wait_log 'conflict recorded.*kind="?edit_edit' 1 30
sleep 3
expect_eq "notifications with conflicts = false" "$(notifications)" 7
x_config

# ---- 5. (M6.3) Missed conflicts shown when the service appears ----
# The daemon's bus is up all along; the service on it is not. The last step of
# 4 left a conflict (g.txt) made with notify.conflicts = false: it counts as
# shown, so none of this shows it.
step "5. no notification service: conflicts made now are not shown, nor marked"
stop_notify_stub
x_config
restart
log_mark
shown=$(notifications)
edit_conflict h h.txt
release_hold h
edit_conflict i i.txt
release_hold i
edit_conflict j j.txt
release_hold j
wait_log 'conflict recorded.*kind="?edit_edit' 3 30
wait_queue_empty
pd conflicts resolve keep-both "$area/j.txt" >/dev/null
sleep 3
expect_eq "notifications with no service" "$(notifications)" "$shown"

step "5. the service starts: h and i are shown, j (resolved) and g (made while off) are not"
start_notify_stub "$bus_address" "$notify_log"
wait_notifications $((shown + 2)) 40
sleep 3
expect_eq "notifications once the service ran" "$(notifications)" $((shown + 2))
for pair in "1 h" "2 i"; do
    set -- $pair
    expect_notification $((shown + $1)) "PdMount: conflict" "$remote/$2.txt
$edit_text" "$show"
done

step "5. a restart of the daemon does not show them again"
restart
sleep 4
expect_eq "notifications after the restart" "$(notifications)" $((shown + 2))

step "5. four missed together are shown in one summary"
stop_notify_stub
shown=$(notifications)
log_mark
for n in 1 2 3 4; do
    edit_conflict "k$n" "k$n.txt"
    release_hold "k$n"
done
wait_log 'conflict recorded.*kind="?edit_edit' 4 40
wait_queue_empty
sleep 2
expect_eq "notifications with no service" "$(notifications)" "$shown"
start_notify_stub "$bus_address" "$notify_log"
wait_notifications $((shown + 1)) 40
sleep 3
expect_eq "notifications once the service ran" "$(notifications)" $((shown + 1))
expect_notification $((shown + 1)) "PdMount: conflicts" '4 conflicts were recorded while no desktop was listening; see `pdmount conflicts list`' '[]'

# ---- 6. The attached desktop bus goes away while a notification is open ----
step "6. attached: a conflict's notification goes to the desktop's bus, its Show too"
own_bus=$bus_address own_stub=$notify_pid
desk_log=$work/notify-desk.log
start_session_bus x6-desk
desk_bus=$bus_address desk_bus_pid=${scenario_pids[-1]} bus_address=$own_bus
start_notify_stub "$desk_bus" "$desk_log"
desk_stub=$notify_pid notify_pid=$own_stub
DBUS_SESSION_BUS_ADDRESS=$desk_bus "$PDMOUNT" --profile "$PROFILE" session attach >/dev/null
pid=$(daemon_pid)
shown=$(notifications)
log_mark
edit_conflict m m.txt
release_hold m
deadline=$((SECONDS + 30))
until (($(notify_count "$desk_log") >= 1)); do
    ((SECONDS < deadline)) || fail "no notification on the desktop's bus; log: $(log_since_mark | tail -5)"
    sleep 0.2
done
desk_field() { notify_lines "$desk_log" | sed -n "1p" | python3 -I -c 'import json, sys; print(json.loads(sys.stdin.read())[sys.argv[1]])' "$1"; }
expect_eq "body on the desktop's bus" "$(desk_field body)" "$remote/m.txt
$edit_text"
expect_eq "notifications on the daemon's bus" "$(notifications)" "$shown"
# No file manager on the desktop's bus: xdg-open, with that bus.
"$PDMOUNT" fake notifications invoke --bus "$desk_bus" --id "$(desk_field id)" --action show
deadline=$((SECONDS + 15))
until grep -qF " $desk_bus" "$opened"; do
    ((SECONDS < deadline)) || fail "xdg-open did not run with the desktop's bus: $(cat "$opened")"
    sleep 0.2
done
expect_eq "xdg-open's folder and bus" "$(tail -1 "$opened")" "$area $desk_bus"

step "6. the desktop's bus goes away: the daemon goes on, the next conflict goes to its own bus"
kill "$desk_stub" "$desk_bus_pid" 2>/dev/null || true
wait "$desk_stub" "$desk_bus_pid" 2>/dev/null || true
edit_conflict o o.txt
release_hold o
wait_notifications $((shown + 1)) 40
expect_notification $((shown + 1)) "PdMount: conflict" "$remote/o.txt
$edit_text" "$show"
expect_eq "the daemon's pid" "$(daemon_pid)" "$pid"
expect_eq "notifications on the gone bus" "$(notify_count "$desk_log")" 1
expect_log 'desktop.s session bus is gone' "the drop of the gone bus"

# ---- 7. A further tree's conflict ----
# A `test` root with one tree shows it at the mountpoint itself; with two,
# each in a folder of the top level. The other device is `fake create` in
# the tree's root folder.
read -r team_volume team_root <<<"$("$PDMOUNT" fake root --name x6-team "$FAKE_DIR")"
read -r _ other_root <<<"$("$PDMOUNT" fake root --name x6-other "$FAKE_DIR")"
[[ -n $team_root && -n $other_root ]] || fail "fake root: no root"
tree_lines=('' '[[roots.tree]]' 'name = "team"' "volume = \"$team_volume\"" "root = \"$team_root\"" 'writable = true')
other_lines=('' '[[roots.tree]]' 'name = "other"' "volume = \"$team_volume\"" "root = \"$other_root\"")
root_lines=('[[roots]]' 'kind = "test"' "mountpoint = \"$team_mnt\"")
mount_team() {
    restart
    local deadline=$((SECONDS + 30))
    until status_text | grep -q '^  team: writable'; do
        ((SECONDS < deadline)) || fail "the further tree did not open: $(status_text)"
        sleep 0.2
    done
}
# A create_create of <name> in the tree mounted at <local dir>; its
# notification names the tree, and Show gives the file manager the copy.
team_conflict() { # <name> <local dir>
    local shown copy
    shown=$(notifications)
    hold_open "$1" "$2/$1.txt" here
    "$PDMOUNT" fake create --root "$team_root" --volume "$team_volume" --owner fake@example.test \
        --path "/$1.txt" --from "$work/there" "$FAKE_DIR"
    release_hold "$1"
    wait_notifications $((shown + 1)) 40
    expect_notification $((shown + 1)) "PdMount: conflict" "team: /$1.txt
$create_text" "$show"
    local calls
    calls=$(grep -c '"op":"show_items"' "$fm_log")
    "$PDMOUNT" fake notifications invoke --bus "$bus_address" --id "$(field $((shown + 1)) id)" --action show
    local deadline=$((SECONDS + 15))
    until (($(grep -c '"op":"show_items"' "$fm_log") > calls)); do
        ((SECONDS < deadline)) || fail "no ShowItems for the further tree: $(cat "$fm_log")"
        sleep 0.2
    done
    copy=$(grep '"op":"show_items"' "$fm_log" | tail -1 | python3 -I -c '
import json, sys, urllib.parse
uris = json.loads(sys.stdin.read())["uris"]
assert len(uris) == 1 and uris[0].startswith("file://"), uris
print(urllib.parse.unquote(uris[0][len("file://"):]))')
    [[ $(dirname "$copy") == "$2" ]] || fail "ShowItems names $copy, not a file in $2"
    [[ $(basename "$copy") == "$1 (conflict "*").txt" && -f $copy ]] || fail "ShowItems names $copy, not $1.txt's conflict copy"
    expect_eq "the further tree's conflict copy" "$(cat "$copy")" here
}
start_file_manager_stub "$bus_address" "$fm_log"

step "7. a further root with one tree: the body names it, Show reaches its copy at the mountpoint"
x_config "${root_lines[@]}" "${tree_lines[@]}"
mount_team
team_conflict t1 "$team_mnt"

step "7. the same root with two trees: Show reaches the copy in the tree's folder"
x_config "${root_lines[@]}" "${tree_lines[@]}" "${other_lines[@]}"
mount_team
team_conflict t2 "$team_mnt/team"

echo "X6: PASS"
