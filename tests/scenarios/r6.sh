#!/usr/bin/env bash
# R6 (DESIGN.md §15.3): `rsync -a M/dst/ out/`, then again. The second run
# transfers nothing, and out/ equals the source by checksum.
source "$(dirname "$0")/lib.sh"
ensure_dataset
prepare_mount
make_workdir

step "R6 [$VERIFY_MODE]: rsync -a from the mount"
time rsync -a "$REMOTE/src/" "$work/out/"
second=$(rsync -ai "$REMOTE/src/" "$work/out/")
[[ -z "$second" ]] || fail "the second rsync run transferred: $second"
echo "second run: nothing transferred"
diff_out=$(rsync -rnci "$LOCAL/src/" "$work/out/")
[[ -z "$diff_out" ]] || fail "out/ differs from the source by checksum: $diff_out"
echo "out/ equals the source by checksum"
check_hot_path
echo "R6 [$VERIFY_MODE]: PASS"
