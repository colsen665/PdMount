#!/usr/bin/env bash
# G6 (DESIGN.md §15.3): a bare repository in the mount; push from outside
# (quarantine migration = cross-directory renames), clone by path and by
# file:// URL, push more, fetch. Everything fscks clean.
source "$(dirname "$0")/lib.sh"
prepare_mount
git_env
make_workdir
scenario_area g6
git init -q --bare -b main "$area/r.git"
git init -q -b main "$work/local"
for i in $(seq 1 20); do
    head -c $((i * 3001)) /dev/urandom >"$work/local/f$i"
    git -C "$work/local" add -A
    git -C "$work/local" commit -q -m "c$i"
done

step "G6 [$VERIFY_MODE]: push, clone, push, fetch"
git -C "$work/local" push -q "$area/r.git" main
git clone -q "$area/r.git" "$work/clone-path"
for i in $(seq 21 30); do
    printf '%d\n' "$i" >"$work/local/f$i"
    git -C "$work/local" add -A
    git -C "$work/local" commit -q -m "c$i"
done
git -C "$work/local" push -q "$area/r.git" main
git -C "$work/clone-path" fetch -q origin
verify_cycle
bare=$TEST_ROOT/g6/r.git
git -C "$bare" fsck --full --strict
git clone -q "file://$bare" "$work/clone-url"
for clone in clone-url; do
    [[ "$(git -C "$work/$clone" rev-parse HEAD)" == "$(git -C "$work/local" rev-parse HEAD)" ]] ||
        fail "$clone has the wrong HEAD"
done
[[ "$(git -C "$work/clone-path" rev-parse origin/main)" == "$(git -C "$work/local" rev-parse HEAD)" ]] ||
    fail "fetch did not see the second push"
check_hot_path
echo "G6 [$VERIFY_MODE]: PASS"
