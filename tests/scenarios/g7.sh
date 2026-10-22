#!/usr/bin/env bash
# G7 (DESIGN.md §15.3): the round trip after G3-G5. A repository with
# commits, renames, mode changes and branch switches is verified from a
# fresh state: fsck, status and HEAD match. Runs the fresh cycle in every
# VERIFY_MODE.
source "$(dirname "$0")/lib.sh"
prepare_mount
git_env
scenario_area g7
repo=$area/repo
git init -q -b main "$repo"
for i in $(seq 1 60); do
    printf 'change %d\n' "$i" >"$repo/f$((i % 20))"
    if ((i % 10 == 0)) && [[ -e "$repo/f$((i % 20 + 1))" ]]; then git -C "$repo" mv "f$((i % 20 + 1))" "moved-$i"; fi
    if ((i % 12 == 0)); then chmod +x "$repo/f$((i % 20))"; fi
    git -C "$repo" add -A
    git -C "$repo" commit -q -m "c$i"
done
git -C "$repo" checkout -q -b side
printf 'side\n' >"$repo/side.txt"
git -C "$repo" add -A
git -C "$repo" commit -q -m side
git -C "$repo" checkout -q main
head=$(git -C "$repo" rev-parse HEAD)

step "G7 [$VERIFY_MODE]: round trip through a fresh state"
VERIFY_MODE=fresh verify_cycle
repo=$TEST_ROOT/g7/repo
git -C "$repo" fsck --full --strict
[[ -z "$(git -C "$repo" status --porcelain)" ]] || fail "the repository is not clean"
[[ "$(git -C "$repo" rev-parse HEAD)" == "$head" ]] || fail "HEAD differs"
git -C "$repo" checkout -q side
[[ -f "$repo/side.txt" ]] || fail "the side branch is incomplete"
check_hot_path
echo "G7 [$VERIFY_MODE]: PASS"
