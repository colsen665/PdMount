# shellcheck shell=bash
# Shared setup for the scenario scripts (DESIGN.md §15.3). Source it.
#
# A scenario calls prepare_mount, makes its changes, calls verify_cycle, then
# checks the result. verify_cycle does what VERIFY_MODE asks for between the
# two:
#   live     nothing: verify on the same mount (default)
#   remount  unmount (draining the queue), mount
#   crash    kill -9 the daemon (with the queue non-empty when the scenario
#            wrote), unmount --force, mount, wait until the queue is empty
#   fresh    wait until the queue is empty, unmount, `reset --keep-login`,
#            mount: everything is read from the backend again
#
# Environment:
#   BACKEND      proton (default): the real account; fake: a persisted fake drive
#                (needs a build with `--features fake-backend`)
#   VERIFY_MODE  live, remount, crash or fresh (see above)
#   PDMOUNT      pdmount binary (default: target/release/pdmount, else target/debug/pdmount;
#                with BACKEND=fake: target/debug/pdmount)
#   PROFILE      profile (default: default; with BACKEND=fake: fake-scenarios)
#   SECRETS      force (default): --force-file-secrets; allow: --allow-file-secrets; keyring: no flag
#                (D029; only keyring tests use the keyring)
#   SCENARIO_ROOT  work directories and, with BACKEND=fake, everything else below (default:
#                /dev/shm/pdmount-tests if /dev/shm is writable, else ~/.cache/pdmount-scenarios).
#                With BACKEND=fake, XDG_{CONFIG,DATA,CACHE,STATE}_HOME point into it as well, so the
#                test profile's store, staging, cache and log stay in tmpfs
#   CACHE_MAX    with BACKEND=fake: the test profile's `[cache] max_bytes` (default: 1GiB)
#   MNT          mountpoint (default: ~/ProtonDrive; with BACKEND=fake: $SCENARIO_ROOT/fake-mnt)
#   FAKE_DIR     the fake drive (default: $SCENARIO_ROOT/fake-drive); seeded with the dataset on
#                first use. PDMOUNT_FAKE_KEEP_REVISIONS defaults to 4 here
#   DATASET      local copy of the M1 dataset, regenerated if missing (default:
#                ~/.cache/pdmount-scenarios/m1; with BACKEND=fake: $SCENARIO_ROOT/m1-nobig, without the
#                2 GiB file; see scripts/testdata/make-m1-dataset.sh)
set -euo pipefail

repo=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
BACKEND=${BACKEND:-proton}
VERIFY_MODE=${VERIFY_MODE:-live}
# Test data lives in tmpfs when there is one, to spare the disks.
if [[ -z "${SCENARIO_ROOT:-}" ]]; then
    if [[ -d /dev/shm && -w /dev/shm ]]; then
        SCENARIO_ROOT=/dev/shm/pdmount-tests
    else
        SCENARIO_ROOT=$HOME/.cache/pdmount-scenarios
    fi
fi
mkdir -p "$SCENARIO_ROOT"
case $BACKEND in
    proton)
        PROFILE=${PROFILE:-default}
        MNT=${MNT:-$HOME/ProtonDrive}
        # The local copy of the uploaded dataset (2.3 GiB) stays on disk; it is only read.
        DATASET=${DATASET:-$HOME/.cache/pdmount-scenarios/m1}
        dataset_flags=()
        backend_flag=()
        ;;
    fake)
        PROFILE=${PROFILE:-fake-scenarios}
        MNT=${MNT:-$SCENARIO_ROOT/fake-mnt}
        FAKE_DIR=${FAKE_DIR:-$SCENARIO_ROOT/fake-drive}
        DATASET=${DATASET:-$SCENARIO_ROOT/m1-nobig}
        dataset_flags=(--no-big)
        backend_flag=(--backend "fake:$FAKE_DIR")
        PDMOUNT=${PDMOUNT:-$repo/target/debug/pdmount}
        # The test profile's state, staging, cache and log go to SCENARIO_ROOT too.
        export XDG_CONFIG_HOME=$SCENARIO_ROOT/xdg/config XDG_DATA_HOME=$SCENARIO_ROOT/xdg/data \
            XDG_CACHE_HOME=$SCENARIO_ROOT/xdg/cache XDG_STATE_HOME=$SCENARIO_ROOT/xdg/state
        config=$XDG_CONFIG_HOME/pdmount/$PROFILE.toml
        if [[ ! -f "$config" ]]; then
            mkdir -p "${config%/*}"
            printf '[cache]\nmax_bytes = "%s"\nram_bytes = "128MiB"\n' "${CACHE_MAX:-1GiB}" >"$config"
        fi
        # The fake keeps a few revisions per file (it lives in the daemon).
        export PDMOUNT_FAKE_KEEP_REVISIONS=${PDMOUNT_FAKE_KEEP_REVISIONS:-4}
        ;;
    *) echo "BACKEND must be proton or fake" >&2; exit 2 ;;
esac
SECRETS=${SECRETS:-force}
case $SECRETS in
    force) secrets_flag=(--force-file-secrets) ;;
    allow) secrets_flag=(--allow-file-secrets) ;;
    keyring) secrets_flag=() ;;
    *) echo "SECRETS must be force, allow or keyring" >&2; exit 2 ;;
esac
if [[ -z "${PDMOUNT:-}" ]]; then
    PDMOUNT=$repo/target/release/pdmount
    [[ -x "$PDMOUNT" ]] || PDMOUNT=$repo/target/debug/pdmount
fi
REMOTE=$MNT/pdmount-m1
LOCAL=$DATASET/pdmount-m1

pd() { "$PDMOUNT" --profile "$PROFILE" "${secrets_flag[@]}" "$@"; }
step() { printf '\n==> %s\n' "$*"; }
fail() { echo "FAIL: $*" >&2; exit 1; }

# `status` exits 1 without an active session (always so for the fake).
status_text() { pd status 2>/dev/null || true; }
is_mounted() { status_text | grep -q '^Mount: *mounted'; }
mount_now() { pd mount "${backend_flag[@]}" "$MNT"; }
daemon_pid() { status_text | sed -n 's/.*daemon pid \([0-9]*\).*/\1/p'; }

ensure_dataset() {
    if [[ ! -f "$LOCAL/MANIFEST.sha256" ]]; then
        step "generating the local dataset in $DATASET"
        rm -rf "$DATASET"
        mkdir -p "$DATASET"
        "$repo/scripts/testdata/make-m1-dataset.sh" "${dataset_flags[@]}" "$DATASET"
    fi
}

# The fake drive starts as a copy of the dataset. Reseeding it makes the
# profile's stored state stale, so that is reset too.
ensure_fake_drive() {
    [[ $BACKEND == fake ]] || return 0
    "$PDMOUNT" fake --help >/dev/null 2>&1 ||
        fail "$PDMOUNT has no fake backend; build it with \`cargo build --features fake-backend\`"
    if [[ ! -f "$FAKE_DIR/.seeded-m1" ]]; then
        step "seeding the fake drive $FAKE_DIR from $DATASET"
        if is_mounted; then pd unmount "$MNT"; fi
        ensure_dataset
        rm -rf "$FAKE_DIR"
        "$PDMOUNT" fake seed --from "$DATASET" "$FAKE_DIR"
        touch "$FAKE_DIR/.seeded-m1"
        pd reset --keep-login >/dev/null
    fi
}

# Mounts if needed. Scenarios that read the M1 dataset call require_m1 after it.
prepare_mount() {
    case "$VERIFY_MODE" in
        live | remount | crash | fresh) ;;
        *) fail "unknown VERIFY_MODE=$VERIFY_MODE (live, remount, crash, fresh)" ;;
    esac
    ensure_fake_drive
    is_mounted || mount_now
}

require_m1() {
    [[ -d "$REMOTE" ]] || fail "$REMOTE not found; upload pdmount-m1 to the root of My Files first"
}

# Waits until no upload or namespace change is pending (`pdmount status`
# reports `Queue: 0 pending`). Daemons without a queue line pass at once.
wait_queue_empty() {
    local deadline=$((SECONDS + ${QUEUE_TIMEOUT:-600})) line
    while :; do
        line=$(status_text | grep '^Queue:' || true)
        [[ -z "$line" || "$line" =~ ^Queue:\ +0\ pending ]] && return 0
        ((SECONDS < deadline)) || fail "the queue did not drain within ${QUEUE_TIMEOUT:-600} s: $line"
        sleep 1
    done
}

verify_cycle() {
    case "$VERIFY_MODE" in
        live) ;;
        remount)
            step "remount: unmount (draining the queue) and mount"
            pd unmount --wait "$MNT"
            mount_now
            ;;
        crash)
            step "crash: kill -9 the daemon, unmount --force, mount, drain"
            local pid
            pid=$(daemon_pid)
            [[ -n "$pid" ]] || fail "no daemon pid in pdmount status"
            kill -9 "$pid"
            while kill -0 "$pid" 2>/dev/null; do sleep 0.1; done
            pd unmount --force "$MNT"
            mount_now
            wait_queue_empty
            ;;
        fresh)
            step "fresh: drain, unmount, reset local state, mount"
            wait_queue_empty
            pd unmount --wait "$MNT"
            pd reset --keep-login
            mount_now
            ;;
    esac
}

check_hot_path() {
    local calls
    calls=$(pd stats | awk '$1 == "hot_path_network_calls" { print $2 }')
    echo "hot_path_network_calls = $calls"
    [[ "$calls" == "0" ]] || fail "hot_path_network_calls is $calls, expected 0"
}

# Sets $work to a scratch directory removed on exit. Not for use in $(...):
# the trap must be set in the script's own shell.
make_workdir() {
    work=$(mktemp -d "$SCENARIO_ROOT/work.XXXXXX")
    trap 'rm -rf "$work"' EXIT
}

# ---- M2 write scenarios ----

# Writes happen only below pdmount-test/ (D028).
TEST_ROOT=$MNT/pdmount-test

# Sets $area to an empty scenario directory on the mount.
scenario_area() { # <name>
    area=$TEST_ROOT/$1
    mkdir -p "$TEST_ROOT"
    rm -rf "$area"
    mkdir -p "$area"
    # Only changes from here on count for remote_temp_names.
    fake_mark=$(fake_change_mark)
}

# A deterministic tree: nested directories, small files with modes
# 0644/0755/0600 and fixed mtimes, and one larger file.
make_tree() { # <dir> <small files> <big MiB>
    python3 -I - "$1" "$2" "$3" <<'PY'
import os, random, sys
root, files, big = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
rng = random.Random(20261020)
dirs = [""]
for i in range(max(1, files // 15)):
    parent = rng.choice(dirs)
    if parent.count("/") >= 3:
        parent = ""
    d = os.path.join(parent, f"dir-{i:03d}")
    dirs.append(d)
    os.makedirs(os.path.join(root, d), exist_ok=True)
for i in range(files):
    path = os.path.join(root, rng.choice(dirs), f"f-{i:04d}.dat")
    with open(path, "wb") as f:
        f.write(rng.randbytes(rng.choice([0, 1, 100, 4096, 4097, rng.randrange(0, 70000)])))
    os.chmod(path, rng.choice([0o644, 0o755, 0o600]))
    os.utime(path, (1700000000 + i * 37, 1700000000 + i * 37))
if big:
    with open(os.path.join(root, "big.bin"), "wb") as f:
        for _ in range(big):
            f.write(rng.randbytes(1 << 20))
    os.utime(os.path.join(root, "big.bin"), (1700000123, 1700000123))
PY
}

# Directory mtimes are local-only (D013): fresh mounts show creation times.
dir_time_flags() {
    if [[ $VERIFY_MODE == fresh ]]; then echo --omit-dir-times; fi
}

# Fails unless rsync finds nothing to do, by mtime and size and by checksum.
assert_same_tree() { # <src> <dst>
    local flags out
    flags=$(dir_time_flags)
    # shellcheck disable=SC2086
    out=$(rsync -ani $flags "$1/" "$2/")
    [[ -z "$out" ]] || fail "rsync -ani $flags $1/ $2/ would change: $out"
    # shellcheck disable=SC2086
    out=$(rsync -ani --checksum $flags "$1/" "$2/")
    [[ -z "$out" ]] || fail "rsync -ani --checksum $1/ $2/ would change: $out"
}

# The fake's change log (BACKEND=fake only): `<number> <kind> <link> <name>`,
# only changes numbered above the first argument (default 0).
fake_changes() { # [after]
    [[ $BACKEND == fake ]] || return 0
    "$PDMOUNT" fake changes --after "${1:-0}" "$FAKE_DIR"
}

# The number of the latest change, to pass to fake_changes later.
fake_change_mark() {
    local last
    last=$(fake_changes | tail -n 1)
    echo "${last%% *}" | grep -E '^[0-9]+$' || echo 0
}

# Names created remotely that look like rsync or git temporaries and no
# longer exist on the mount: those must never have been uploaded (§7.3). A
# temporary that still exists (left by a process killed with the daemon) is
# a real file.
remote_temp_names() {
    local name
    fake_changes "${fake_mark:-0}" |
        awk '$2 == "created-file" || $2 == "renamed" { print $4 }' |
        grep -E '^\..+\.[A-Za-z0-9]{6}$|\.lock$|^tmp_obj_|^tmp_pack_' | sort -u |
        while read -r name; do
            [[ -n "$(find "$TEST_ROOT" -name "$name" -print -quit)" ]] || echo "$name"
        done || true
}

git_env() {
    export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1
    # No detached background maintenance: runs stay deterministic.
    export GIT_CONFIG_COUNT=2 GIT_CONFIG_KEY_0=gc.auto GIT_CONFIG_VALUE_0=0 \
        GIT_CONFIG_KEY_1=maintenance.auto GIT_CONFIG_VALUE_1=false
    export GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@t GIT_COMMITTER_NAME=t GIT_COMMITTER_EMAIL=t@t
    export GIT_AUTHOR_DATE='2026-10-20T00:00:00Z' GIT_COMMITTER_DATE='2026-10-20T00:00:00Z'
}
