#!/usr/bin/env bash
# fsx with mmap reads and writes (DESIGN.md §15.1) over the fake backend; never
# the real account (D030). The M2 exit check runs it for 6 h.
#
# Usage: tests/fsx.sh
#   FSX           fsx binary (required), built from xfstests' ltp/fsx.c
#   FSX_DURATION  seconds (default: 21600, 6 h)
#   FSX_SEED      seed of the first instance (default: from the clock; printed)
#
# Two instances run at once on their own files: one with fsx's defaults
# (256 KiB files, 64 KiB operations), one with 8 MiB files and 256 KiB
# operations, so writes, truncations and mapped I/O cross 4 MiB blocks.
# fallocate, clone and dedupe ranges are unsupported and disabled by fsx
# itself. `-c` (close and reopen) is not used: it writes
# /proc/sys/vm/drop_caches, which needs root.
BACKEND=fake
source "$(dirname "$0")/scenarios/lib.sh"

# Local tool locations (AGENTS.md, "Local paths").
[[ -f "$repo/.pdmount-local.env" ]] && source "$repo/.pdmount-local.env"

FSX=${FSX:?set FSX to the fsx binary; see the header of this script}
FSX_DURATION=${FSX_DURATION:-21600}
FSX_SEED=${FSX_SEED:-$(date +%s)}
[[ -x "$FSX" ]] || fail "no fsx at $FSX; build xfstests' ltp/fsx.c (TODO.md M2.9)"

prepare_mount
area=$TEST_ROOT/fsx
rm -rf "$area"
mkdir -p "$area"
logs=$SCENARIO_ROOT/fsx-logs
rm -rf "$logs"
mkdir -p "$logs"

step "fsx: 2 instances for $FSX_DURATION s, seeds $FSX_SEED and $((FSX_SEED + 1)), logs in $logs"
"$FSX" -q -S "$FSX_SEED" --duration="$FSX_DURATION" -P "$logs" "$area/small" >"$logs/small.out" 2>&1 &
small=$!
"$FSX" -q -S "$((FSX_SEED + 1))" --duration="$FSX_DURATION" -l 8388608 -o 262144 -P "$logs" \
    "$area/large" >"$logs/large.out" 2>&1 &
large=$!
status=0
wait "$small" || status=1
wait "$large" || status=1
for name in small large; do
    echo "$name: $(grep -E 'operations completed|^[A-Za-z].*(ERROR|error|failed)' "$logs/$name.out" | tail -1)"
done
((status == 0)) || fail "fsx reported an error; see $logs"

pd stats | awk '$1 == "handler_panics" || $1 == "deadline_expired" { print; if ($2 != 0) bad = 1 }
    END { exit bad }' || fail "the daemon counted panics or expired deadlines"
check_hot_path
rm -rf "$area"
echo "fsx: PASS"
