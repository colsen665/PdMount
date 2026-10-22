#!/usr/bin/env bash
# A1 (DESIGN.md §15.3): `pdmount access set ro <dir>` while mounted. Every
# change in the subtree gives EROFS, `stat` shows no write bits, `rsync -a`
# into it reports "Read-only file system", and `git commit` in a repository
# inside it fails cleanly.
source "$(dirname "$0")/lib.sh"
source "$(dirname "$0")/access-lib.sh"
prepare_mount
git_env
make_workdir
scenario_area a1
dir=$area/dir
mkdir -p "$dir/sub"
printf 'content\n' >"$dir/file"
chmod 0640 "$dir/file"
git init -q -b main "$dir/repo"
printf 'x\n' >"$dir/repo/x"
git -C "$dir/repo" add -A
git -C "$dir/repo" commit -q -m one
mkdir -p "$work/src" && printf 'new\n' >"$work/src/new"

step "A1 [$VERIFY_MODE]: access set ro"
pd access set ro "$dir"
check_denied() {
    expect_erofs create touch "$dir/new"
    expect_erofs write sh -c "printf more >>'$dir/file'"
    expect_erofs truncate truncate -s 0 "$dir/file"
    expect_erofs unlink rm -f "$dir/file"
    expect_erofs rmdir rmdir "$dir/sub"
    expect_erofs rename mv "$dir/file" "$dir/file2"
    expect_erofs chmod chmod 0600 "$dir/file"
    expect_erofs touch touch -d @1 "$dir/file"
    expect_erofs mkdir mkdir "$dir/newdir"
    [[ "$(stat -c %a "$dir/file")" == 440 ]] || fail "file shows mode $(stat -c %a "$dir/file"), expected 440"
    [[ "$(stat -c %a "$dir")" == 555 ]] || fail "dir shows mode $(stat -c %a "$dir"), expected 555"
    out=$(rsync -a "$work/src/" "$dir/sub/" 2>&1) && fail "rsync into the read-only subtree succeeded"
    grep -qi 'read-only file system' <<<"$out" || fail "rsync failed differently: $out"
    if out=$(git -C "$dir/repo" commit -q --allow-empty -m two 2>&1); then
        fail "git commit succeeded in the read-only subtree"
    fi
    grep -qi 'read-only file system' <<<"$out" || fail "git commit failed differently: $out"
    [[ "$(git -C "$dir/repo" rev-list --count HEAD)" == 1 ]] || fail "the repository changed"
}
check_denied
verify_cycle
dir=$TEST_ROOT/a1/dir
if rules_kept; then
    check_denied
    pd access unset "$dir"
fi
[[ "$(cat "$dir/file")" == content ]] || fail "the file changed"
check_hot_path
echo "A1 [$VERIFY_MODE]: PASS"
