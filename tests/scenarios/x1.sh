#!/usr/bin/env bash
# X1 (DESIGN.md §15.3, D075): files written just before SIGTERM, within the
# upload debounce, are all on the drive before the daemon exits, well under
# the grace. Fake with the network profile.
source "$(dirname "$0")/lib.sh"
[[ $BACKEND == fake ]] || { echo "x1 needs the fake's network profile; skipped for BACKEND=$BACKEND"; exit 0; }
x_live_only
ensure_dataset
prepare_mount
make_workdir
scenario_area x1
x_config_save
trap 'x_restore; scenario_cleanup' EXIT

step "a debounce of a minute, so only the flush uploads"
x_config '[upload]' 'debounce = "60s"'
pd unmount "$MNT"
mount_now
"$PDMOUNT" fake profile --rtt 80 --up 20000000 --down 20000000 "$FAKE_DIR"

step "write files, SIGTERM at once"
log_mark
write_files "$area" x1 20 16
head -c 3145728 /dev/urandom >"$work/x1/big"
cp "$work/x1/big" "$area/big"
# A shared writable mapping dirtied and kept, never synced: its pages are in
# the kernel until the flush's syncfs writes them back (DESIGN §14.4).
head -c 65536 /dev/zero >"$area/mapped"
head -c 65536 /dev/urandom >"$work/x1/mapped"
python3 -I - "$area/mapped" "$work/x1/mapped" "$work/mapped.ready" <<'PY' &
import mmap, os, sys, time
path, data_path, ready = sys.argv[1:4]
data = open(data_path, "rb").read()
fd = os.open(path, os.O_RDWR)
mapping = mmap.mmap(fd, len(data), mmap.MAP_SHARED, mmap.PROT_READ | mmap.PROT_WRITE)
os.close(fd)
mapping[:] = data
open(ready, "w").close()
deadline = time.time() + 300
while os.path.exists(ready) and time.time() < deadline:
    time.sleep(0.1)
PY
holder=$!
until [[ -e $work/mapped.ready ]]; do
    kill -0 "$holder" 2>/dev/null || fail "could not map $area/mapped"
    sleep 0.1
done
pending=$(status_text | sed -n 's/^Queue: *\([0-9]*\) pending.*/\1/p')
((${pending:-0} > 0)) || fail "nothing is queued before SIGTERM: $(status_text | grep '^Queue:')"
term_daemon
# The mapping keeps the mount busy: the daemon detaches it and waits up to 10 s for the session.
kill -9 "$holder" 2>/dev/null || true
wait "$holder" 2>/dev/null || true
((stop_ms < 30000)) || fail "the stop took $stop_ms ms; the grace is 90 s and the flush should take seconds"
expect_log 'flushed [0-9]+ entr(y|ies) in' "the flush"

step "everything is on the drive"
"$PDMOUNT" fake profile clear "$FAKE_DIR"
x_restore
expect_on_drive "$area" x1
check_hot_path
echo "x1: PASS"
