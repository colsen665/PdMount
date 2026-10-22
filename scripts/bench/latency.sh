#!/usr/bin/env bash
# Client-side latency of namespace operations on a fake-backend mount, against
# the DESIGN.md §5.7 SLOs. Prints p50/p99/max in µs per operation.
#
# Usage: scripts/bench/latency.sh [clients] [iterations]   (default: 4 1000)
#   SCENARIO_ROOT  where the profile state lives (default: tmpfs, see
#                  tests/scenarios/lib.sh); use a directory on the disk to
#                  measure the disk, e.g. ~/.cache/pdmount-bench
#   FAKE_DIR       the fake drive; keep it on tmpfs (default
#                  /dev/shm/pdmount-bench-fake), since a real backend writes
#                  nothing locally
# Needs a release build with the fake backend:
#   cargo build --release -p pdmount --features fake-backend
set -euo pipefail
clients=${1:-4} iterations=${2:-1000}
here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
export BACKEND=fake VERIFY_MODE=live
export SCENARIO_ROOT=${SCENARIO_ROOT:-/dev/shm/pdmount-bench}
export FAKE_DIR=${FAKE_DIR:-/dev/shm/pdmount-bench-fake}
export PDMOUNT=${PDMOUNT:-$repo/target/release/pdmount}
export PROFILE=${PROFILE:-fake-bench}
# shellcheck source=../../tests/scenarios/lib.sh
source "$repo/tests/scenarios/lib.sh"
if [[ ! -f "$FAKE_DIR/.seeded" ]]; then
    empty=$(mktemp -d)
    rm -rf "$FAKE_DIR"
    "$PDMOUNT" fake seed --from "$empty" "$FAKE_DIR" >/dev/null
    rmdir "$empty"
    touch "$FAKE_DIR/.seeded"
    pd reset --keep-login >/dev/null
fi
if is_mounted; then pd unmount "$MNT" >/dev/null; fi
mkdir -p "$MNT"
mount_now >/dev/null
area=$MNT/pdmount-test/bench-$$
out=$(mktemp -d)
mkdir -p "$area"
for i in $(seq 1 "$clients"); do
    python3 -I "$here/ops.py" "$area/c$i" "$iterations" "$out/$i.json" &
done
wait
python3 -I "$here/report.py" "${clients}x$iterations" "$out"/*.json
rm -rf "$out"
wait_queue_empty >/dev/null
rm -rf "$area"
wait_queue_empty >/dev/null
pd unmount "$MNT" >/dev/null
