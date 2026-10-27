#!/usr/bin/env bash
# Client-side read throughput and latency on a fake-backend mount at several RAM-cache
# sizes (cache.ram_bytes, DESIGN.md §6.2). Prints MiB/s and p50/p99 per read for a
# sequential 64 MiB read (1 MiB reads), random 128 KiB reads and random 128 KiB mmap reads.
#
# For each size the mount is restarted, which empties the RAM cache and the kernel page
# cache, and the file is read in three states:
#   download  the first pass of the run, the blocks come from the fake drive (first size only)
#   disk      after the restart: RAM and page cache empty, the disk cache holds the blocks
#   ram       again on the same mount, the kernel page cache dropped before each test
#             (posix_fadvise DONTNEED), so the RAM cache answers as far as it holds the blocks
#
# Usage: scripts/bench/read.sh [ram_mib ...]   (default: 16 32 256; at least 16)
#   SCENARIO_ROOT  where the profile state lives (default: /dev/shm/pdmount-bench-read)
#   FAKE_DIR       the fake drive (default /dev/shm/pdmount-bench-read-fake)
#   FILE_MIB       size of the test file (default 64)
# Needs a release build with the fake backend:
#   cargo build --release -p pdmount --features fake-backend
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
sizes=("$@")
[[ ${#sizes[@]} -gt 0 ]] || sizes=(16 32 256)
file_mib=${FILE_MIB:-64}
export BACKEND=fake VERIFY_MODE=live
export SCENARIO_ROOT=${SCENARIO_ROOT:-/dev/shm/pdmount-bench-read}
export FAKE_DIR=${FAKE_DIR:-/dev/shm/pdmount-bench-read-fake}
export PDMOUNT=${PDMOUNT:-$repo/target/release/pdmount}
export PROFILE=${PROFILE:-fake-bench-read}
# shellcheck source=../../tests/scenarios/lib.sh
source "$repo/tests/scenarios/lib.sh"
if [[ ! -f "$FAKE_DIR/.seeded-$file_mib" ]]; then
    if is_mounted; then pd unmount "$MNT" >/dev/null; fi
    seed=$(mktemp -d)
    mkdir -p "$seed/pdmount-test/bench-read"
    head -c "$((file_mib << 20))" /dev/urandom >"$seed/pdmount-test/bench-read/data.bin"
    rm -rf "$FAKE_DIR"
    "$PDMOUNT" fake seed --from "$seed" "$FAKE_DIR" >/dev/null
    rm -rf "$seed"
    touch "$FAKE_DIR/.seeded-$file_mib"
    pd reset --keep-login >/dev/null
fi
file=$MNT/pdmount-test/bench-read/data.bin
config=$XDG_CONFIG_HOME/pdmount/$PROFILE.toml
remount() {
    if is_mounted; then pd unmount "$MNT" >/dev/null; fi
    mkdir -p "$MNT"
    mount_now >/dev/null
}
first=1
for mib in "${sizes[@]}"; do
    sed -i "s/^ram_bytes = .*/ram_bytes = \"${mib}MiB\"/" "$config"
    remount
    if ((first)); then
        python3 -I "$here/read.py" "$file" "ram=${mib}MiB download" drop
        first=0
        remount
    fi
    python3 -I "$here/read.py" "$file" "ram=${mib}MiB disk" drop
    python3 -I "$here/read.py" "$file" "ram=${mib}MiB ram" drop
done
pd unmount "$MNT" >/dev/null
