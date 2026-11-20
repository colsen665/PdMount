#!/usr/bin/env bash
# X18 (DESIGN.md §6.3, §15.3, D132 amended): nodes whose signatures do not
# verify, on the fake. A file whose content key and a folder whose key "do
# not verify" (the fake marks them) are listed and stat, but open, rename,
# delete, a create inside and a listing of the folder answer EACCES (My
# Files refuses by default); `status` counts them; `signatures check` finds
# a third one in a folder never listed; an accept opens the file, also after
# a remount, while the folder stays refused; once they verify again the list
# is empty. Fake, live mode only.
source "$(dirname "$0")/lib.sh"
[[ $BACKEND == fake ]] || { echo "x18 marks signatures on the fake; skipped for BACKEND=$BACKEND"; exit 0; }
x_live_only
ensure_dataset
prepare_mount
make_workdir
area=$TEST_ROOT/x18
remote=/pdmount-test/x18

marks=("content-key $remote/a.txt" "key $remote/dir" "name $remote/deep/sub/hidden.txt")
unmark_all() {
    local mark
    for mark in "${marks[@]}"; do
        read -r part path <<<"$mark"
        "$PDMOUNT" fake unverified --clear --part "$part" --path "$path" "$FAKE_DIR" >/dev/null 2>&1 || true
    done
}
x18_cleanup() {
    unmark_all
    scenario_cleanup
}
trap x18_cleanup EXIT

wait_for() { # <what> <seconds> <command...>
    local what=$1 limit=$2 start=$SECONDS
    shift 2
    until "$@"; do
        ((SECONDS - start < limit)) || fail "timed out waiting for $what; list: $(pd signatures list 2>&1)"
        sleep 0.5
    done
}
listed() { pd signatures list 2>/dev/null | grep -Eq " $1( \\[name unverified\\])?\$"; }
not_listed() { ! pd signatures list 2>/dev/null | grep -Fq "$1"; }
list_empty() { [[ $(pd signatures list 2>/dev/null) == 'no nodes with unverified signatures' ]]; }
expect_eacces() { # <what> <command...>
    local what=$1 out
    shift
    if out=$("$@" 2>&1); then fail "$what succeeded"; fi
    grep -q 'Permission denied' <<<"$out" || fail "$what: not EACCES: $out"
}
id_of() { # <path suffix>
    pd signatures list --json | python3 -I -c '
import json, sys
nodes = [n for n in json.load(sys.stdin) if (n.get("path") or "").endswith(sys.argv[1])]
assert len(nodes) == 1, nodes
print(nodes[0]["id"])' "$1"
}

step "a file, a folder and a folder never listed, made by another client"
unmark_all
scenario_area x18
wait_queue_empty
printf 'content a\n' >"$work/a.txt"
printf 'content b\n' >"$work/b.txt"
printf 'hidden\n' >"$work/hidden.txt"
"$PDMOUNT" fake create --path "$remote/a.txt" --from "$work/a.txt" "$FAKE_DIR"
"$PDMOUNT" fake mkdir --path "$remote/dir" "$FAKE_DIR"
"$PDMOUNT" fake create --path "$remote/dir/b.txt" --from "$work/b.txt" "$FAKE_DIR"
"$PDMOUNT" fake mkdir --path "$remote/deep" "$FAKE_DIR"
"$PDMOUNT" fake mkdir --path "$remote/deep/sub" "$FAKE_DIR"
"$PDMOUNT" fake create --path "$remote/deep/sub/hidden.txt" --from "$work/hidden.txt" "$FAKE_DIR"
event_barrier
wait_visible "$area/dir/b.txt" 120
# No `ls` of deep: its listing would prefetch sub's (§5.3), and hidden.txt must stay unseen.

step "their signatures stop verifying"
log_mark
for mark in "${marks[@]}"; do
    read -r part path <<<"$mark"
    "$PDMOUNT" fake unverified --part "$part" --signer "stranger@example.test" --path "$path" "$FAKE_DIR"
done
wait_for "a.txt in the list" 120 listed "$remote/a.txt"
wait_for "dir in the list" 60 listed "$remote/dir"
expect_log 'nodes whose signatures do not verify' "the daemon's log"
not_listed hidden.txt || fail "hidden.txt was found before its folder was listed"

step "listed and stat, but refused with EACCES"
ls "$area" | grep -qx a.txt || fail "a.txt is not listed"
stat "$area/a.txt" >/dev/null || fail "stat a.txt"
expect_eacces "cat a.txt" cat "$area/a.txt"
expect_eacces "mv a.txt" mv "$area/a.txt" "$area/renamed.txt"
expect_eacces "rm a.txt" rm -f "$area/a.txt"
expect_eacces "a create in dir" touch "$area/dir/new.txt"
expect_eacces "a copy into dir" cp "$work/b.txt" "$area/dir/copied.txt"
printf local >"$area/local.txt"
expect_eacces "a move into dir" mv "$area/local.txt" "$area/dir/local.txt"
expect_eacces "ls dir" ls "$area/dir"
[[ -e $area/a.txt && ! -e $area/renamed.txt ]] || fail "a refused change took effect"
status_text | grep -Eq '^Signatures: 2 unverified, 2 refused \(pdmount signatures list\)$' ||
    fail "status: $(status_text | grep -i signatures)"

step "check finds the node in the folder never listed"
out=$(pd signatures check "$area" 2>&1) || fail "check: $out"
echo "$out"
grep -Eq '^Checked [0-9]+ folders and [0-9]+ nodes in 1 tree: 3 with unverified signatures\.$' <<<"$out" ||
    fail "check summary: $out"
pd signatures list | grep -Fq "$remote/deep/sub/hidden.txt [name unverified]" ||
    fail "hidden.txt is not listed with its name marked: $(pd signatures list)"

step "accept the file: it opens; the folder stays refused"
id=$(id_of /a.txt)
pd signatures accept "$id"
expect_eq "a.txt" "$(cat "$area/a.txt")" "content a"
expect_eacces "a create in dir" touch "$area/dir/new.txt"
pd signatures accept 00000000 >/dev/null 2>&1 && fail "an unknown id was accepted"

step "a remount keeps the found nodes and the accept"
pd unmount "$MNT"
mount_now
expect_eq "a.txt" "$(cat "$area/a.txt")" "content a"
expect_eacces "a create in dir" touch "$area/dir/new.txt"
pd signatures list | grep -Fq "$remote/deep/sub/hidden.txt" || fail "hidden.txt is gone after the remount"

step "once they verify again, nothing is listed and everything opens"
unmark_all
wait_for "an empty list" 120 list_empty
ls "$area/dir" >/dev/null || fail "ls dir after it verifies"
cat "$area/deep/sub/hidden.txt" >/dev/null
rm -rf "$area"
check_hot_path
echo "x18: PASS"
