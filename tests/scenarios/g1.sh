#!/usr/bin/env bash
# G1 (DESIGN.md §15.3): `git init` in the mount probes the filesystem:
# core.filemode=true, core.symlinks=false, core.ignorecase=false.
source "$(dirname "$0")/lib.sh"
prepare_mount
git_env
scenario_area g1

step "G1 [$VERIFY_MODE]: git init"
git init -q "$area/repo"
verify_cycle
repo=$TEST_ROOT/g1/repo
for setting in core.filemode=true core.symlinks=false core.ignorecase=false; do
    key=${setting%%=*}
    value=$(git -C "$repo" config --get "$key" || echo unset)
    # git leaves core.ignorecase unset when it is false.
    [[ "$value" == "${setting#*=}" || ("$key" == core.ignorecase && "$value" == unset) ]] ||
        fail "$key is $value, expected ${setting#*=}"
done
check_hot_path
echo "G1 [$VERIFY_MODE]: PASS"
