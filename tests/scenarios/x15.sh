#!/usr/bin/env bash
# X15 (DESIGN.md §14.5, D107, D112): a Shared mount on a fake drive of its
# own. Shares from three other users: `Team` from alice and from bob (same
# name: owner suffixes), `Docs` from carol, `Quiet` from carol (never
# browsed), and a shared file `plan.txt` from bob (in the files folder).
# Checks the names, reads, EROFS on writes, the poll intervals in `status`
# (other users' volumes every 10 min, the one in use like the own volume),
# a new share after the core event, a revoked one that goes on the core
# event while a file of it is open (EIO), one that goes at the first "not
# found" of its root without a core event, and names that stay with their
# shares (D125): the suffix stays when the other share goes, a newcomer named
# like a shown name gets its own suffix, and a remount keeps every name.
# Fake, live mode only, its own profile and fake drive.
source "$(dirname "$0")/lib.sh"
[[ $BACKEND == fake ]] || { echo "x15 needs a fake drive of its own; skipped for BACKEND=$BACKEND"; exit 0; }
x_live_only
make_workdir

PROFILE=fake-x15
MNT=$SCENARIO_ROOT/fake-mnt-x15
SHARED=$SCENARIO_ROOT/fake-shared-x15
FAKE_DIR=$SCENARIO_ROOT/fake-drive-x15
backend_flag=(--backend "fake:$FAKE_DIR")
config_file=$XDG_CONFIG_HOME/pdmount/$PROFILE.toml
# The core stream and the volume in use poll every sync.poll_interval
# (30 s, the least); a change shows within one poll plus the listing.
CORE_WAIT=${CORE_WAIT:-45}

x15_cleanup() {
    exec 3<&- 2>/dev/null || true
    if is_mounted; then pd unmount "$MNT" >/dev/null 2>&1 || true; fi
    if mounted_at "$SHARED"; then fusermount3 -u "$SHARED" 2>/dev/null || fusermount3 -uz "$SHARED" || true; fi
    scenario_cleanup
}
trap x15_cleanup EXIT

mounted_at() { awk -v mnt="$1" '$2 == mnt { found = 1 } END { exit !found }' /proc/self/mounts; }
wait_for() { # <what> <seconds> <command...>
    local what=$1 limit=$2 start=$SECONDS
    shift 2
    until "$@"; do
        ((SECONDS - start < limit)) || fail "timed out waiting for $what; status: $(status_text)"
        sleep 0.5
    done
}
status_has() { status_text | grep -Eq "$1"; }
root_line() { status_has "^Root: +$SHARED \\(shared\\): $1\$"; }
names() { ls -1 "$1" | tr '\n' '|'; }
shows() { [[ -e $SHARED/$1 ]]; }
gone() { ! names "$SHARED" | grep -Fq "$1|"; }
expect_erofs() { # <what> <command...>
    local what=$1 out
    shift
    if out=$("$@" 2>&1); then fail "$what succeeded"; fi
    grep -q 'Read-only file system' <<<"$out" || fail "$what: not EROFS: $out"
}
tree_line() { # <name regex> <rest regex>
    status_has "^  $1: read-only \\(viewer\\), 0 pending, 0 parked, .*$2"
}
fake_share() { # <name> <owner> <dir to copy>; prints the share's id
    local share
    read -r _ _ share <<<"$("$PDMOUNT" fake root --name "$1" --owner "$2" --role viewer --from "$3" "$FAKE_DIR")"
    [[ -n $share ]] || fail "fake root $1: no share"
    echo "$share"
}

step "a fake drive of its own with shares from three users; the profile mounts Shared"
if is_mounted; then pd unmount "$MNT"; fi
if mounted_at "$SHARED"; then fusermount3 -u "$SHARED"; fi
remove_fake_dir "$FAKE_DIR"
mkdir -p "$work/seed/pdmount-test" "$work/alice" "$work/bob" "$work/carol" "$work/quiet" "$work/dave"
printf hello >"$work/seed/pdmount-test/hello"
printf 'from alice\n' >"$work/alice/a.txt"
mkdir "$work/alice/sub"
head -c $((300 * 1024)) /dev/urandom >"$work/alice/sub/big.bin"
printf 'from bob\n' >"$work/bob/b.txt"
head -c $((200 * 1024)) /dev/urandom >"$work/carol/c.bin"
printf 'quiet\n' >"$work/quiet/q.txt"
printf 'from dave\n' >"$work/dave/d.txt"
printf 'the plan\n' >"$work/plan.txt"
"$PDMOUNT" fake seed --from "$work/seed" "$FAKE_DIR"
touch "$FAKE_DIR/.seeded-x15"
rm -f "$config_file"
fake_config "$PROFILE" dev-x15 "$MNT"
pd reset --keep-login >/dev/null
# The names a previous run's shares were shown under (`reset` keeps them).
rm -f "$XDG_DATA_HOME/pdmount/$PROFILE/shared-names.json"
fake_share Team alice "$work/alice" >/dev/null
bob_team=$(fake_share Team bob "$work/bob")
docs=$(fake_share Docs carol "$work/carol")
quiet=$(fake_share Quiet carol "$work/quiet")
"$PDMOUNT" fake share-file --name plan.txt --owner bob --from "$work/plan.txt" "$FAKE_DIR" >/dev/null
cat >>"$config_file" <<EOF

[[roots]]
kind = "shared"
mountpoint = "$SHARED"
EOF

step "mount: one folder per share, owner suffixes for the same name, the file in the files folder"
log_mark
mount_now
wait_for "the Shared root" 30 root_line mounted
wait_for "the shares" 30 shows "Docs"
expect_eq "the top level" "$(names "$SHARED")" "Docs|Quiet|Shared files|Team (alice)|Team (bob)|"
expect_eq "the files folder" "$(names "$SHARED/Shared files")" "plan.txt|"
expect_eq "the top level's mode" "$(stat -c %a "$SHARED")" 555

step "reads work; writes give EROFS"
expect_eq "a.txt" "$(cat "$SHARED/Team (alice)/a.txt")" "from alice"
expect_eq "b.txt" "$(cat "$SHARED/Team (bob)/b.txt")" "from bob"
cmp -s "$work/alice/sub/big.bin" "$SHARED/Team (alice)/sub/big.bin" || fail "big.bin differs"
expect_eq "plan.txt" "$(cat "$SHARED/Shared files/plan.txt")" "the plan"
expect_erofs "a new file in a share" touch "$SHARED/Team (alice)/new"
expect_erofs "a change of a shared file" sh -c "printf x >>'$SHARED/Team (alice)/a.txt'"
expect_erofs "a folder in a share" mkdir "$SHARED/Team (bob)/new"
expect_erofs "a rename in a share" mv "$SHARED/Team (bob)/b.txt" "$SHARED/Team (bob)/b2.txt"
expect_erofs "a delete in a share" rm "$SHARED/Team (bob)/b.txt"
expect_erofs "a change of the shared file" sh -c "printf x >>'$SHARED/Shared files/plan.txt'"
expect_erofs "a folder in the top level" mkdir "$SHARED/new"
expect_eq "My Files" "$(cat "$MNT/pdmount-test/hello")" hello

step "polling: other users' volumes every 10 min; the one in use like the own volume (D107)"
ls "$SHARED/Team (bob)" >/dev/null
wait_for "bob's volume in use" 10 tree_line 'Team \(bob\)' ', every 30 s,'
tree_line 'Team \(alice\)' ', every 600 s,' || fail "alice's volume: $(status_text)"
tree_line 'Docs' ', every 600 s,' || fail "carol's volume: $(status_text)"
ls "$SHARED/Team (alice)" >/dev/null
wait_for "alice's volume in use" 10 tree_line 'Team \(alice\)' ', every 30 s,'
tree_line 'Team \(bob\)' ', every 600 s,' || fail "bob's volume after alice's: $(status_text)"
# Bob's share and bob's file are on one volume: one poller for both.
expect_log 'a share is mounted' "the shares' mounts"

step "a revoked share goes on the core event while a file of it is open: EIO"
exec 3<"$SHARED/Docs/c.bin"
head -c 4096 <&3 >/dev/null
dave=$(fake_share Late dave "$work/dave")
"$PDMOUNT" fake share-role --role none "$docs" "$FAKE_DIR"
"$PDMOUNT" fake core-signal "$FAKE_DIR"
log_mark
wait_for "the new share" "$CORE_WAIT" shows Late
wait_for "the revoked share to go" "$CORE_WAIT" gone Docs
expect_eq "the new share's file" "$(cat "$SHARED/Late/d.txt")" "from dave"
if out=$(cat <&3 2>&1 >/dev/null); then fail "a read of the revoked share's open file succeeded"; fi
grep -q 'Input/output error' <<<"$out" || fail "the open file: not EIO: $out"
exec 3<&-
expect_log 'the core event stream says shares changed' "the core event"
expect_log 'a share left the listing; removing it' "the revocation"
[[ -n $dave ]] || fail "no share for Late"

step "a revoked share goes at the first \"not found\" of its root, before any core event"
"$PDMOUNT" fake share-role --role none "$quiet" "$FAKE_DIR"
log_mark
# Its root was never listed: this listing finds it gone.
ls "$SHARED/Quiet" >/dev/null 2>&1 || true
wait_for "the share without a core event" 10 gone Quiet
expect_log "a share's root is not found; removing the share" "the not-found"

step "the gone shares' state is deleted"
trees_dir=$XDG_DATA_HOME/pdmount/$PROFILE/trees
# Open: Team (alice), Team (bob), plan.txt, Late.
tree_states() { [[ $(find "$trees_dir" -mindepth 1 -maxdepth 1 | wc -l) -eq $1 ]]; }
wait_for "the state of the gone shares to go" 10 tree_states 4

step "a shown name stays with its share; a newcomer with that name gets its own suffix (D125)"
"$PDMOUNT" fake share-role --role none "$bob_team" "$FAKE_DIR"
"$PDMOUNT" fake core-signal "$FAKE_DIR"
wait_for "bob's Team to go" "$CORE_WAIT" gone "Team (bob)"
expect_eq "the top level" "$(names "$SHARED")" "Late|Shared files|Team (alice)|"
fake_share "Team (alice)" mallory "$work/dave" >/dev/null
"$PDMOUNT" fake core-signal "$FAKE_DIR"
wait_for "mallory's share" "$CORE_WAIT" shows "Team (alice) (mallory)"
expect_eq "the top level" "$(names "$SHARED")" "Late|Shared files|Team (alice)|Team (alice) (mallory)|"
expect_eq "a.txt" "$(cat "$SHARED/Team (alice)/a.txt")" "from alice"
expect_eq "the files folder" "$(names "$SHARED/Shared files")" "plan.txt|"

step "a remount keeps the names"
pd unmount "$MNT" >/dev/null
mount_now
wait_for "the Shared root" 30 root_line mounted
wait_for "the shares" 30 shows "Team (alice) (mallory)"
expect_eq "the top level" "$(names "$SHARED")" "Late|Shared files|Team (alice)|Team (alice) (mallory)|"
expect_eq "a.txt" "$(cat "$SHARED/Team (alice)/a.txt")" "from alice"
expect_eq "d.txt" "$(cat "$SHARED/Team (alice) (mallory)/d.txt")" "from dave"

step "the daemon's stop unmounts the Shared root too"
pd unmount "$MNT" >/dev/null
mounted_at "$SHARED" && fail "$SHARED is still mounted after the daemon's stop"
mounted_at "$MNT" && fail "$MNT is still mounted"
echo "x15: PASS"
