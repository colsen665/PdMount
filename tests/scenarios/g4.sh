#!/usr/bin/env bash
# G4 (DESIGN.md §15.3): `git gc --aggressive`, `git repack -a -d` and
# `git prune` in the mount: a pack over 40 MiB, 0444 modes, packs unlinked
# while mapped, `objects/xx` directories removed. fsck stays clean and
# `git count-objects -v` reports no garbage.
source "$(dirname "$0")/lib.sh"
prepare_mount
git_env
scenario_area g4
repo=$area/repo
git init -q -b main "$repo"
for i in $(seq 1 "${G4_BLOBS:-9}"); do
    head -c $((5 << 20)) /dev/urandom >"$repo/blob$i.bin"
    printf 'small %d\n' "$i" >"$repo/small$i.txt"
    git -C "$repo" add -A
    git -C "$repo" commit -q -m "c$i"
done

step "G4 [$VERIFY_MODE]: gc, repack, prune"
git -C "$repo" gc -q --aggressive
git -C "$repo" repack -q -a -d
git -C "$repo" prune
verify_cycle
repo=$TEST_ROOT/g4/repo
git -C "$repo" fsck --full --strict
counts=$(git -C "$repo" count-objects -v)
echo "$counts"
grep -q '^garbage: 0$' <<<"$counts" || fail "git reports garbage"
grep -q '^count: 0$' <<<"$counts" || fail "loose objects remain after gc"
pack=$(find "$repo/.git/objects/pack" -name '*.pack' -size +40M | head -1)
[[ -n "$pack" ]] || fail "no pack over 40 MiB"
[[ "$(stat -c %a "$pack")" == 444 ]] || fail "the pack is not mode 0444"
check_hot_path
echo "G4 [$VERIFY_MODE]: PASS"
