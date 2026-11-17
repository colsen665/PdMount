#!/usr/bin/env bash
# X14 (DESIGN.md §14.5, D110): further roots in one daemon, on a fake drive
# of its own. One `test` root with three trees: `work` on the drive's own
# volume with its write gate open, `shared` on another user's volume shared
# with the viewer role (gate closed), and `missing`, whose root does not
# exist; and a second root whose only tree does not exist. Checks the
# `Root:` lines of `status`, writes and EROFS, `unmount <root>` (a slow
# drain during which `status` answers and a second command is refused;
# My Files and the daemon go on), `mount -o ro <root>` and a plain mount
# again, an unmount from outside (noticed, then a new mount works), and a
# daemon under a service: it reports ready before the roots mount (they
# mount in the background), and `unmount <root>` leaves it running.
# Fake, live mode only, its own profile and fake drive.
source "$(dirname "$0")/lib.sh"
[[ $BACKEND == fake ]] || { echo "x14 needs a fake drive of its own; skipped for BACKEND=$BACKEND"; exit 0; }
x_live_only
make_workdir

PROFILE=fake-x14
MNT=$SCENARIO_ROOT/fake-mnt-x14
ROOT_MNT=$SCENARIO_ROOT/fake-root-x14
GONE_MNT=$SCENARIO_ROOT/fake-gone-x14
SECOND_MNT=$SCENARIO_ROOT/fake-second-x14
FAKE_DIR=$SCENARIO_ROOT/fake-drive-x14
backend_flag=(--backend "fake:$FAKE_DIR")
TEST_ROOT=$MNT/pdmount-test
config_file=$XDG_CONFIG_HOME/pdmount/$PROFILE.toml

daemon=
listener=
x14_cleanup() {
    "$PDMOUNT" fake profile clear "$FAKE_DIR" >/dev/null 2>&1 || true
    [[ -z $listener ]] || kill "$listener" 2>/dev/null || true
    if [[ -n $daemon ]]; then
        kill -TERM "$daemon" 2>/dev/null || true
        wait "$daemon" 2>/dev/null || true
    fi
    if is_mounted; then pd unmount "$MNT" >/dev/null 2>&1 || true; fi
    for mnt in "$ROOT_MNT" "$GONE_MNT" "$SECOND_MNT"; do
        if mounted_at "$mnt"; then fusermount3 -u "$mnt" 2>/dev/null || fusermount3 -uz "$mnt" || true; fi
    done
    scenario_cleanup
}
trap x14_cleanup EXIT

mounted_at() { awk -v mnt="$1" '$2 == mnt { found = 1 } END { exit !found }' /proc/self/mounts; }
wait_for() { # <what> <seconds> <command...>
    local what=$1 limit=$2 start=$SECONDS
    shift 2
    until "$@"; do
        ((SECONDS - start < limit)) || fail "timed out waiting for $what; status: $(status_text)"
        sleep 0.2
    done
}
status_has() { status_text | grep -Eq "$1"; }
root_line() { # <mountpoint> <state regex>
    status_has "^Root: +$1 \\(test\\): $2\$"
}
expect_erofs() { # <what> <command...>
    local what=$1 out
    shift
    if out=$("$@" 2>&1); then fail "$what succeeded"; fi
    grep -q 'Read-only file system' <<<"$out" || fail "$what: not EROFS: $out"
}
my_files_works() { # <tag>
    printf '%s' "$1" >"$TEST_ROOT/x14-$1"
    expect_eq "My Files read-back" "$(cat "$TEST_ROOT/x14-$1")" "$1"
}
work_uploaded() { status_has '^  work: writable, 0 pending,'; }
log_has() { log_since_mark | grep -Eq "$1"; }
kill_daemon() { # SIGKILL, as a crash: its mounts stay, dead
    local pid
    pid=$(daemon_pid)
    [[ -n $pid ]] || fail "no daemon to kill"
    kill -KILL "$pid"
    while kill -0 "$pid" 2>/dev/null; do sleep 0.1; done
}
on_drive() { # <name>
    local changes
    changes=$("$PDMOUNT" fake changes "$FAKE_DIR")
    grep -q " $1\$" <<<"$changes"
}

step "a fake drive of its own with two root folders; the profile's config names them"
if is_mounted; then pd unmount "$MNT"; fi
for mnt in "$ROOT_MNT" "$GONE_MNT" "$SECOND_MNT"; do
    if mounted_at "$mnt"; then fusermount3 -u "$mnt"; fi
done
remove_fake_dir "$FAKE_DIR"
mkdir -p "$work/seed/pdmount-test"
printf hello >"$work/seed/pdmount-test/hello"
"$PDMOUNT" fake seed --from "$work/seed" "$FAKE_DIR"
touch "$FAKE_DIR/.seeded-x14"
rm -f "$config_file"
fake_config "$PROFILE" dev-x14 "$MNT"
# The state of an earlier drive, trees/ included.
pd reset --keep-login >/dev/null
read -r work_volume work_root <<<"$("$PDMOUNT" fake root --name work "$FAKE_DIR")"
read -r shared_volume shared_root share <<<"$("$PDMOUNT" fake root --name shared --owner carol --role viewer "$FAKE_DIR")"
[[ -n $work_root && -n $share && $shared_volume != "$work_volume" ]] ||
    fail "fake root: $work_volume $work_root / $shared_volume $shared_root $share"
cat >>"$config_file" <<EOF

[[roots]]
kind = "test"
mountpoint = "$ROOT_MNT"

[[roots.tree]]
name = "work"
volume = "$work_volume"
root = "$work_root"
writable = true

[[roots.tree]]
name = "shared"
volume = "$shared_volume"
root = "$shared_root"

[[roots.tree]]
name = "missing"
volume = "$work_volume"
root = "link-x14-missing"

[[roots]]
kind = "test"
mountpoint = "$GONE_MNT"

[[roots.tree]]
name = "gone"
volume = "$work_volume"
root = "link-x14-gone"
EOF

step "mount: My Files, the root with two trees, the other root failed"
log_mark
mount_now
wait_for "the root" 30 root_line "$ROOT_MNT" mounted
st=$(status_text)
grep -Eq '^  work: writable, 0 pending, 0 parked, ' <<<"$st" || fail "work: $st"
grep -Eq '^  shared: read-only, 0 pending, 0 parked, ' <<<"$st" || fail "shared: $st"
grep -Eq '^  missing: not opened: cannot open the tree missing' <<<"$st" || fail "missing: $st"
grep -Eq "^Root: +$GONE_MNT \\(test\\): failed: no tree opened \\(gone: " <<<"$st" || fail "the failed root: $st"
expect_eq "the root's folders" "$(ls "$ROOT_MNT" | tr '\n' ' ')" "shared work "
mounted_at "$GONE_MNT" && fail "the failed root is mounted"
expect_eq "My Files" "$(cat "$TEST_ROOT/hello")" hello
my_files_works first
expect_log 'a further root did not mount.*no tree opened' "the failed root"

step "writes on the writable tree; EROFS on the closed one and the top level"
printf one >"$ROOT_MNT/work/x14-one"
mkdir "$ROOT_MNT/work/dir"
expect_eq "read-back" "$(cat "$ROOT_MNT/work/x14-one")" one
expect_erofs "a file in the closed tree" touch "$ROOT_MNT/shared/x14-no"
expect_erofs "a folder in the closed tree" mkdir "$ROOT_MNT/shared/x14-no"
expect_erofs "a folder in the top level" mkdir "$ROOT_MNT/x14-no"
expect_erofs "a move into the closed tree" mv "$ROOT_MNT/work/x14-one" "$ROOT_MNT/shared/x14-one"
[[ -f $ROOT_MNT/work/x14-one ]] || fail "the failed move removed its source"
wait_for "the upload" 60 work_uploaded
on_drive x14-one || fail "x14-one is not on the drive"

step "unmount <root> with a slow drain: status answers, a second command is refused"
"$PDMOUNT" fake profile --up 4096 "$FAKE_DIR" >/dev/null
head -c $((64 * 1024)) /dev/urandom >"$work/slow"
cp "$work/slow" "$ROOT_MNT/work/x14-slow"
pid=$(daemon_pid)
pd unmount "$ROOT_MNT" >"$work/unmount.out" 2>&1 &
unmounting=$!
wait_for "the unmounting state" 20 root_line "$ROOT_MNT" 'unmounting: uploading first'
start=$(now_ms)
st=$(status_text)
took=$(($(now_ms) - start))
((took < 3000)) || fail "status took $took ms during the drain"
grep -Eq '^  work: writable, [1-9][0-9]* pending,' <<<"$st" || fail "the pending upload in status: $st"
if out=$(pd unmount "$ROOT_MNT" 2>&1); then fail "a second unmount succeeded: $out"; fi
grep -q 'is being unmounted' <<<"$out" || fail "the second unmount: $out"
if out=$(pd mount "$ROOT_MNT" 2>&1); then fail "a mount during the unmount succeeded: $out"; fi
grep -q 'is being unmounted' <<<"$out" || fail "the mount during the unmount: $out"
my_files_works during
wait "$unmounting" || fail "the unmount failed: $(cat "$work/unmount.out")"
grep -Eq "^Uploading [1-9][0-9]* pending changes of $ROOT_MNT first" "$work/unmount.out" ||
    fail "no progress line: $(cat "$work/unmount.out")"
grep -q "^Unmounted $ROOT_MNT.\$" "$work/unmount.out" || fail "unmount: $(cat "$work/unmount.out")"
"$PDMOUNT" fake profile clear "$FAKE_DIR" >/dev/null
on_drive x14-slow || fail "x14-slow was not uploaded before the unmount"

step "after the unmount: My Files and the daemon go on"
root_line "$ROOT_MNT" 'not mounted' || fail "status: $(status_text)"
mounted_at "$ROOT_MNT" && fail "$ROOT_MNT is still mounted"
expect_eq "the daemon" "$(daemon_pid)" "$pid"
my_files_works after
if out=$(pd unmount "$ROOT_MNT" 2>&1); then fail "an unmount of an unmounted root succeeded: $out"; fi
grep -q 'is not mounted' <<<"$out" || fail "unmount of an unmounted root: $out"

step "mount -o ro <root>: EROFS on the writable tree; a plain mount makes it writable again"
out=$(pd mount -o ro "$ROOT_MNT") || fail "mount -o ro: $out"
grep -q "^Mounted $ROOT_MNT (read-only).\$" <<<"$out" || fail "mount -o ro: $out"
root_line "$ROOT_MNT" 'mounted, read-only' || fail "status: $(status_text)"
expect_eq "read under -o ro" "$(cat "$ROOT_MNT/work/x14-one")" one
cmp -s "$work/slow" "$ROOT_MNT/work/x14-slow" || fail "x14-slow differs after the remount"
expect_erofs "a write under -o ro" touch "$ROOT_MNT/work/x14-ro"
if out=$(pd mount "$ROOT_MNT" 2>&1); then fail "a second mount succeeded: $out"; fi
grep -q 'is already mounted' <<<"$out" || fail "a second mount: $out"
pd unmount "$ROOT_MNT" >/dev/null
pd mount "$ROOT_MNT" >/dev/null
root_line "$ROOT_MNT" mounted || fail "status: $(status_text)"
printf two >"$ROOT_MNT/work/x14-two"
expect_eq "read-back" "$(cat "$ROOT_MNT/work/x14-two")" two

step "an unmount from outside is noticed; then a mount works"
wait_for "the upload" 60 work_uploaded
fusermount3 -u "$ROOT_MNT"
wait_for "the outside unmount in status" 15 root_line "$ROOT_MNT" 'not mounted \(unmounted from outside\)'
expect_log 'a further root was unmounted from outside; stopping its trees' "the outside unmount"
pd mount "$ROOT_MNT" >/dev/null
expect_eq "read-back after the outside unmount" "$(cat "$ROOT_MNT/work/x14-two")" two
printf three >"$ROOT_MNT/work/x14-three"
# At once after the outside unmount: the mount stops the trees itself, or
# waits for the watch that does.
wait_for "the upload" 60 work_uploaded
fusermount3 -u "$ROOT_MNT"
pd mount "$ROOT_MNT" >/dev/null || fail "a mount at once after an outside unmount"
root_line "$ROOT_MNT" mounted || fail "status: $(status_text)"
expect_eq "read-back" "$(cat "$ROOT_MNT/work/x14-three")" three
expect_eq "the daemon" "$(daemon_pid)" "$pid"

step "unmount --force <root> with a file open: the detached mount is stopped; a mount works"
# The lazy detach keeps the FUSE session while the file is open; the
# daemon goes by the mount table.
wait_for "the upload" 60 work_uploaded
exec {held}<"$ROOT_MNT/work/x14-three"
out=$(pd unmount --force "$ROOT_MNT") || fail "unmount --force <root>: $out"
mounted_at "$ROOT_MNT" && fail "$ROOT_MNT is still in the mount table"
wait_for "the forced unmount in status" 15 root_line "$ROOT_MNT" 'not mounted \(unmounted from outside\)'
pd mount "$ROOT_MNT" >/dev/null || fail "a mount after unmount --force with a file open"
expect_eq "read-back after the forced unmount" "$(cat "$ROOT_MNT/work/x14-three")" three
exec {held}<&-

step "the daemon's stop unmounts the roots too"
pd unmount "$MNT" >/dev/null
mounted_at "$ROOT_MNT" && fail "$ROOT_MNT is still mounted after the daemon's stop"
mounted_at "$MNT" && fail "$MNT is still mounted"
on_drive x14-three || fail "x14-three is not on the drive"

step "under a service: ready before the roots mount, which never delays the start"
# Every call takes 1.5 s, so opening the trees takes a few seconds.
"$PDMOUNT" fake profile --rtt 1500 "$FAKE_DIR" >/dev/null
fifo=$work/fd.fifo
mkfifo "$fifo"
cat "$fifo" >"$work/fd.out" &
listener=$!
"$PDMOUNT" --profile "$PROFILE" "${secrets_flag[@]}" mount --foreground --service openrc --notify-fd 3 \
    "${backend_flag[@]}" "$MNT" 3>"$fifo" >>"$work/daemon.log" 2>&1 &
daemon=$!
wait "$listener" || fail "the notification descriptor was not closed"
listener=
expect_eq "bytes on the descriptor" "$(od -An -c <"$work/fd.out" | tr -d ' ')" '\n'
st=$(status_text)
grep -Eq "^Root: +$ROOT_MNT \\(test\\): (mounting|not mounted)\$" <<<"$st" ||
    fail "the root was mounted before the readiness report: $st"
"$PDMOUNT" fake profile clear "$FAKE_DIR" >/dev/null
wait_for "the root" 60 root_line "$ROOT_MNT" mounted
expect_eq "read-back" "$(cat "$ROOT_MNT/work/x14-three")" three

step "under a service: unmount <root> leaves the daemon and My Files running"
out=$(pd unmount "$ROOT_MNT") || fail "unmount <root> under a service: $out"
grep -q "^Unmounted $ROOT_MNT.\$" <<<"$out" || fail "unmount: $out"
sleep 1
kill -0 "$daemon" || fail "the daemon stopped"
my_files_works service
root_line "$ROOT_MNT" 'not mounted' || fail "status: $(status_text)"
pd mount "$ROOT_MNT" >/dev/null
root_line "$ROOT_MNT" mounted || fail "status: $(status_text)"

step "stop during unmount <root> with a file open: the root is detached, not left behind"
# The unmount's drain ends at the stop, and the busy root is detached and
# stopped; it must not go back to mounted after the stop took the mounts.
"$PDMOUNT" fake profile --up 4096 "$FAKE_DIR" >/dev/null
head -c $((64 * 1024)) /dev/urandom >"$work/busy"
cp "$work/busy" "$ROOT_MNT/work/x14-busy"
exec {held}<"$ROOT_MNT/work/x14-three"
pd unmount "$ROOT_MNT" >"$work/unmount-stop.out" 2>&1 &
unmounting=$!
wait_for "the unmounting state" 20 root_line "$ROOT_MNT" 'unmounting: uploading first'
kill -TERM "$daemon"
wait "$daemon" || fail "the daemon exited with an error"
daemon=
wait "$unmounting" || true
# This daemon logs to its standard error.
mounted_at "$ROOT_MNT" && fail "$ROOT_MNT was left mounted by the stop: $(tail -20 "$work/daemon.log")"
mounted_at "$MNT" && fail "$MNT is still mounted"
grep -q 'the daemon stops: unmounting the further root without uploading the rest' "$work/daemon.log" ||
    fail "the unmount's stop: $(tail -20 "$work/daemon.log")"
exec {held}<&-
"$PDMOUNT" fake profile clear "$FAKE_DIR" >/dev/null

step "a second SIGTERM hurries the roots' upload: the daemon stops at once and keeps the rest"
mount_now
wait_for "the root" 30 root_line "$ROOT_MNT" mounted
"$PDMOUNT" fake profile --up 4096 "$FAKE_DIR" >/dev/null
head -c $((96 * 1024)) /dev/urandom >"$work/hurried"
cp "$work/hurried" "$ROOT_MNT/work/x14-hurried"
pid=$(daemon_pid)
log_mark
kill -TERM "$pid"
wait_for "the stop" 20 log_has 'shutting down'
start=$SECONDS
kill -TERM "$pid"
while kill -0 "$pid" 2>/dev/null; do
    ((SECONDS - start < 5)) || fail "the daemon did not stop within 5 s of the second signal: $(log_since_mark | tail -20)"
    sleep 0.1
done
expect_log 'stop signal during the stop: ending its waits' "the second signal"
expect_log 'pdmount::daemon(::[a-z_]+)?: stopped$' "the store commit"
if log_since_mark | grep -q 'could not commit'; then fail "a store was not committed: $(log_since_mark | tail -20)"; fi
mounted_at "$ROOT_MNT" && fail "$ROOT_MNT is still mounted"
mounted_at "$MNT" && fail "$MNT is still mounted"
"$PDMOUNT" fake profile clear "$FAKE_DIR" >/dev/null

step "a killed daemon leaves stale mounts: --if-stale removes the roots' too"
mount_now
wait_for "the root" 30 root_line "$ROOT_MNT" mounted
# What the hurried stop left uploads now (RQ3).
wait_for "the rest of the hurried upload" 60 on_drive x14-hurried
cmp -s "$work/hurried" "$ROOT_MNT/work/x14-hurried" || fail "x14-hurried differs"
kill_daemon
mounted_at "$ROOT_MNT" || fail "no stale mount at $ROOT_MNT after the kill"
out=$(pd unmount --if-stale "$MNT") || fail "unmount --if-stale: $out"
grep -q "^Removed the stale mount at $MNT.\$" <<<"$out" || fail "--if-stale, My Files: $out"
grep -q "^Removed the stale mount at $ROOT_MNT.\$" <<<"$out" || fail "--if-stale, the root: $out"
mounted_at "$ROOT_MNT" && fail "$ROOT_MNT is still mounted"
mounted_at "$MNT" && fail "$MNT is still mounted"

step "a killed daemon's dead root mount is cleared by the next start"
mount_now
wait_for "the root" 30 root_line "$ROOT_MNT" mounted
kill_daemon
pd unmount --force "$MNT" >/dev/null
mounted_at "$ROOT_MNT" || fail "no stale mount at $ROOT_MNT after the kill"
log_mark
mount_now
wait_for "the root" 30 root_line "$ROOT_MNT" mounted
expect_log 'removing the dead mount a stopped daemon left' "the dead root mount"
expect_eq "read-back" "$(cat "$ROOT_MNT/work/x14-three")" three

step "stop: the mounts drain at once, so a second root's upload is not starved by the first's"
# A third root with a tree of its own. Nothing uploads before the stop (a
# long debounce); the first root's upload outlasts the grace, the second's
# is small. One after the other, the second root got no time.
read -r work2_volume work2_root <<<"$("$PDMOUNT" fake root --name work2 "$FAKE_DIR")"
[[ -n $work2_root ]] || fail "fake root work2"
cat >>"$config_file" <<EOF

[[roots]]
kind = "test"
mountpoint = "$SECOND_MNT"

[[roots.tree]]
name = "work2"
volume = "$work2_volume"
root = "$work2_root"
writable = true

[upload]
debounce = "120s"

[timeouts]
shutdown_grace = "8s"
EOF
pd unmount "$MNT" >/dev/null
mount_now
wait_for "the root" 30 root_line "$ROOT_MNT" mounted
wait_for "the second root" 30 root_line "$SECOND_MNT" mounted
"$PDMOUNT" fake profile --up 4096 "$FAKE_DIR" >/dev/null
head -c $((96 * 1024)) /dev/urandom >"$ROOT_MNT/work/x14-large"
# One tree alone in its mount: its root is the mount.
printf small >"$SECOND_MNT/x14-small"
on_drive x14-small && fail "x14-small uploaded before the stop"
pid=$(daemon_pid)
kill -TERM "$pid"
while kill -0 "$pid" 2>/dev/null; do sleep 0.2; done
"$PDMOUNT" fake profile clear "$FAKE_DIR" >/dev/null
on_drive x14-small || fail "the second root's upload got no time at the stop"
mounted_at "$SECOND_MNT" && fail "$SECOND_MNT is still mounted"
mounted_at "$ROOT_MNT" && fail "$ROOT_MNT is still mounted"
mounted_at "$MNT" && fail "$MNT is still mounted"
# The rest uploads at the next stop's drain (RQ3), and nothing is left
# pending for the next run's reset.
mount_now
wait_for "the root" 30 root_line "$ROOT_MNT" mounted
pd unmount "$MNT" >/dev/null
on_drive x14-large || fail "x14-large was not uploaded after the stop"
echo "x14: PASS"
