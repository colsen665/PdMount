#!/usr/bin/env bash
# Runs scenarios in verify modes and prints one line per run (DESIGN.md §15.3).
#
# Usage: tests/scenarios/run-all.sh [scenario ...]
#   SCENARIOS  default when no arguments: every scenario (the M scenarios m1..m7 and e2 run in
#              live mode only and skip the others)
#   MODES      verify modes (default: live remount crash fresh; `second` adds device B, lib.sh)
#   STOP_ON    comma-separated stop conditions (default: 429,fail for the real account, none
#              for BACKEND=fake): 429 stops the matrix after a run whose daemon log lines
#              show an HTTP 429 (the SDK retries 429s, so a run can pass with one; D030).
#              Both profiles' logs are read, since device B may call the API too;
#              fail stops it after the first failed run
#   DAEMON_LOG the daemon log the 429 check reads (default: the profile's log, as lib.sh
#              sets it up)
#   TRASH_MIN_FREE  bytes; on the real account, a run that would start with less storage free
#              unmounts and empties the account's trash first (D036). Default 1073741824
#              (1 GiB); 0 turns it off. Not on the fake
# The environment (BACKEND, sizes, ...) is passed through; see lib.sh.
set -uo pipefail
# A relative LOG_DIR is taken from the caller's directory, before the cd.
if [[ -n "${LOG_DIR:-}" ]]; then
    mkdir -p "$LOG_DIR"
    LOG_DIR=$(cd "$LOG_DIR" && pwd)
fi
cd "$(dirname "$0")"
default="r1 r2 r3 r4 r5 r6 r7 g1 g2 g3 g4 g5 g6 g6_read g7 n1 a1 a2 a3 a4 a5 a6 e1 e2 m1 m3 m4 m5 m6 m7 sha"
scenarios=("$@")
((${#scenarios[@]})) || read -ra scenarios <<<"${SCENARIOS:-$default}"
read -ra modes <<<"${MODES:-live remount crash fresh}"
if [[ ${BACKEND:-proton} == fake ]]; then stop_on=${STOP_ON-}; else stop_on=${STOP_ON-429,fail}; fi
stops() { [[ ",$stop_on," == *",$1,"* ]]; }
if stops 429; then
    # lib.sh decides the profile and XDG directories; the daemon appends to this log.
    daemon_log=${DAEMON_LOG:-$(source ./lib.sh && echo "${XDG_STATE_HOME:-$HOME/.local/state}/pdmount/$PROFILE.log")}
    daemon_log2=${DAEMON_LOG2:-$(source ./lib.sh && echo "${XDG_STATE_HOME:-$HOME/.local/state}/pdmount/$PROFILE2.log")}
    echo "stop on: $stop_on (429 check reads $daemon_log and $daemon_log2)"
fi
log_size() { stat -c %s "$1" 2>/dev/null || echo 0; }
# This run's lines of a log that match a 429; the first argument is the log, the second its offset.
log_hits() { tail -c +"$(($2 + 1))" "$1" 2>/dev/null | grep -E 'status: 429|RateLimited' || true; }
if [[ ${BACKEND:-proton} == fake ]]; then trash_min_free=0; else trash_min_free=${TRASH_MIN_FREE:-1073741824}; fi
pd_() { (source ./lib.sh && pd "$@"); }
# Free storage in bytes from `pdmount status` (one decimal of its unit), or
# nothing when it shows no quota.
free_bytes() {
    pd_ status 2>/dev/null | awk '
        function bytes(n, unit) {
            return n * 1024 ^ ((unit == "KiB") + 2 * (unit == "MiB") + 3 * (unit == "GiB") + 4 * (unit == "TiB"))
        }
        $1 == "Storage:" && $4 == "used" { printf "%d\n", bytes($6, $7) - bytes($2, $3) }'
}
# D036: empty the trash when storage runs low. Each run deletes its
# scenario's previous area, which goes to the trash.
check_storage() {
    ((trash_min_free > 0)) || return 0
    local free
    free=$(free_bytes)
    [[ -n "$free" ]] || return 0
    ((free < trash_min_free)) || return 0
    echo "storage: $free bytes free, below $trash_min_free: unmount and empty the trash (D036)"
    pd_ unmount --wait >/dev/null 2>&1 || true
    pd_ trash empty --yes || { echo "STOP: emptying the trash failed"; return 1; }
}
failed=0
logs=${LOG_DIR:-$(mktemp -d "${TMPDIR:-/tmp}/pdmount-scenarios.XXXXXX")}
mkdir -p "$logs"
for scenario in "${scenarios[@]}"; do
    for mode in "${modes[@]}"; do
        check_storage || { stopped=1; break 2; }
        start=$SECONDS
        log=$logs/$scenario-$mode.log
        if stops 429; then offset=$(log_size "$daemon_log"); offset2=$(log_size "$daemon_log2"); fi
        if VERIFY_MODE=$mode "./$scenario.sh" >"$log" 2>&1; then
            echo "$scenario [$mode]: PASS ($((SECONDS - start)) s)"
            ok=1
        else
            failed=$((failed + 1))
            echo "$scenario [$mode]: FAIL ($((SECONDS - start)) s): $(grep -m1 '^FAIL' "$log" || tail -1 "$log")"
            ok=0
        fi
        if stops 429; then
            # Only this run's lines; `grep 429` alone would match revision ids and sizes.
            hits=$(log_hits "$daemon_log" "$offset"; log_hits "$daemon_log2" "$offset2")
            if [[ -n "$hits" ]]; then
                echo "STOP: HTTP 429 during $scenario [$mode] ($(wc -l <<<"$hits") lines), first:"
                echo "  $(head -1 <<<"$hits" | cut -c1-300)"
                stopped=1
                break 2
            fi
        fi
        if ((ok == 0)) && stops fail; then
            echo "STOP: $scenario [$mode] failed"
            stopped=1
            break 2
        fi
    done
done
echo "logs: $logs"
if ((${stopped:-0})); then
    echo "the matrix stopped early; the mount may still be up"
    exit 1
fi
((failed == 0)) || { echo "$failed runs failed"; exit 1; }
