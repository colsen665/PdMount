#!/usr/bin/env bash
# X19 (DESIGN.md §14.2, §14.5, M6.0.10): `cache wipe` reaches the block cache
# of every mount. One daemon on a fake drive of its own with three trees:
# My Files (two files and a pinned one), a Photos root (three photos in two
# months of two years) and a Shared root (the share `Team`, and the shared
# file `plan.txt`). Checks that a path in Photos (a photo, a month, a year) or
# in a share (a folder, the files folder) wipes the blocks at or below it in
# that tree and no other, that no path wipes every tree and the reply names
# what each freed, that pinned blocks stay, that a path outside every mount
# and one that names nothing are refused, and that what was wiped reads
# again. Blocks are counted from the reply: every file here is one block.
# Fake, live mode only, its own profile and fake drive.
source "$(dirname "$0")/lib.sh"
[[ $BACKEND == fake ]] || { echo "x19 needs a fake drive of its own; skipped for BACKEND=$BACKEND"; exit 0; }
x_live_only
make_workdir

PROFILE=fake-x19
MNT=$SCENARIO_ROOT/fake-mnt-x19
PHOTOS=$SCENARIO_ROOT/fake-photos-x19
SHARED=$SCENARIO_ROOT/fake-shared-x19
FAKE_DIR=$SCENARIO_ROOT/fake-drive-x19
backend_flag=(--backend "fake:$FAKE_DIR")
config_file=$XDG_CONFIG_HOME/pdmount/$PROFILE.toml
# Two photos in one month of 2024, one in 2023 (mid-month, so no time zone moves them).
JUNE_2024=1718452800
MAY_2023=1684152000

x19_cleanup() {
    if is_mounted; then pd unmount "$MNT" >/dev/null 2>&1 || true; fi
    for mnt in "$PHOTOS" "$SHARED"; do
        if mounted_at "$mnt"; then fusermount3 -u "$mnt" 2>/dev/null || fusermount3 -uz "$mnt" || true; fi
    done
    scenario_cleanup
}
trap x19_cleanup EXIT

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
root_line() { status_has "^Root: +$1 \\($2\\): $3\$"; }
names() { ls -1 "$1" | tr '\n' '|'; }
lists() { [[ $(names "$1") == "$2" ]]; } # <dir> <names>
month_of() { date -d "@$1" +%Y/%m; }
pin_fetched() { [[ $(pinned_bytes) =~ ^[1-9] ]]; }
pinned_bytes() { pd cache stats | sed -n 's/^Pinned: *\(.*\) (not counted.*/\1/p'; }

# What a wipe reported for a tree, from its output: `  <name>: <size> in <n> blocks`.
tree_blocks() { # <output> <name>
    sed -n "s|^  $2: .* in \\([0-9]*\\) blocks\$|\\1|p" <<<"$1"
}
total_blocks() { sed -n 's/^Freed .* in \([0-9]*\) blocks\.$/\1/p' <<<"$1"; }
# A wipe that must work: its output.
wipe() {
    local out
    out=$(pd cache wipe "$@" 2>&1) || fail "cache wipe $*: $out"
    printf '%s\n' "$out"
}
wipe_fails() { # <what> <message regex> <path>
    local out
    if out=$(pd cache wipe "$3" 2>&1); then fail "$1: the wipe worked: $out"; fi
    grep -Eq "$2" <<<"$out" || fail "$1: the error does not match '$2': $out"
}

# Reads every file of the three trees into the caches (My Files' pinned one is
# fetched by its pin). O_DIRECT, so that the kernel's page cache never answers
# a read for which the daemon's cache was wiped.
read_all() {
    local file
    for file in "$MNT/pdmount-test/f1" "$MNT/pdmount-test/f2" \
        "$PHOTOS/$M1/P1.jpg" "$PHOTOS/$M1/P2.jpg" "$PHOTOS/$M3/P3.jpg" \
        "$SHARED/Team/a.bin" "$SHARED/Team/sub/b.bin" "$SHARED/Shared files/plan.txt"; do
        dd if="$file" of=/dev/null bs=1M iflag=direct status=none || fail "reading $file"
    done
}

step "a fake drive of its own: My Files, three photos, the share Team and the shared file plan.txt"
if is_mounted; then pd unmount "$MNT"; fi
for mnt in "$PHOTOS" "$SHARED"; do
    if mounted_at "$mnt"; then fusermount3 -u "$mnt"; fi
done
remove_fake_dir "$FAKE_DIR"
M1=$(month_of $((JUNE_2024 - 3600)))
M3=$(month_of "$MAY_2023")
Y1=${M1%/*}
Y3=${M3%/*}
[[ $Y1 != "$Y3" ]] || fail "the test's photos are not in two years"
mkdir -p "$work/seed/pdmount-test" "$work/team/sub"
head -c $((300 * 1024)) /dev/urandom >"$work/seed/pdmount-test/f1"
head -c $((200 * 1024)) /dev/urandom >"$work/seed/pdmount-test/f2"
head -c $((100 * 1024)) /dev/urandom >"$work/seed/pdmount-test/pinned.bin"
head -c $((120 * 1024)) /dev/urandom >"$work/P1.jpg"
head -c $((130 * 1024)) /dev/urandom >"$work/P2.jpg"
head -c $((140 * 1024)) /dev/urandom >"$work/P3.jpg"
head -c $((50 * 1024)) /dev/urandom >"$work/team/a.bin"
head -c $((60 * 1024)) /dev/urandom >"$work/team/sub/b.bin"
printf 'the plan\n' >"$work/plan.txt"
"$PDMOUNT" fake seed --from "$work/seed" "$FAKE_DIR"
touch "$FAKE_DIR/.seeded-x19"
"$PDMOUNT" fake photo add --name P1.jpg --from "$work/P1.jpg" --captured $((JUNE_2024 - 3600)) "$FAKE_DIR" >/dev/null
"$PDMOUNT" fake photo add --name P2.jpg --from "$work/P2.jpg" --captured $((JUNE_2024 - 7200)) "$FAKE_DIR" >/dev/null
"$PDMOUNT" fake photo add --name P3.jpg --from "$work/P3.jpg" --captured "$MAY_2023" "$FAKE_DIR" >/dev/null
"$PDMOUNT" fake root --name Team --owner alice --role viewer --from "$work/team" "$FAKE_DIR" >/dev/null
"$PDMOUNT" fake share-file --name plan.txt --owner bob --from "$work/plan.txt" "$FAKE_DIR" >/dev/null
rm -f "$config_file"
fake_config "$PROFILE" dev-x19 "$MNT"
pd reset --keep-login >/dev/null
rm -f "$XDG_DATA_HOME/pdmount/$PROFILE/shared-names.json"
cat >>"$config_file" <<EOF

[[roots]]
kind = "photos"
mountpoint = "$PHOTOS"

[[roots]]
kind = "shared"
mountpoint = "$SHARED"
EOF

step "mount: My Files, Photos and Shared; the months' names, the pin"
mount_now
wait_for "the Photos root" 30 root_line "$PHOTOS" photos "mounted, read-only"
wait_for "the Shared root" 30 root_line "$SHARED" shared mounted
wait_for "the names of $M1" 30 lists "$PHOTOS/$M1" "P1.jpg|P2.jpg|"
wait_for "the names of $M3" 30 lists "$PHOTOS/$M3" "P3.jpg|"
wait_for "the share" 30 lists "$SHARED" "Shared files|Team|"
pd pin "$MNT/pdmount-test/pinned.bin" >/dev/null
wait_for "the pinned file" 30 pin_fetched
pinned=$(pinned_bytes)
read_all

step "a photo of the Photos tree: only its block, in Photos"
out=$(wipe "$PHOTOS/$M3/P3.jpg")
expect_eq "the photo's blocks" "$(tree_blocks "$out" "$PHOTOS")" 1
expect_eq "the total" "$(total_blocks "$out")" 1
grep -q 'My Files\|Team' <<<"$out" && fail "the wipe named other trees: $out"
out=$(wipe "$PHOTOS/$M3/P3.jpg")
expect_eq "the photo again" "$(total_blocks "$out")" 0

step "a month and a year of Photos wipe the photos below them"
read_all
out=$(wipe "$PHOTOS/$M1")
expect_eq "the month's blocks" "$(tree_blocks "$out" "$PHOTOS")" 2
expect_eq "the total" "$(total_blocks "$out")" 2
out=$(wipe "$PHOTOS/$Y3")
expect_eq "the other year's blocks" "$(tree_blocks "$out" "$PHOTOS")" 1
read_all
out=$(wipe "$PHOTOS/$Y1")
expect_eq "the year's blocks" "$(tree_blocks "$out" "$PHOTOS")" 2
out=$(wipe "$PHOTOS")
expect_eq "the rest of Photos" "$(tree_blocks "$out" "$PHOTOS")" 1

step "a path in a share, and the files folder"
read_all
out=$(wipe "$SHARED/Team/sub")
expect_eq "the folder's blocks" "$(tree_blocks "$out" "$SHARED/Team")" 1
expect_eq "the total" "$(total_blocks "$out")" 1
out=$(wipe "$SHARED/Shared files")
expect_eq "the shared file's blocks" "$(tree_blocks "$out" "$SHARED/Shared files/plan.txt")" 1
expect_eq "the total" "$(total_blocks "$out")" 1
out=$(wipe "$SHARED/Team/a.bin")
expect_eq "the file's blocks" "$(tree_blocks "$out" "$SHARED/Team")" 1

step "My Files: a relative path, an absolute one"
read_all
out=$(wipe pdmount-test/f1)
expect_eq "relative" "$(total_blocks "$out")" 1
out=$(wipe "$MNT/pdmount-test/f2")
expect_eq "absolute" "$(total_blocks "$out")" 1

step "no path: every tree's cache, named in the reply; the pinned file stays"
read_all
out=$(wipe)
echo "$out"
expect_eq "My Files" "$(tree_blocks "$out" "My Files")" 2
expect_eq "Photos" "$(tree_blocks "$out" "$PHOTOS")" 3
expect_eq "the share" "$(tree_blocks "$out" "$SHARED/Team")" 2
expect_eq "the shared file" "$(tree_blocks "$out" "$SHARED/Shared files/plan.txt")" 1
expect_eq "the total" "$(total_blocks "$out")" 8
grep -Eq 'left of .* in all 4 trees\.$' <<<"$out" || fail "the profile's total is missing: $out"
expect_eq "the pinned bytes" "$(pinned_bytes)" "$pinned"
out=$(wipe)
expect_eq "a second wipe" "$(total_blocks "$out")" 0

step "refused: a path outside every mount, and paths that name nothing"
wipe_fails "outside" "outside every mount" "$SCENARIO_ROOT/outside"
wipe_fails "the parent of a mount" "outside every mount" "$(dirname "$MNT")"
wipe_fails "an unknown share" "no tree named Nope" "$SHARED/Nope"
wipe_fails "an unknown shared file" "no shared file named nope" "$SHARED/Shared files/nope"
wipe_fails "an unknown year" "no such file or directory" "$PHOTOS/1999"
wipe_fails "an unknown file in a share" "no such file or directory" "$SHARED/Team/nope"
wipe_fails "dots" '\.\.' "$PHOTOS/../x"

step "what was wiped reads again; the pinned file too"
cmp -s "$work/seed/pdmount-test/f1" "$MNT/pdmount-test/f1" || fail "f1 differs after the wipe"
cmp -s "$work/seed/pdmount-test/pinned.bin" "$MNT/pdmount-test/pinned.bin" || fail "the pinned file differs"
cmp -s "$work/P3.jpg" "$PHOTOS/$M3/P3.jpg" || fail "P3.jpg differs after the wipe"
cmp -s "$work/team/sub/b.bin" "$SHARED/Team/sub/b.bin" || fail "b.bin differs after the wipe"
expect_eq "plan.txt" "$(cat "$SHARED/Shared files/plan.txt")" "the plan"

step "the daemon's stop unmounts the roots too"
pd unmount "$MNT" >/dev/null
mounted_at "$PHOTOS" && fail "$PHOTOS is still mounted after the daemon's stop"
mounted_at "$SHARED" && fail "$SHARED is still mounted after the daemon's stop"
mounted_at "$MNT" && fail "$MNT is still mounted"
echo "x19: PASS"
