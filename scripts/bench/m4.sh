#!/usr/bin/env bash
# The M4 exit benchmarks (DESIGN.md §16; definitions D057) on a fake-backend mount with a
# simulated network (`pdmount fake profile`, DESIGN.md §15.1).
#
# Rows:
#   git    `git status` on a warm clone of git.git (v2.50.0): 2 untimed runs per side, then 11
#          timed pairs, local then mount; median on the mount over median on the local disk, at
#          most 2.0. Notes in git.log whether `git status` rewrote the mount's index; the
#          daemon's stats over the timed runs go to git-stats.json (latency reset before them)
#   untar  `tar -xf` of 5000 small files (scripts/testdata/make-untar-dataset.sh), timed until
#          tar exits: median of 3 runs, mount over local, at most 15.0 (D065)
#   read   sequential 1 MiB reads of a 1 GiB file after a remount (daemon RAM cold, disk cache
#          warm, page cache dropped): at least 500 MB/s (10^6 bytes)
#   slo    the daemon's p99 latencies (`pdmount stats --json | slo.py`, §5.7) over latency.sh's
#          workload, the read row's sequential read and 5 runs of the git row's `git status`:
#          the median of 5 rounds on a quiet disk. A round with a bare fdatasync p99 above
#          10 ms before or after it does not count; after 15 rounds without 5 quiet ones the
#          row fails (D086)
#   gc     with `cache.max_bytes = 512MiB`, reads 6 files of 256 MiB while sampling the block
#          cache's size every second, then `pdmount cache gc`: every sample at most 1.05 x the
#          cap, at most the cap after gc. "SKIP" until `cache gc` exists (M4.9); the samples are
#          still taken
#
# Usage: scripts/bench/m4.sh [git|untar|read|slo|gc|all ...]   (default: all, in that order)
#   BENCH_ROOT    the profile's state and cache, the local comparisons and the git.git mirror
#                 (default ~/.cache/pdmount-bench). On the disk, not tmpfs: the SLOs are
#                 defined on SSD (§5.7)
#   FAKE_DIR      the fake drive (default /dev/shm/pdmount-bench-m4-fake): in tmpfs, since a
#                 real backend writes nothing locally
#   RTT_MS, UP, DOWN  the network profile (default 80 ms, 20000000 B/s each way)
#   REPORT_DIR    where report.md and the logs go (default target/bench/m4-<date>); each run
#                 appends a section to report.md
# Exit status 1 if a row fails. About 15 min, plus the first clone of git.git from
# git.kernel.org (once; the mirror is kept in BENCH_ROOT). Not part of scripts/ci.sh.
set -euo pipefail
export LC_ALL=C
here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)

rows=()
for arg in "${@:-all}"; do
    case $arg in
        all) rows+=(git untar read slo gc) ;;
        git | untar | read | slo | gc) rows+=("$arg") ;;
        *) echo "usage: $0 [git|untar|read|slo|gc|all ...]" >&2; exit 2 ;;
    esac
done
for tool in git tar python3 du; do
    command -v "$tool" >/dev/null || { echo "missing: $tool" >&2; exit 1; }
done

BENCH_ROOT=${BENCH_ROOT:-$HOME/.cache/pdmount-bench}
RTT_MS=${RTT_MS:-80} UP=${UP:-20000000} DOWN=${DOWN:-20000000}
report_dir=${REPORT_DIR:-$repo/target/bench/m4-$(date +%F)}
GIT_URL=https://git.kernel.org/pub/scm/git/git.git GIT_TAG=v2.50.0
CACHE_MAX=${CACHE_MAX:-4GiB} GC_CAP_MIB=512

(cd "$repo" && cargo build --release --locked -p pdmount --features fake-backend)

# shellcheck disable=SC2174
mkdir -p -m 0700 "$BENCH_ROOT"
export BACKEND=fake VERIFY_MODE=live SCENARIO_ROOT=$BENCH_ROOT/root PROFILE=bench
export FAKE_DIR=${FAKE_DIR:-/dev/shm/pdmount-bench-m4-fake}
export PDMOUNT=$repo/target/release/pdmount
# The mount's queue drains at the simulated network's pace.
export QUEUE_TIMEOUT=${QUEUE_TIMEOUT:-7200}
# shellcheck source=../../tests/scenarios/lib.sh
source "$repo/tests/scenarios/lib.sh"
private_root "$BENCH_ROOT"
git_env
config=$XDG_CONFIG_HOME/pdmount/$PROFILE.toml
blocks=$XDG_CACHE_HOME/pdmount/$PROFILE/blocks
area=$MNT/pdmount-test/bench
local_root=$BENCH_ROOT/local
mirror=$BENCH_ROOT/git.git
mkdir -p "$report_dir"
report=$report_dir/report.md
log=$report_dir/setup.log
failed=0

# The test profile's whole config: the defaults, a cache cap and a device name.
write_config() { # <cache max_bytes>
    mkdir -p "${config%/*}"
    printf '[cache]\nmax_bytes = "%s"\n\n[device]\nname = "bench"\n' "$1" >"$config"
}
net_on() { "$PDMOUNT" fake profile --rtt "$RTT_MS" --up "$UP" --down "$DOWN" "$FAKE_DIR"; }
net_off() { "$PDMOUNT" fake profile clear "$FAKE_DIR"; }
unmount_now() { if is_mounted; then pd unmount "$MNT" >/dev/null; fi; }
remount() {
    unmount_now
    mkdir -p "$MNT"
    mount_now >/dev/null
}
cleanup() {
    unmount_now || true
    [[ -d $FAKE_DIR ]] && net_off || true
    cp "$XDG_STATE_HOME/pdmount/$PROFILE.log" "$report_dir/daemon.log" 2>/dev/null || true
}
trap cleanup EXIT

say() { echo "$*" | tee -a "$log"; }
# Runs a command with its output in the log, prints its wall time in seconds and returns
# its status (a command substitution does not stop at errors).
seconds() { # <command...>
    local start=$EPOCHREALTIME status=0
    "$@" >>"$log" 2>&1 || status=$?
    awk -v a="$start" -v b="$EPOCHREALTIME" 'BEGIN { printf "%.4f", b - a }'
    return "$status"
}
median() { printf '%s\n' "$@" | sort -g | awk '{ v[NR] = $1 } END { print (NR % 2) ? v[(NR + 1) / 2] : (v[NR / 2] + v[NR / 2 + 1]) / 2 }'; }
ratio() { awk -v a="$1" -v b="$2" 'BEGIN { printf "%.2f", a / b }'; }
at_most() { awk -v a="$1" -v b="$2" 'BEGIN { exit !(a <= b) }'; }
row() { # <row> <mount> <local> <ratio or value> <limit> <PASS|FAIL|SKIP>
    printf '| %s | %s | %s | %s | %s | %s |\n' "$@" >>"$report"
    [[ $6 != FAIL ]] || failed=1
    echo "$1: $6 ($4; limit $5)"
}

# The empty drive, made once; rows add their files with `fake create` while unmounted.
ensure_drive() {
    private_root "$FAKE_DIR"
    [[ -f $FAKE_DIR/.seeded-m4 ]] && return 0
    unmount_now
    remove_fake_dir "$FAKE_DIR"
    private_root "$FAKE_DIR"
    local seed
    seed=$(mktemp -d "$BENCH_ROOT/seed.XXXXXX")
    mkdir -p "$seed/pdmount-test/bench/gc"
    "$PDMOUNT" fake seed --from "$seed" "$FAKE_DIR" >/dev/null
    rm -rf "$seed"
    touch "$FAKE_DIR/.seeded-m4"
    pd reset --keep-login >/dev/null
}

# Puts a deterministic file of <MiB> on the drive at /pdmount-test/bench/<name>, once,
# without the network profile.
seed_file() { # <name> <MiB>
    local marker=$FAKE_DIR/.seeded-file-${1//\//-}
    [[ -f $marker ]] && return 0
    unmount_now
    net_off
    local tmp=$BENCH_ROOT/seed-file.bin
    python3 -I - "$tmp" "$1" "$2" <<'EOF'
import random, sys
path, name, mib = sys.argv[1], sys.argv[2], int(sys.argv[3])
rng = random.Random(f"pdmount-m4-{name}")
with open(path, "wb") as f:
    for _ in range(mib):
        f.write(rng.randbytes(1 << 20))
EOF
    "$PDMOUNT" fake create --path "/pdmount-test/bench/$1" --from "$tmp" "$FAKE_DIR"
    rm -f "$tmp"
    touch "$marker"
}

ensure_mirror() {
    if [[ ! -d $mirror ]]; then
        say "cloning $GIT_URL into $mirror (once)"
        rm -rf "$mirror.tmp"
        git clone --mirror --quiet "$GIT_URL" "$mirror.tmp"
        mv "$mirror.tmp" "$mirror"
    fi
    say "git.git mirror HEAD $(git -C "$mirror" rev-parse HEAD), $GIT_TAG $(git -C "$mirror" rev-parse "$GIT_TAG^{commit}")"
}

# A clone on the mount whose uploads are done; fresh with <fresh>. A marker in .git says
# the clone completed (a failed checkout leaves a .git behind).
mount_clone() { # [fresh]
    ensure_mirror
    if [[ -n ${1:-} || ! -f $area/git/.git/pdmount-bench-complete ]]; then
        rm -rf "$area/git"
        wait_queue_empty
        local t start
        t=$(seconds git clone --quiet --no-hardlinks --branch "$GIT_TAG" "$mirror" "$area/git") ||
            fail "git clone into the mount failed; see $log"
        say "clone on the mount: $t s"
        start=$SECONDS
        wait_queue_empty
        say "queue drained after $((SECONDS - start)) s"
        touch "$area/git/.git/pdmount-bench-complete"
    fi
}

# A value of `pdmount stats --json`, by its section and key (`gauges.queue_pending`).
stat_of() { # <section.key>
    pd stats --json | python3 -I -c 'import json, sys
section, key = sys.argv[1].split(".")
print(json.load(sys.stdin)[section][key])' "$1"
}

git_status_runs() { # <dir> <runs>: prints the times
    local i
    for ((i = 0; i < $2; i++)); do
        seconds git -C "$1" status || fail "git status in $1 failed; see $log"
        echo
    done
}

row_git() {
    mount_clone fresh
    rm -rf "$local_root/git"
    mkdir -p "$local_root"
    git clone --quiet --no-hardlinks --branch "$GIT_TAG" "$mirror" "$local_root/git" >>"$log" 2>&1
    # The clone's writeback (about 100 MB) would slow the local side's first timed runs.
    sync -f "$local_root"
    local i t mount_times=() local_times=()
    # The sides alternate, so a slow spell of the disk hits both.
    for i in 1 2; do
        git_status_runs "$local_root/git" 1 >/dev/null
        git_status_runs "$area/git" 1 >/dev/null
    done
    # Whether `git status` rewrites the index on the mount (git's racy-index refresh): its
    # upload would compete with the timed runs.
    local index=$area/git/.git/index mtime pending uploads
    pd stats --reset >>"$log"
    mtime=$(stat -c %.9Y "$index") pending=$(stat_of gauges.queue_pending)
    uploads=$(stat_of counters.uploads)
    for i in {1..11}; do
        t=$(git_status_runs "$local_root/git" 1) && local_times+=("$t")
        t=$(git_status_runs "$area/git" 1) && mount_times+=("$t")
        if ((i == 1)); then
            mtime="$mtime -> $(stat -c %.9Y "$index")"
            pending="$pending -> $(stat_of gauges.queue_pending)"
            say "first timed run on the mount: index mtime $mtime, queue_pending $pending"
        fi
    done
    pd stats --json >"$report_dir/git-stats.json"
    say "uploads during the timed runs: $(($(stat_of counters.uploads) - uploads))"
    say "git status, mount: ${mount_times[*]} s; local: ${local_times[*]} s"
    ((${#mount_times[@]} == 11 && ${#local_times[@]} == 11)) || fail "a git status failed; see $log"
    local m l r
    m=$(median "${mount_times[@]}") l=$(median "${local_times[@]}")
    r=$(ratio "$m" "$l")
    rm -rf "$local_root/git"
    row git "${m} s" "${l} s" "$r" "≤ 2.0" "$(at_most "$r" 2.0 && echo PASS || echo FAIL)"
}

row_untar() {
    local tarball=$BENCH_ROOT/untar.tar
    [[ -f $tarball ]] || "$repo/scripts/testdata/make-untar-dataset.sh" "$tarball"
    local mount_times=() local_times=() i t ok=1
    rm -rf "$local_root/untar" "$area"/untar-*
    wait_queue_empty
    mkdir -p "$local_root/untar"
    for i in 1 2 3; do
        mkdir "$local_root/untar/$i"
        t=$(seconds tar -xf "$tarball" -C "$local_root/untar/$i") || fail "local tar failed; see $log"
        local_times+=("$t")
    done
    for i in 1 2 3; do
        mkdir "$area/untar-$i"
        if ! t=$(seconds tar -xf "$tarball" -C "$area/untar-$i"); then
            say "tar into the mount failed (run $i); see $log"
            ok=0
            break
        fi
        mount_times+=("$t")
        wait_queue_empty
    done
    say "untar, mount: ${mount_times[*]} s; local: ${local_times[*]} s"
    rm -rf "$local_root/untar" "$area"/untar-*
    wait_queue_empty
    if ((!ok)); then
        row untar "tar failed" "$(median "${local_times[@]}") s" "-" "≤ 15.0" FAIL
        return
    fi
    local m l r
    m=$(median "${mount_times[@]}") l=$(median "${local_times[@]}")
    r=$(ratio "$m" "$l")
    row untar "${m} s" "${l} s" "$r" "≤ 15.0" "$(at_most "$r" 15.0 && echo PASS || echo FAIL)"
}

# The 1 GiB file, downloaded once on this profile, so the disk cache holds it.
read_file=$area/read.bin
warm_read_file() {
    seed_file read.bin 1024
    net_on
    is_mounted || remount
    wait_visible "$read_file" 120
    python3 -I "$here/read.py" --only seq --json "$report_dir/read-download.json" \
        "$read_file" "read download" drop | tee -a "$log"
}

row_read() {
    warm_read_file
    remount
    python3 -I "$here/read.py" --only seq --json "$report_dir/read-cached.json" \
        "$read_file" "read cached" drop | tee -a "$log"
    local mb
    mb=$(python3 -I -c 'import json, sys; print(f"{json.load(open(sys.argv[1]))["seq"]["mb_s"]:.0f}")' \
        "$report_dir/read-cached.json")
    row read "${mb} MB/s" - "${mb} MB/s" "≥ 500 MB/s" "$(at_most 500 "$mb" && echo PASS || echo FAIL)"
}

# One round of the slo row; the daemon's stats go to $1.
slo_round() {
    rm -rf "$area/slo"
    wait_queue_empty
    pd stats --reset >>"$log"
    local out pids=() pid i
    out=$(mktemp -d "$BENCH_ROOT/slo.XXXXXX")
    for i in 1 2 3 4; do
        python3 -I "$here/ops.py" "$area/slo/c$i" 1000 "$out/$i.json" &
        pids+=($!)
    done
    for pid in "${pids[@]}"; do wait "$pid" || fail "a latency client failed"; done
    python3 -I "$here/report.py" "client 4x1000" "$out"/*.json | tee -a "$log"
    rm -rf "$out"
    python3 -I "$here/read.py" --only seq "$read_file" "slo read" drop | tee -a "$log"
    git_status_runs "$area/git" 5 >>"$log"
    pd stats --json >"$1"
    rm -rf "$area/slo"
    wait_queue_empty
}

# Whether a bare fdatasync on the bench disk is at most 10 ms at p99 (D086).
disk_quiet() {
    local p99
    p99=$(python3 -I "$here/fdatasync.py" "$BENCH_ROOT")
    echo "bare fdatasync p99 $p99 ms" | tee -a "$log"
    python3 -I -c 'import sys; sys.exit(float(sys.argv[1]) > 10)' "$p99"
}

row_slo() {
    mount_clone
    warm_read_file
    git_status_runs "$area/git" 2 >/dev/null
    local quiet=() round file noisy=0
    rm -f "$report_dir"/slo-stats-*.json
    for round in $(seq 1 15); do
        file=$report_dir/slo-stats-$round.json
        if disk_quiet && slo_round "$file" && disk_quiet; then
            quiet+=("$file")
            ((${#quiet[@]} < 5)) || break
        else
            noisy=$((noisy + 1))
        fi
    done
    if ((${#quiet[@]} < 5)); then
        row slo - - "only ${#quiet[@]} quiet rounds of 15" "§5.7 p99, median of 5 quiet rounds" FAIL
        return
    fi
    local verdict=PASS
    python3 -I "$here/slo.py" "${quiet[@]}" | tee "$report_dir/slo.txt" | tee -a "$log" ||
        verdict=FAIL
    local passed total
    passed=$(grep -c '^PASS' "$report_dir/slo.txt" || true)
    total=$(grep -vc '^MISSING' "$report_dir/slo.txt" || true)
    row slo - - "$passed of $total checks pass ($noisy rounds not counted)" \
        "§5.7 p99, median of 5 quiet rounds" "$verdict"
}

row_gc() {
    local i
    for i in 1 2 3 4 5 6; do seed_file "gc/f$i.bin" 256; done
    write_config "${GC_CAP_MIB}MiB"
    net_on
    remount
    local samples=$report_dir/gc-samples.txt
    : >"$samples"
    (while :; do du -sb "$blocks" 2>/dev/null | cut -f1 >>"$samples"; sleep 1; done) &
    local sampler=$!
    for i in 1 2 3 4 5 6; do
        wait_visible "$area/gc/f$i.bin" 120
        cat "$area/gc/f$i.bin" >/dev/null
    done
    local have_gc=0 after=-
    if "$PDMOUNT" cache gc --help >/dev/null 2>&1; then
        have_gc=1
        pd cache gc >>"$log" 2>&1
        sleep 2
    fi
    kill "$sampler"
    wait "$sampler" 2>/dev/null || true
    ((have_gc)) && after=$(du -sb "$blocks" | cut -f1)
    write_config "$CACHE_MAX"
    remount
    local cap=$((GC_CAP_MIB << 20)) max
    max=$(sort -n "$samples" | tail -1)
    local value
    value="max $(ratio "$max" "$cap") x cap over $(wc -l <"$samples") samples, after gc $after"
    say "gc: $value"
    if ((!have_gc)); then
        row gc - - "$value" "≤ 1.05 x cap; ≤ cap after gc" "SKIP (no cache gc)"
    elif at_most "$max" "$(awk -v c="$cap" 'BEGIN { print c * 1.05 }')" && at_most "$after" "$cap"; then
        row gc - - "$value" "≤ 1.05 x cap; ≤ cap after gc" PASS
    else
        row gc - - "$value" "≤ 1.05 x cap; ≤ cap after gc" FAIL
    fi
}

{
    printf '\n## Run %s\n\n' "$(date '+%F %T')"
    printf 'Commit %s, fake profile rtt %s ms, up %s B/s, down %s B/s, BENCH_ROOT %s, FAKE_DIR %s.\n\n' \
        "$(git -C "$repo" describe --always --dirty)" "$RTT_MS" "$UP" "$DOWN" "$BENCH_ROOT" "$FAKE_DIR"
    printf '| Row | Mount | Local | Ratio or value | Limit | Result |\n|---|---|---|---|---|---|\n'
} >>"$report"
ensure_drive
write_config "$CACHE_MAX"
net_on
remount
mkdir -p "$area"
for name in "${rows[@]}"; do
    log=$report_dir/$name.log
    step "$name (log: $log)"
    # In a subshell, so a row that stops (`fail`) still lets the others run.
    lines=$(wc -l <"$report")
    if ! ("row_$name"; exit "$failed") 2> >(tee -a "$log" >&2); then
        failed=1
        if [[ $(wc -l <"$report") == "$lines" ]]; then
            row "$name" - - "stopped: $(grep '^FAIL:' "$log" | tail -1 | cut -c7-)" - FAIL
        fi
    fi
done
echo
echo "Report: $report"
exit "$failed"
