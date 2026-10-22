#!/usr/bin/env bash
# G2 (DESIGN.md §15.3): `git clone` into the mount, from a local path and
# from a file:// URL; `git fsck --full --strict` and `git status` are clean.
source "$(dirname "$0")/lib.sh"
prepare_mount
git_env
make_workdir
scenario_area g2
git init -q -b main "$work/origin"
for i in $(seq 1 "${G2_COMMITS:-30}"); do
    mkdir -p "$work/origin/d$((i % 7))"
    head -c $((i * 997)) /dev/urandom >"$work/origin/d$((i % 7))/f$i"
    git -C "$work/origin" add -A
    git -C "$work/origin" commit -q -m "c$i"
done
head=$(git -C "$work/origin" rev-parse HEAD)

step "G2 [$VERIFY_MODE]: git clone into the mount"
git clone -q "$work/origin" "$area/by-path"
git clone -q "file://$work/origin" "$area/by-url"
verify_cycle
for clone in by-path by-url; do
    repo=$TEST_ROOT/g2/$clone
    git -C "$repo" fsck --full --strict
    [[ -z "$(git -C "$repo" status --porcelain)" ]] || fail "$clone is not clean"
    [[ "$(git -C "$repo" rev-parse HEAD)" == "$head" ]] || fail "$clone has the wrong HEAD"
done
[[ -z "$(remote_temp_names)" ]] || fail "temporary names reached the backend: $(remote_temp_names)"
check_hot_path
echo "G2 [$VERIFY_MODE]: PASS"
