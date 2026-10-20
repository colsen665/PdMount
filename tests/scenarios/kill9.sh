#!/usr/bin/env bash
# M1 hang safety (DESIGN.md §14.4, §16): kill -9 the daemon while mounted,
# `unmount --force`, mount again and read; no reboot.
source "$(dirname "$0")/lib.sh"
ensure_dataset
VERIFY_MODE=live prepare_mount

step "kill -9 the daemon"
cat "$REMOTE/big/5MiB.bin" >/dev/null
pid=$(pd status | sed -n 's/.*daemon pid \([0-9]*\).*/\1/p')
[[ -n "$pid" ]] || fail "no daemon pid in pdmount status"
kill -9 "$pid"
sleep 1
if ls "$MNT" >/dev/null 2>&1; then fail "the mount still answers after kill -9"; fi
pd status | grep -q 'Mount: *stale' || fail "status does not report a stale mount"
echo "mount is stale, as expected"

step "unmount --force and mount again"
pd unmount --force "$MNT"
pd mount "$MNT"
expected=$(grep ' big/5MiB.bin$' "$LOCAL/MANIFEST.sha256" | cut -d' ' -f1)
actual=$(sha256sum "$REMOTE/big/5MiB.bin" | cut -d' ' -f1)
[[ "$actual" == "$expected" ]] || fail "5MiB.bin checksum after remount: $actual"
check_hot_path
echo "kill -9 recovery: PASS"
