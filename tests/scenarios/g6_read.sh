#!/usr/bin/env bash
# G6, read side (DESIGN.md §15.3): clone the bare repository from the mount,
# by path and by file:// URL, and fsck the clones.
source "$(dirname "$0")/lib.sh"
ensure_dataset
prepare_mount
require_m1
verify_cycle
make_workdir
expected=$(cat "$LOCAL/HEAD-COMMIT")

step "G6 [$VERIFY_MODE]: clone and fsck"
git clone -q "$REMOTE/r.git" "$work/path-clone"
git clone -q "file://$REMOTE/r.git" "$work/url-clone"
for clone in path-clone url-clone; do
    git -C "$work/$clone" fsck --full --strict
    head=$(git -C "$work/$clone" rev-parse HEAD)
    [[ "$head" == "$expected" ]] || fail "$clone HEAD is $head, expected $expected"
    [[ -z "$(git -C "$work/$clone" status --porcelain)" ]] || fail "$clone is not clean"
    echo "$clone: fsck clean, HEAD $head"
done
git -C "$REMOTE/r.git" fsck --full --strict --no-dangling
echo "r.git on the mount: fsck clean"
check_hot_path
echo "G6 read [$VERIFY_MODE]: PASS"
