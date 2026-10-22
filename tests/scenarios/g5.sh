#!/usr/bin/env bash
# G5 (DESIGN.md §15.3): switch between divergent branches (mass rewrite,
# deletions, mode flips); `git status` stays clean and modes are kept.
source "$(dirname "$0")/lib.sh"
prepare_mount
git_env
scenario_area g5
repo=$area/repo
git init -q -b main "$repo"
for i in $(seq 1 80); do printf 'main %d\n' "$i" >"$repo/f$i"; done
chmod +x "$repo/f1" "$repo/f2"
git -C "$repo" add -A
git -C "$repo" commit -q -m main
git -C "$repo" checkout -q -b other
for i in $(seq 1 80); do printf 'other %d\n' "$i" >"$repo/f$i"; done
git -C "$repo" rm -q -f f7{0..9}
chmod -x "$repo/f1"
chmod +x "$repo/f3"
git -C "$repo" add -A
git -C "$repo" commit -q -m other

step "G5 [$VERIFY_MODE]: switch branches"
for _ in 1 2 3; do
    git -C "$repo" checkout -q main
    git -C "$repo" checkout -q other
done
git -C "$repo" checkout -q main
verify_cycle
repo=$TEST_ROOT/g5/repo
[[ -z "$(git -C "$repo" status --porcelain)" ]] || fail "the repository is not clean: $(git -C "$repo" status --porcelain | head)"
[[ -x "$repo/f1" && -x "$repo/f2" && ! -x "$repo/f3" ]] || fail "modes were not kept"
git -C "$repo" checkout -q other
[[ -z "$(git -C "$repo" status --porcelain)" ]] || fail "other is not clean"
[[ ! -e "$repo/f75" && -x "$repo/f3" && ! -x "$repo/f1" ]] || fail "the other branch is wrong"
git -C "$repo" fsck --full --strict
check_hot_path
echo "G5 [$VERIFY_MODE]: PASS"
