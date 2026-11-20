#!/usr/bin/env bash
# X16 (DESIGN.md §14.5, D080, D114): writes into shares where the role
# allows, on a fake drive of its own. Shares of other users: `Edit` (alice)
# and `Other` (bob) as an editor, `View` (alice) as a viewer, `Gone` (carol)
# as an editor until it is revoked. Device A writes; device B, a second
# daemon of the same member, reads the shares.
#   1. Gate: an editor writes and the changes reach the drive, a viewer gets
#      EROFS, a rename between two shares is EXDEV (mv copies).
#   2. Losing the right with changes pending: a change the server refuses
#      closes the gate and holds the queue (status says why, a notification);
#      a role change to viewer read from the listing, while a file is still
#      open, holds its change too, and the close does not lift it; the file
#      stays readable. Editor again: the held changes upload. A revocation
#      with changes queued (new files, a folder, an edit of a file read
#      before) moves them to orphaned/ with a notification and an
#      `Orphaned:` line in status.
#   2a. Rules (D124): `Gone`'s owner put `*` in the share's `/.pdignore`; it
#      keeps nothing of the member's local-only (a new file uploads). A file
#      the profile's own `.pdignore` keeps local-only is exported to
#      orphaned/ at the revocation with the queued changes. An uploaded file
#      moved into a folder the profile's rules keep local-only keeps its
#      remote place, and its edit stays local (§7.7); the revocation exports
#      it as `not-uploaded` (M6.0.3 V-1).
#   3. Marker (D076): A's Shared root has `pending_marker`; A runs out of
#      grace with changes in `Edit` and leaves a marker there that names no
#      device; B shows `Incoming:` under the share; A's My Files, with its
#      own marker on, gets none. A's next mount uploads the rest and trashes
#      it, and B's `Incoming:` goes.
#   1a. Planted markers (D126): 40 markers of other members in `Other`
#      (editor) give 16 read, one notification that counts the rest, and
#      `Incoming:` capped at 5 lines and `+35 more`; the 40 in `View`
#      (viewer) are not read: no line, no notification.
# Fake, live mode only.
TARGET=my-files
source "$(dirname "$0")/lib.sh"
[[ $BACKEND == fake ]] || { echo "x16 needs a fake drive of its own; skipped for BACKEND=$BACKEND"; exit 0; }
need_dbus_daemon x16
x_live_only
make_workdir

PROFILE=fake-x16
PROFILE2=fake-x16-b
MNT=$SCENARIO_ROOT/fake-mnt-x16
MNT2=$SCENARIO_ROOT/fake-mnt-x16-b
SHARED=$SCENARIO_ROOT/fake-shared-x16
SHARED2=$SCENARIO_ROOT/fake-shared-x16-b
FAKE_DIR=$SCENARIO_ROOT/fake-drive-x16
backend_flag=(--backend "fake:$FAKE_DIR")
config_a=$XDG_CONFIG_HOME/pdmount/$PROFILE.toml
config_b=$XDG_CONFIG_HOME/pdmount/$PROFILE2.toml
orphaned=$XDG_DATA_HOME/pdmount/$PROFILE/orphaned
local_rules=$XDG_CONFIG_HOME/pdmount/$PROFILE.pdignore
# The core stream and the volume in use poll every sync.poll_interval
# (30 s, the least); a change shows within one poll plus the listing.
CORE_WAIT=${CORE_WAIT:-45}
grace=16
# A and B each have a session bus with a notification stub (lib.sh).
notify_log=$work/notify.log
notify_log_b=$work/notify-b.log
start_session_bus x16-a
bus_a=$bus_address
start_notify_stub "$bus_a" "$notify_log"
start_session_bus x16-b
bus_b=$bus_address
start_notify_stub "$bus_b" "$notify_log_b"

mounted_at() { awk -v mnt="$1" '$2 == mnt { found = 1 } END { exit !found }' /proc/self/mounts; }
x16_cleanup() {
    "$PDMOUNT" fake profile clear "$FAKE_DIR" >/dev/null 2>&1 || true
    if is_mounted; then pd unmount "$MNT" >/dev/null 2>&1 || true; fi
    if is_mounted_second; then pd2 unmount "$MNT2" >/dev/null 2>&1 || true; fi
    local mnt
    for mnt in "$SHARED" "$SHARED2"; do
        if mounted_at "$mnt"; then fusermount3 -u "$mnt" 2>/dev/null || fusermount3 -uz "$mnt" || true; fi
    done
    scenario_cleanup
}
trap x16_cleanup EXIT

wait_for() { # <what> <seconds> <command...>
    local what=$1 limit=$2 start=$SECONDS
    shift 2
    until "$@"; do
        ((SECONDS - start < limit)) || fail "timed out waiting for $what; status: $(status_text)"
        sleep 0.5
    done
    echo "$what after $((SECONDS - start)) s"
}
status_has() { status_text | grep -Eq "$1"; }
tree_line() { status_has "^  $1: $2"; } # <name> <rest regex>
# The `Incoming:` lines under a tree's line, from a status on stdin.
incoming_lines() { # <tree name>
    awk -v tree="$1" '/^  [^ ]/ { inside = index($0, "  " tree ": ") == 1 } inside && /^    Incoming: / { print }'
}
shows() { [[ -e $SHARED/$1 ]]; }
gone() { [[ ! -e $SHARED/$1 ]]; }
expect_erofs() { # <what> <command...>
    local what=$1 out
    shift
    if out=$("$@" 2>&1); then fail "$what succeeded"; fi
    grep -q 'Read-only file system' <<<"$out" || fail "$what: not EROFS: $out"
}
fake_share() { # <name> <owner> <role> <dir to copy>; prints the share's id
    local share
    read -r _ _ share <<<"$("$PDMOUNT" fake root --name "$1" --owner "$2" --role "$3" --from "$4" "$FAKE_DIR")"
    [[ -n $share ]] || fail "fake root $1: no share"
    echo "$share"
}
role() { "$PDMOUNT" fake share-role --role "$1" "$2" "$FAKE_DIR"; } # <role|none> <share>
core_signal() { "$PDMOUNT" fake core-signal "$FAKE_DIR"; }
# A's config: the upload debounce, a short grace, the marker on in My Files
# and in the Shared root.
config_a() { # <debounce>
    printf '%s\n' "$saved_a" >"$config_a"
    cat >>"$config_a" <<EOF

[upload]
debounce = "$1"

[timeouts]
shutdown_grace = "${grace}s"

[sync]
pending_marker = true

[[roots]]
kind = "shared"
mountpoint = "$SHARED"
pending_marker = true
EOF
}
# A and B on their own session buses, the way x12 does it.
mount_a() { DBUS_SESSION_BUS_ADDRESS=$bus_a mount_now; }
mount_b() { DBUS_SESSION_BUS_ADDRESS=$bus_b mount_second; }

step "a fake drive of its own with shares from three users; A mounts My Files and Shared"
if is_mounted; then pd unmount "$MNT"; fi
if is_mounted_second; then pd2 unmount "$MNT2"; fi
for mnt in "$SHARED" "$SHARED2"; do
    if mounted_at "$mnt"; then fusermount3 -u "$mnt"; fi
done
remove_fake_dir "$FAKE_DIR"
mkdir -p "$work/seed/pdmount-test" "$work/edit" "$work/view" "$work/other" "$work/gone"
printf hello >"$work/seed/pdmount-test/hello"
printf 'a\n' >"$work/edit/a.txt"
printf base >"$work/edit/base.txt"
printf 'view\n' >"$work/view/v.txt"
printf 'other\n' >"$work/other/o.txt"
# 40 markers of other members in `Other` (editor) and `View` (viewer), D126.
python3 -I - "$work/other/.pdmount" "$work/view/.pdmount" <<'PY'
import json, os, sys, time, uuid
for folder in sys.argv[1:]:
    os.makedirs(folder)
    for n in range(40):
        body = {"version": 1, "time": int(time.time()), "entries": 1, "bytes": 10}
        with open(os.path.join(folder, f"{uuid.uuid4()}.pending"), "w") as f:
            json.dump(body, f)
PY
printf gone >"$work/gone/g.txt"
printf 'old\n' >"$work/gone/old.txt"
printf 'uploaded\n' >"$work/gone/up.txt"
# The share's owner keeps everything local-only, were it applied (D124).
printf '*\n' >"$work/gone/.pdignore"
"$PDMOUNT" fake seed --from "$work/seed" "$FAKE_DIR"
touch "$FAKE_DIR/.seeded-x16"
rm -f "$config_a" "$config_b"
fake_config "$PROFILE" dev-x16a "$MNT"
fake_config "$PROFILE2" dev-x16b "$MNT2"
saved_a=$(cat "$config_a")
# A's own rules: `*.local` and `build/` stay on this device, in every tree.
printf '*.local\nbuild/\n' >"$local_rules"
config_a 1s
printf '\n[[roots]]\nkind = "shared"\nmountpoint = "%s"\n' "$SHARED2" >>"$config_b"
pd reset --keep-login >/dev/null
pd2 reset --keep-login >/dev/null
rm -rf "$orphaned"
edit=$(fake_share Edit alice@example.test editor "$work/edit")
fake_share View alice@example.test viewer "$work/view" >/dev/null
fake_share Other bob@example.test editor "$work/other" >/dev/null
gone_share=$(fake_share Gone carol@example.test editor "$work/gone")
mount_a
wait_for "the shares" 30 shows Gone

step "the share's /.pdignore with '*' keeps nothing local-only: a new file uploads (D124)"
expect_eq "the share's rules" "$(cat "$SHARED/Gone/.pdignore")" '*'
mark=$(fake_change_mark)
printf 'a\n' >"$SHARED/Gone/a.txt"
wait_queue_empty
uploaded=$(fake_changes "$mark" | awk '$2 == "created-file" && $4 == "a.txt"' | wc -l)
expect_eq "uploads of Gone/a.txt" "$uploaded" 1

step "an uploaded file moved into a local-only folder keeps its remote place; its edit stays local (§7.7)"
xattr() { python3 -I -c 'import os, sys; print(os.getxattr(sys.argv[2], sys.argv[1]).decode())' "$@"; } # <name> <path>
up_link=$(xattr user.pdmount.link_id "$SHARED/Gone/up.txt")
mark=$(fake_change_mark)
mkdir "$SHARED/Gone/build"
mv "$SHARED/Gone/up.txt" "$SHARED/Gone/build/up.txt"
printf 'edited in build' >"$SHARED/Gone/build/up.txt"
wait_queue_empty
sleep 2
remote=$(fake_changes "$mark" | awk -v link="$up_link" '$3 == link || ($2 == "created-folder" && $4 == "build")')
expect_eq "remote changes of up.txt and build/" "$remote" ""
expect_eq "build/up.txt's state" "$(xattr user.pdmount.state "$SHARED/Gone/build/up.txt")" pending

step "an editor writes; a viewer gets EROFS; a rename between shares is EXDEV"
tree_line Edit 'writable \(editor\), ' || fail "Edit: $(status_text)"
tree_line View 'read-only \(viewer\), ' || fail "View: $(status_text)"
printf 'new\n' >"$SHARED/Edit/new.txt"
mkdir "$SHARED/Edit/dir"
printf x >"$SHARED/Edit/dir/x"
mv "$SHARED/Edit/dir/x" "$SHARED/Edit/dir/y"
rm "$SHARED/Edit/a.txt"
printf ' more' >>"$SHARED/Edit/base.txt"
expect_erofs "a new file in a viewer share" touch "$SHARED/View/new"
expect_erofs "a change in a viewer share" sh -c "printf x >>'$SHARED/View/v.txt'"
expect_erofs "a delete in a viewer share" rm "$SHARED/View/v.txt"
python3 -I - "$SHARED/Edit/new.txt" "$SHARED/Other/new.txt" <<'PY' || fail "a rename between two shares is not EXDEV"
import errno, os, sys
try:
    os.rename(sys.argv[1], sys.argv[2])
except OSError as error:
    sys.exit(0 if error.errno == errno.EXDEV else f"rename: {error}")
sys.exit("the rename between shares succeeded")
PY
mv "$SHARED/Edit/new.txt" "$SHARED/Other/moved.txt"
[[ -f $SHARED/Other/moved.txt && ! -e $SHARED/Edit/new.txt ]] || fail "mv between shares did not copy"
wait_queue_empty
tree_line Edit 'writable \(editor\), 0 pending, 0 parked' || fail "Edit: $(status_text)"

step "40 markers of other members: in Other (editor) 16 read, one notification, a capped Incoming:; none in View (D126)"
ls "$SHARED/Other/" "$SHARED/View/" >/dev/null
other_capped() { [[ $(status_text | incoming_lines Other | tail -n 1) == "    Incoming: +35 more" ]]; }
# Read after an event pass of the share's volume, at the latest after 2 min.
wait_for "Other's Incoming:" 150 other_capped
expect_eq "Other's Incoming: lines (5 and +35 more)" "$(status_text | incoming_lines Other | wc -l)" 6
expect_eq "View's Incoming: lines" "$(status_text | incoming_lines View | wc -l)" 0
other_notes() { grep -c 'changes on their way in Other from 16 devices: device [0-9a-f]\{8\}, ' "$notify_log" || true; }
expect_eq "notifications for Other" "$(other_notes)" 1
grep 'on their way in Other' "$notify_log" | grep -q ' 24 more markers were not read\.' ||
    fail "the notification does not count the markers not read: $(cat "$notify_log")"
grep -q 'on their way in View' "$notify_log" && fail "a notification for View: $(cat "$notify_log")"

step "the server refuses a change: the gate closes, the change is held, the file stays readable"
log_mark
hold_open held "$SHARED/Edit/held.txt" "held content"
# No core signal: the daemon learns it from the refusal.
role viewer "$edit"
release_hold held
wait_for "the refusal" 30 tree_line Edit 'read-only \((editor|viewer)\), 0 pending, 1 parked'
status_has '^    the server refused a change' || fail "no note in status: $(status_text)"
expect_log 'the server refused a change' "the refusal"
expect_eq "the held file" "$(cat "$SHARED/Edit/held.txt")" "held content"
expect_erofs "a new file after the refusal" touch "$SHARED/Edit/after"
grep -q 'to Edit wait' "$notify_log" || fail "no notification: $(cat "$notify_log")"

step "editor again: the gate opens and the held change uploads"
role editor "$edit"
core_signal
wait_for "the gate to open" "$CORE_WAIT" tree_line Edit 'writable \(editor\), 0 pending, 0 parked'
status_has '^    the server refused a change' && fail "the note stays: $(status_text)"
expect_log 'a share allows writing again' "the reopening"

step "a role change to viewer while a file is open holds its change; the close does not lift it"
hold_open open "$SHARED/Edit/open.txt" "open content"
role viewer "$edit"
core_signal
wait_for "the role change" "$CORE_WAIT" tree_line Edit 'read-only \(viewer\), 0 pending, 1 parked'
expect_eq "the open file" "$(cat "$SHARED/Edit/open.txt")" "open content"
expect_erofs "a new file as a viewer" touch "$SHARED/Edit/after"
release_hold open
sleep 3
tree_line Edit 'read-only \(viewer\), 0 pending, 1 parked' || fail "the close lifted the hold: $(status_text)"
role editor "$edit"
core_signal
wait_for "the gate to open again" "$CORE_WAIT" tree_line Edit 'writable \(editor\), 0 pending, 0 parked'

step "uploads wait from here on (debounce 120 s)"
pd unmount "$MNT" >/dev/null
config_a 120s
mount_a
wait_for "the shares" 30 shows Gone

step "a revocation with changes queued moves them to orphaned/"
expect_eq "g.txt" "$(cat "$SHARED/Gone/g.txt")" gone
printf n1 >"$SHARED/Gone/n1.txt"
mkdir "$SHARED/Gone/d"
printf n2 >"$SHARED/Gone/d/n2.txt"
printf ' edited' >>"$SHARED/Gone/g.txt"
rm "$SHARED/Gone/old.txt"
# Local-only by the profile's own rules: never queued, only in the staging.
printf 'kept locally' >"$SHARED/Gone/keep.local"
tree_line Gone 'writable \(editor\), [1-9][0-9]* pending' || fail "Gone: $(status_text)"
log_mark
role none "$gone_share"
core_signal
wait_for "the revoked share to go" "$CORE_WAIT" gone Gone
orphan_ready() { compgen -G "$orphaned/Gone (*)/pdmount-orphaned.json" >/dev/null; }
wait_for "the orphaned changes" 20 orphan_ready
dir=$(compgen -G "$orphaned/Gone (*)/pdmount-orphaned.json")
dir=${dir%/pdmount-orphaned.json}
expect_eq "n1.txt" "$(cat "$dir/files/n1.txt")" n1
expect_eq "d/n2.txt" "$(cat "$dir/files/d/n2.txt")" n2
expect_eq "g.txt" "$(cat "$dir/files/g.txt")" "gone edited"
expect_eq "keep.local (local-only)" "$(cat "$dir/files/keep.local")" "kept locally"
expect_eq "build/up.txt (not-uploaded)" "$(cat "$dir/files/build/up.txt")" "edited in build"
[[ ! -e $dir/state ]] || fail "the raw state was kept although every content was exported"
python3 -I - "$dir/pdmount-orphaned.json" <<'PY' || fail "the manifest is not as expected"
import json, sys
m = json.load(open(sys.argv[1]))
assert m["version"] == 1 and m["tree"] == "Gone", m
changes = {e["path"]: e for e in m["entries"]}
# A delete is not debounced: old.txt went to the trash before the revocation.
assert all(e["change"] != "trash" or "file" not in e for e in m["entries"]), m
assert changes["/d"]["change"] == "create-folder", m
for path in ("/n1.txt", "/d/n2.txt", "/g.txt", "/keep.local", "/build/up.txt"):
    assert changes[path]["file"] == "files" + path and "error" not in changes[path], m
# The share's `*` made nothing local-only; the profile's rule did (D124).
assert changes["/n1.txt"]["change"] == "upload", m
assert changes["/keep.local"]["change"] == "local-only", m
# Uploaded, then edited in a local-only folder: never queued (M6.0.3 V-1).
assert changes["/build/up.txt"]["change"] == "not-uploaded", m
PY
status_has "^Orphaned: +[0-9]+ changes of Gone \\(the share('s root is not found| left the shared-with-me listing)\\) in " ||
    fail "no Orphaned: line: $(status_text)"
expect_log 'a share went with changes queued' "the export"
grep -q 'changes to Gone kept' "$notify_log" || fail "no notification: $(cat "$notify_log")"
trees_dir=$XDG_DATA_HOME/pdmount/$PROFILE/trees
tree_states() { [[ $(find "$trees_dir" -mindepth 1 -maxdepth 1 | wc -l) -eq $1 ]]; }
wait_for "the gone share's state to go" 10 tree_states 3

step "B mounts the shares and sees A's changes"
mount_b
wait_for "B's shares" 30 test -e "$SHARED2/Edit"
expect_eq "held.txt on B" "$(cat "$SHARED2/Edit/held.txt")" "held content"
expect_eq "open.txt on B" "$(cat "$SHARED2/Edit/open.txt")" "open content"
expect_eq "base.txt on B" "$(cat "$SHARED2/Edit/base.txt")" "base more"
expect_eq "dir/y on B" "$(cat "$SHARED2/Edit/dir/y")" x
[[ ! -e $SHARED2/Edit/a.txt && ! -e $SHARED2/Edit/new.txt ]] || fail "deleted files show on B"
expect_eq "moved.txt on B" "$(cat "$SHARED2/Other/moved.txt")" new
[[ ! -e $SHARED2/Gone ]] || fail "B shows the revoked share"

step "A runs out of grace with changes in Edit: a marker without a device name"
id_a=$(device_uid 1)
id_a=${id_a#pdmount-}
[[ -n $id_a ]] || fail "no device id for A"
marker=$id_a.pending
"$PDMOUNT" fake profile --rtt 80 --up 1000000 "$FAKE_DIR"
mark=$(fake_change_mark)
log_mark
write_files "$SHARED/Edit/big" x16 24 1024
term_daemon
((stop_ms <= (grace + 5) * 1000)) || fail "the stop took $stop_ms ms; the grace is $grace s"
expect_log 'left a pending marker for other devices' "the marker"
expect_log 'unmounting with changes left' "the stop"
"$PDMOUNT" fake profile clear "$FAKE_DIR"
written=$(fake_changes "$mark" | awk -v name="$marker" '$2 == "created-file" && $4 == name' | wc -l)
expect_eq "markers written (the share's, none in My Files)" "$written" 1
ls "$SHARED2/Edit" >/dev/null
incoming_b() { status_text2 | incoming_lines Edit | grep -Eq '^    Incoming: device [0-9a-f]{8}: [0-9]+ changes? \('; }
wait_for "Incoming: on B" 90 incoming_b
json=$(cat "$SHARED2/Edit/.pdmount/$marker")
echo "marker: $json"
python3 -I - "$json" <<'PY' || fail "the marker's JSON is not as specified"
import json, sys
m = json.loads(sys.argv[1])
assert set(m) == {"version", "time", "entries", "bytes"}, m
assert m["version"] == 1 and m["entries"] >= 1 and m["bytes"] >= 1024, m
PY
[[ ! -e $MNT2/.pdmount/$marker ]] || fail "My Files has A's marker"
grep -q 'on their way in Edit from device' "$notify_log_b" || fail "no notification on B: $(cat "$notify_log_b")"

step "A's next mount uploads the rest and trashes the marker; B's Incoming goes"
mount_a
wait_queue_empty
no_incoming_b() { ! incoming_b; }
wait_for "B's Incoming: to go" 90 no_incoming_b
same_on_b() { diff -r "$work/x16" "$SHARED2/Edit/big" >/dev/null 2>&1; }
wait_for "the files on B" 90 same_on_b
expect_eq "notifications for Other after A's remounts" "$(other_notes)" 1

step "both daemons stop"
pd unmount "$MNT" >/dev/null
pd2 unmount "$MNT2" >/dev/null
for mnt in "$MNT" "$MNT2" "$SHARED" "$SHARED2"; do
    mounted_at "$mnt" && fail "$mnt is still mounted"
done
echo "x16: PASS"
