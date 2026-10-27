#!/usr/bin/env bash
# Soak test (DESIGN.md §15.1, D030, D048): two devices on one fake drive, a
# mix of workloads and faults for SOAK_DURATION seconds, then the checks (a)-(e)
# below. Never the real account.
#
# Usage: tests/soak.sh
#   SOAK_DURATION  seconds (default: 10800, 3 h)
#   SCENARIO_ROOT  everything lives here (default: /dev/shm/pdmount-soak)
#   PROFILE, PROFILE2  device A and B (default: fake-soak, fake-soak-2)
#   FLAP_EVERY, FLAP_JITTER  seconds between network outages, and the jitter
#                  either way (default: 2400, 600)
#   TOKEN_FIRST, TOKEN_EVERY  seconds until the first rejected session of A, and
#                  between the next ones (default: 1800, 3600)
#   OUTAGE_LENGTH  seconds a fault lasts (default: 300)
#   RSYNC_EVERY, GIT_EVERY, BEDIT_EVERY, APP_EVERY, AEDIT_EVERY, SAMPLE_EVERY
#                  seconds between runs of each loop (default: 600, 30, 60, 300, 600, 60)
#   FINAL_TIMEOUT  seconds each final catch-up step may take (default: 1800)
#
# Loops, all in the background, each logging `<epoch> <loop> ok|FAIL <detail>` to
# soak-logs/loops.log (a failing command's output goes to soak-logs/<loop>.out):
#   rsync    mutates a source tree (about 5 % of the files) and rsyncs it into A
#   git      a commit on A (adds, changes, renames, chmod +x); gc every 50 commits
#   bedit    B edits its own files; every AEDIT_EVERY also a file of A's rsync target
#   app      an official app (no mode, no SHA-1) writes a new revision of a B file
#   flaps    the drive is unreachable for all clients (`fake fault offline`)
#   token    A's session is rejected (`fake fault unauthorized`), then cleared
#   sampler  VmRSS, queue and counters of both daemons into samples.csv
#
# The mounts stay up afterwards (as fsx.sh leaves them). Unmount them before
# deleting SCENARIO_ROOT, or the mounts go stale; lib.sh sets the XDG paths:
#   (BACKEND=fake SCENARIO_ROOT=/dev/shm/pdmount-soak PROFILE=fake-soak PROFILE2=fake-soak-2
#    source tests/scenarios/lib.sh && pd unmount && pd2 unmount)
BACKEND=fake
SCENARIO_ROOT=${SCENARIO_ROOT:-/dev/shm/pdmount-soak}
PROFILE=${PROFILE:-fake-soak}
PROFILE2=${PROFILE2:-fake-soak-2}
source "$(dirname "$0")/scenarios/lib.sh"

SOAK_DURATION=${SOAK_DURATION:-10800}
FLAP_EVERY=${FLAP_EVERY:-2400}
FLAP_JITTER=${FLAP_JITTER:-600}
TOKEN_FIRST=${TOKEN_FIRST:-1800}
TOKEN_EVERY=${TOKEN_EVERY:-3600}
OUTAGE_LENGTH=${OUTAGE_LENGTH:-300}
RSYNC_EVERY=${RSYNC_EVERY:-600}
GIT_EVERY=${GIT_EVERY:-30}
BEDIT_EVERY=${BEDIT_EVERY:-60}
APP_EVERY=${APP_EVERY:-300}
AEDIT_EVERY=${AEDIT_EVERY:-600}
SAMPLE_EVERY=${SAMPLE_EVERY:-60}
FINAL_TIMEOUT=${FINAL_TIMEOUT:-1800}
GRACE=70 # seconds after an outage in which a failure is still expected

work=$SCENARIO_ROOT/soak-work
logs=$SCENARIO_ROOT/soak-logs
stopfile=$logs/stop
loops_log=$logs/loops.log
outages_log=$logs/outages.log
samples=$logs/samples.csv
soak_dir=pdmount-test/soak # below the mount, and on the drive with a leading /

now() { date +%s; }
log() { echo "$(now) $1 $2 ${3:-}" >>"$loops_log"; } # <loop> ok|FAIL <detail>

soak_over() { [[ -e $stopfile ]] || (($(now) >= END)); }

# Waits until <epoch>; returns 1 when the run is over by then.
wait_until() {
    while (($(now) < $1)); do
        soak_over && return 1
        sleep 1
    done
    ! soak_over
}

# Runs <function> every <seconds> until the run is over (the first run after one
# interval, unless the third argument is the delay of the first).
every() { # <seconds> <function> [first delay]
    local next=$(($(now) + ${3:-$1}))
    while wait_until "$next"; do
        "$2"
        # From the end of a run: a slow iteration does not queue up others.
        next=$(($(now) + $1))
    done
}

# ---- workloads ----

src=$work/rsync-src
rsync_iteration=0
loop_rsync_once() {
    rsync_iteration=$((rsync_iteration + 1))
    python3 -I - "$src" "$rsync_iteration" <<'PY' >>"$logs/rsync.out" 2>&1
import os, random, sys
root, n = sys.argv[1], int(sys.argv[2])
rng = random.Random(1000 + n)
files, dirs = [], []
for base, subdirs, names in os.walk(root):
    dirs.append(base)
    files += [os.path.join(base, name) for name in names]
files.sort()
dirs.sort()
k = max(1, len(files) // 60)  # three kinds of change: about 5 % in all
for path in rng.sample(files, min(len(files), 2 * k)):
    if rng.random() < 0.5:
        os.unlink(path)
    else:
        mode = os.stat(path).st_mode & 0o777
        with open(path, "wb") as f:
            f.write(rng.randbytes(rng.choice([0, 1, 100, 4096, rng.randrange(0, 70000)])))
        os.chmod(path, mode)
for j in range(k):
    path = os.path.join(rng.choice(dirs), f"add-{n:03d}-{j:02d}.dat")
    with open(path, "wb") as f:
        f.write(rng.randbytes(rng.choice([1, 100, 4096, rng.randrange(0, 70000)])))
    os.chmod(path, rng.choice([0o644, 0o755, 0o600]))
PY
    local status=0
    mkdir -p "$ROOT_A/soak/rsync" || status=$?
    ((status != 0)) || rsync -a --delete "$src/" "$ROOT_A/soak/rsync/" >>"$logs/rsync.out" 2>&1 || status=$?
    if ((status == 0)); then log rsync ok "iteration $rsync_iteration"; else log rsync FAIL "iteration $rsync_iteration exit $status"; fi
}

git_commit=0
soak_repo=$ROOT_A/soak/git/repo
soak_git() { git -C "$soak_repo" "$@"; }
loop_git_once() {
    git_commit=$((git_commit + 1))
    local i=$git_commit status=0
    {
        [[ -d $soak_repo/.git ]] || { mkdir -p "$soak_repo" && soak_git init -q -b main; }
        printf 'file %d\n' "$i" >"$soak_repo/f$((i % 40))"
        printf 'line %d\n' "$i" >>"$soak_repo/log.txt"
        if ((i % 10 == 0)) && [[ -e "$soak_repo/f$((i % 40 + 1))" ]]; then
            soak_git mv "f$((i % 40 + 1))" "renamed-$i"
        fi
        if ((i % 15 == 0)); then chmod +x "$soak_repo/f$((i % 40))"; fi
        soak_git add -A
        soak_git commit -q -m "commit $i"
        # gc.auto is lowered so that the pass really packs; autoDetach keeps it in the foreground.
        if ((i % 50 == 0)); then soak_git -c gc.auto=50 -c gc.autoDetach=false gc --auto -q; fi
    } >>"$logs/git.out" 2>&1 || status=$?
    if ((status == 0)); then log git ok "commit $i"; else log git FAIL "commit $i exit $status"; fi
}

bedit_count=0
loop_bedit_once() {
    bedit_count=$((bedit_count + 1))
    local dir=$ROOT_B/soak/b-own status=0
    {
        mkdir -p "$dir"
        printf 'B edit %d at %s\n' "$bedit_count" "$(now)" >"$dir/file-$((bedit_count % 20))"
        printf 'B append %d\n' "$bedit_count" >>"$dir/log-$((bedit_count % 5))"
    } >>"$logs/bedit.out" 2>&1 || status=$?
    if ((status == 0)); then log bedit ok "edit $bedit_count"; else log bedit FAIL "edit $bedit_count exit $status"; fi
}

# A file of A's rsync target as B sees it; the next rsync overwrites it, so
# conflict copies are expected.
loop_aedit_once() {
    local file status=0
    file=$(find "$ROOT_B/soak/rsync" -type f 2>/dev/null | shuf -n 1 || true)
    if [[ -z $file ]]; then log aedit ok "nothing to edit yet"; return; fi
    printf 'B changes A: %s\n' "$(now)" >"$file" 2>>"$logs/aedit.out" || status=$?
    if ((status == 0)); then log aedit ok "${file#"$ROOT_B"/}"; else log aedit FAIL "${file#"$ROOT_B"/} exit $status"; fi
}

app_count=0
loop_app_once() {
    app_count=$((app_count + 1))
    local file status=0
    # A file that A already has is on the drive.
    file=$(find "$ROOT_A/soak/b-own" -type f -name 'file-*' 2>/dev/null | shuf -n 1 || true)
    if [[ -z $file ]]; then log app ok "nothing to write yet"; return; fi
    printf 'app write %d at %s\n' "$app_count" "$(now)" >"$work/app-content"
    "$PDMOUNT" fake write --app --path "/$soak_dir/b-own/${file##*/}" --from "$work/app-content" "$FAKE_DIR" \
        >>"$logs/app.out" 2>&1 || status=$?
    if ((status == 0)); then log app ok "${file##*/}"; else log app FAIL "${file##*/} exit $status"; fi
}

# ---- faults ----

window() { echo "$1 $2 $3" >>"$outages_log"; } # <kind> <start> <end>

loop_flaps() {
    local next
    next=$(($(now) + FLAP_EVERY + RANDOM % (2 * FLAP_JITTER + 1) - FLAP_JITTER))
    while wait_until "$next"; do
        local start
        start=$(now)
        window offline "$start" $((start + OUTAGE_LENGTH))
        if fake_fault offline --for "$OUTAGE_LENGTH" >>"$logs/flaps.out" 2>&1; then
            log flaps ok "offline for $OUTAGE_LENGTH s"
        else
            log flaps FAIL "could not set the fault"
        fi
        next=$(($(now) + FLAP_EVERY + RANDOM % (2 * FLAP_JITTER + 1) - FLAP_JITTER))
    done
}

loop_token() {
    local next=$(($(now) + TOKEN_FIRST)) uid start
    while wait_until "$next"; do
        uid=$(device_uid 1)
        start=$(now)
        window unauthorized "$start" $((start + OUTAGE_LENGTH))
        if [[ -n $uid ]] && fake_fault unauthorized --client "$uid" >>"$logs/token.out" 2>&1; then
            # Cleared after OUTAGE_LENGTH, or at once when the run ends.
            wait_until $((start + OUTAGE_LENGTH)) || true
            if fake_fault clear --client "$uid" >>"$logs/token.out" 2>&1; then
                log token ok "session rejected for $(($(now) - start)) s"
            else
                log token FAIL "could not clear the fault"
            fi
        else
            log token FAIL "could not set the fault (device '$uid')"
        fi
        next=$((start + TOKEN_EVERY))
    done
}

# ---- sampler ----

daemon_rss() { # <pid>
    [[ -n ${1:-} ]] && sed -n 's/^VmRSS:[[:space:]]*\([0-9]*\) kB$/\1/p' "/proc/$1/status" 2>/dev/null || true
}

queue_numbers() { # <status text> -> "<pending> <parked>"
    sed -n 's/^Queue: *\([0-9]*\) pending, \([0-9]*\) parked.*/\1 \2/p' <<<"$1"
}

sample_once() {
    local text_a text_b pid_a pid_b pending_a parked_a pending_b parked_b
    text_a=$(status_text)
    text_b=$(status_text2)
    pid_a=$(sed -n 's/.*daemon pid \([0-9]*\).*/\1/p' <<<"$text_a")
    pid_b=$(sed -n 's/.*daemon pid \([0-9]*\).*/\1/p' <<<"$text_b")
    read -r pending_a parked_a <<<"$(queue_numbers "$text_a")"
    read -r pending_b parked_b <<<"$(queue_numbers "$text_b")"
    printf '%s,%s,%s,%s,%s,%s,%s\n' "$(now)" "$(daemon_rss "$pid_a")" "$(daemon_rss "$pid_b")" \
        "${pending_a:-}" "${parked_a:-}" "${pending_b:-}" "${parked_b:-}" >>"$samples"
    echo "$(now) A $(counters pd | tr '\n' ' ')" >>"$logs/counters.log"
    echo "$(now) B $(counters pd2 | tr '\n' ' ')" >>"$logs/counters.log"
}

# `pdmount stats` as `name=value` words on one line per call.
counters() { # <pd function>
    "$1" stats 2>/dev/null | awk 'NF == 2 { print $1 "=" $2 }'
}

# ---- run ----

prepare_mount # mounts A (seeding the fake drive first if needed)
is_mounted_second || mount_second
rm -rf "$logs"
mkdir -p "$logs" "$work"
[[ -n "$(device_uid 1)" ]] || fail "no device id for A in pdmount status"

step "soak: preparing $ROOT_A/soak"
rm -rf "$ROOT_A/soak" "$work/rsync-src" "$work/app-content"
mkdir -p "$ROOT_A/soak/b-own" "$ROOT_A/soak/rsync"
make_tree "$src" 400 2
wait_queue_empty
wait_queue_empty2
send_marker "$ROOT_A" "$ROOT_B" soak-start "${BARRIER_TIMEOUT:-900}"
echo "timestamp,rss_a_kb,rss_b_kb,pending_a,parked_a,pending_b,parked_b" >"$samples"
: >"$loops_log"
: >"$outages_log"

git_env
unset GIT_AUTHOR_DATE GIT_COMMITTER_DATE # real commit times; the hashes are not compared
export GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=maintenance.auto GIT_CONFIG_VALUE_0=false

START=$(now)
END=$((START + SOAK_DURATION))
step "soak: $SOAK_DURATION s from $START (until $END), logs in $logs"

loop_pids=()
start_loop() { # <name> <function...>
    local name=$1
    shift
    (
        set +e
        trap - EXIT
        "$@"
    ) >>"$logs/$name.err" 2>&1 &
    loop_pids+=($!)
}
stop_loops() {
    touch "$stopfile"
    local pid
    for pid in "${loop_pids[@]}"; do wait "$pid" 2>/dev/null || true; done
    loop_pids=()
}
trap 'touch "$stopfile"; kill "${loop_pids[@]}" 2>/dev/null || true' INT TERM

start_loop rsync every "$RSYNC_EVERY" loop_rsync_once 30
start_loop git every "$GIT_EVERY" loop_git_once 15
start_loop bedit every "$BEDIT_EVERY" loop_bedit_once 45
start_loop aedit every "$AEDIT_EVERY" loop_aedit_once 330
start_loop app every "$APP_EVERY" loop_app_once 120
start_loop flaps loop_flaps
start_loop token loop_token
start_loop sampler every "$SAMPLE_EVERY" sample_once 1

while ! soak_over; do sleep 5; done
step "soak: stopping the loops"
stop_loops
fake_fault clear
echo "loops stopped $(($(now) - START)) s after the start"

# ---- end of the run ----

step "soak: catching up"
QUEUE_TIMEOUT=$FINAL_TIMEOUT wait_queue_empty
QUEUE_TIMEOUT=$FINAL_TIMEOUT wait_queue_empty2
send_marker "$ROOT_A" "$ROOT_B" soak-end-a "$FINAL_TIMEOUT"
QUEUE_TIMEOUT=$FINAL_TIMEOUT wait_queue_empty2
send_marker "$ROOT_B" "$ROOT_A" soak-end-b "$FINAL_TIMEOUT"
QUEUE_TIMEOUT=$FINAL_TIMEOUT wait_queue_empty
sample_once

status=0
check() { # <name> <exit status of the check>
    if (($2 == 0)); then echo "PASS ($1)"; else echo "FAIL ($1)"; status=1; fi
}
run_check() { # <name> <command...>: the command's stdin is the caller's
    local rc=0
    "${@:2}" || rc=$?
    check "$1" "$rc"
}

step "soak: (a) no EIO outside outages"
run_check "no EIO outside outages" python3 -I - "$logs" "$GRACE" "$SCENARIO_ROOT/xdg/state/pdmount/$PROFILE.log" \
    "$SCENARIO_ROOT/xdg/state/pdmount/$PROFILE2.log" "$START" <<'PY'
import calendar, re, sys, time
logs, grace, start = sys.argv[1], int(sys.argv[2]), int(sys.argv[5])
windows = []
for line in open(f"{logs}/outages.log"):
    parts = line.split()
    if len(parts) == 3:
        windows.append((int(parts[1]), int(parts[2]) + grace))
def inside(t):
    return any(a <= t <= b for a, b in windows)
bad, total = [], 0
for line in open(f"{logs}/loops.log"):
    parts = line.split(maxsplit=3)
    if len(parts) >= 3 and parts[2] == "FAIL":
        total += 1
        if not inside(int(parts[0])):
            bad.append("loop " + line.strip())
stamp = re.compile(r"^(\d{4})-(\d\d)-(\d\d)T(\d\d):(\d\d):(\d\d)")
for path in sys.argv[3:5]:
    try:
        lines = open(path, errors="replace")
    except OSError as e:
        bad.append(f"cannot read {path}: {e}")
        continue
    for line in lines:
        if "WARN" not in line or "answered EIO" not in line:
            continue
        m = stamp.match(line)
        if not m:
            continue
        t = calendar.timegm(tuple(int(x) for x in m.groups()))
        if t < start - 60:  # an earlier run's lines in a reused log
            continue
        total += 1
        if not inside(t):
            bad.append(f"{path.rsplit('/', 1)[-1]}: {line.strip()[:200]}")
print(f"{total} failures in all, {len(windows)} outage windows, {len(bad)} outside them")
for line in bad[:20]:
    print("  " + line)
sys.exit(1 if bad else 0)
PY

step "soak: (b) nothing parked"
parked=0
for text in "$(status_text)" "$(status_text2)"; do
    q=$(queue_numbers "$text")
    echo "queue (pending parked): $q"
    [[ ${q##* } == 0 ]] || parked=1
done
check "nothing parked" $parked

step "soak: (c) stable RSS"
run_check "stable RSS" python3 -I - "$samples" "$START" "$SOAK_DURATION" <<'PY'
import csv, sys
path, start, length = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
rows = []
with open(path) as f:
    for row in csv.DictReader(f):
        try:
            rows.append((int(row["timestamp"]) - start, int(row["rss_a_kb"]), int(row["rss_b_kb"])))
        except (ValueError, TypeError):
            pass
bad = False
for name, idx in (("A", 1), ("B", 2)):
    early = [r[idx] for r in rows if length / 6 <= r[0] <= length / 3]
    late = [r[idx] for r in rows if 2 * length / 3 <= r[0] <= length]
    if not early or not late:
        print(f"{name}: too few samples ({len(rows)}) for the windows; not checked")
        continue
    ratio = max(late) / max(early)
    print(f"{name}: max RSS {max(early) / 1024:.0f} MiB (early window) -> {max(late) / 1024:.0f} MiB (last third), x{ratio:.2f}")
    bad |= ratio > 1.25
sys.exit(1 if bad else 0)
PY

step "soak: (d) convergence"
converged=0
out=$(rsync -ani --checksum --omit-dir-times "$ROOT_A/soak/" "$ROOT_B/soak/") || converged=1
if [[ -n $out ]]; then echo "$out" | head -20; converged=1; fi
for root in "$ROOT_A" "$ROOT_B"; do
    git -C "$root/soak/git/repo" fsck --full --strict || converged=1
done
echo "commits on A: $(git -C "$ROOT_A/soak/git/repo" rev-list --count HEAD)"
check "convergence" $converged

step "soak: (e) no internal faults"
faults=0
for fn in pd pd2; do
    for name in handler_panics hot_path_network_calls; do
        value=$("$fn" stats | awk -v n="$name" '$1 == n { print $2 }')
        echo "$fn $name = $value"
        [[ $value == 0 ]] || faults=1
    done
done
check "no internal faults" $faults

echo "loops: $(awk '$3 == "ok"' "$loops_log" | wc -l) ok, $(awk '$3 == "FAIL"' "$loops_log" | wc -l) FAIL; logs in $logs"
((status == 0)) || fail "the soak found problems; see above and $logs"
echo "soak: PASS"
