#!/usr/bin/env bash
# G3 (DESIGN.md §15.3): a commit loop inside the mount (adds, changes,
# renames, chmod +x); `git log` hashes are identical in every verify mode.
source "$(dirname "$0")/lib.sh"
prepare_mount
git_env
scenario_area g3
repo=$area/repo
git init -q -b main "$repo"

step "G3 [$VERIFY_MODE]: ${G3_COMMITS:-200} commits"
for i in $(seq 1 "${G3_COMMITS:-200}"); do
    printf 'file %d\n' "$i" >"$repo/f$((i % 40))"
    printf 'line %d\n' "$i" >>"$repo/log.txt"
    if ((i % 10 == 0)) && [[ -e "$repo/f$((i % 40 + 1))" ]]; then
        git -C "$repo" mv "f$((i % 40 + 1))" "renamed-$i"
    fi
    if ((i % 15 == 0)); then chmod +x "$repo/f$((i % 40))"; fi
    git -C "$repo" add -A
    git -C "$repo" commit -q -m "commit $i"
done
log=$(git -C "$repo" log --format=%H)
verify_cycle
repo=$TEST_ROOT/g3/repo
[[ "$(git -C "$repo" log --format=%H)" == "$log" ]] || fail "git log differs after $VERIFY_MODE"
git -C "$repo" fsck --full --strict
[[ -z "$(git -C "$repo" status --porcelain)" ]] || fail "the repository is not clean"
[[ -z "$(remote_temp_names)" ]] || fail "temporary names reached the backend: $(remote_temp_names)"
check_hot_path
echo "G3 [$VERIFY_MODE]: PASS"
