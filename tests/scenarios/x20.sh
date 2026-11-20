#!/usr/bin/env bash
# X20 (DESIGN.md §14.4, D137, M6.4): the slow-download notification. One
# daemon on a fake drive of its own, with My Files and a Photos root, a
# private session bus with the notification stub, and a slow network
# profile (80 ms, 500 kB/s: a 4 MiB block takes about 8 s). Checks that a
# read that waits 5 s for its blocks names its file and size in one
# "PdMount: downloading" notification, closed once the read got its bytes;
# that a second slow read of the file sends nothing (one per file per 10
# min); that a read from the cache sends nothing; that the reads of a scan
# (a burst of opens, as a thumbnailer's) send nothing although they wait as
# long; that a name with markup characters is escaped; that a photo of the
# Photos tree is named too; and that `notify.downloads = false` sends none.
# Fake, live mode only, its own profile and fake drive.
source "$(dirname "$0")/lib.sh"
[[ $BACKEND == fake ]] || { echo "x20 needs a fake drive of its own; skipped for BACKEND=$BACKEND"; exit 0; }
need_dbus_daemon x20
x_live_only
make_workdir

PROFILE=fake-x20
MNT=$SCENARIO_ROOT/fake-mnt-x20
PHOTOS=$SCENARIO_ROOT/fake-photos-x20
FAKE_DIR=$SCENARIO_ROOT/fake-drive-x20
backend_flag=(--backend "fake:$FAKE_DIR")
config_file=$XDG_CONFIG_HOME/pdmount/$PROFILE.toml
# Mid-June 2024, so no time zone moves the photo out of its month.
CAPTURED=1718452800
notify_log=$work/notify.log
start_session_bus x20
start_notify_stub "$bus_address" "$notify_log"

x20_cleanup() {
    "$PDMOUNT" fake profile clear "$FAKE_DIR" >/dev/null 2>&1 || true
    if is_mounted; then pd unmount "$MNT" >/dev/null 2>&1 || true; fi
    if mounted_at "$PHOTOS"; then fusermount3 -u "$PHOTOS" 2>/dev/null || fusermount3 -uz "$PHOTOS" || true; fi
    scenario_cleanup
}
trap x20_cleanup EXIT

mounted_at() { awk -v mnt="$1" '$2 == mnt { found = 1 } END { exit !found }' /proc/self/mounts; }
wait_for() { # <what> <seconds> <command...>
    local what=$1 limit=$2 start=$SECONDS
    shift 2
    until "$@"; do
        ((SECONDS - start < limit)) || fail "timed out waiting for $what; status: $(status_text)"
        sleep 0.5
    done
}
status_has() { status_text | grep -Eq "$1"; }
names() { ls -1 "$1" | tr '\n' '|'; }
lists() { [[ $(names "$1") == "$2" ]]; } # <dir> <names>
root_line() { status_has "^Root: +$PHOTOS \\(photos\\): $1\$"; }
mount_x20() { DBUS_SESSION_BUS_ADDRESS=$bus_address mount_now; }
slow() { "$PDMOUNT" fake profile --rtt 80 --down 500000 "$FAKE_DIR"; }
fast() { "$PDMOUNT" fake profile clear "$FAKE_DIR"; }

# The stub's download notifications and closes, as `<op> <id> <body>` lines.
downloads() {
    python3 -I - "$notify_log" <<'PY'
import json, sys
for line in open(sys.argv[1]):
    try:
        entry = json.loads(line)
    except ValueError:
        continue
    if entry.get("op") == "notify" and entry.get("summary") == "PdMount: downloading":
        print("notify", entry["id"], entry["body"])
    elif entry.get("op") == "close":
        print("close", entry["id"])
PY
}
notified() { downloads | grep -c '^notify ' || true; }
closed() { downloads | grep -q "^close $1\$"; } # <id>
# The first 64 KiB of a file, past the kernel's page cache; sets $took (s).
read_head() { # <file>
    local start
    start=$(now_ms)
    dd if="$1" of=/dev/null bs=64k count=1 iflag=direct status=none || fail "reading $1"
    took=$((($(now_ms) - start) / 1000))
}
# A read that waited for its blocks: it took at least 5 s.
read_slowly() { # <file>
    read_head "$1"
    ((took >= 5)) || fail "$1 read in $took s: not slow; the test proves nothing"
    echo "$(basename "$1"): read in $took s"
}
# Exactly one more notification, with this body, and its close.
expect_one() { # <count before> <body>
    local line id
    wait_for "the notification" 10 test "$(notified)" -gt "$1"
    expect_eq "notifications" "$(notified)" $(($1 + 1))
    line=$(downloads | grep '^notify ' | tail -n 1)
    id=$(cut -d' ' -f2 <<<"$line")
    expect_eq "the body" "$(cut -d' ' -f3- <<<"$line")" "$2"
    wait_for "the close of notification $id" 10 closed "$id"
}

step "a fake drive of its own: files in My Files, a photo; the profile mounts My Files and Photos"
if is_mounted; then pd unmount "$MNT"; fi
if mounted_at "$PHOTOS"; then fusermount3 -u "$PHOTOS"; fi
remove_fake_dir "$FAKE_DIR"
mkdir -p "$work/seed/pdmount-test"
for name in big.bin cached.bin warm.bin '<b>&.bin' s1.bin s2.bin s3.bin off.bin; do
    head -c $((6 << 20)) /dev/urandom >"$work/seed/pdmount-test/$name"
done
head -c $((6 << 20)) /dev/urandom >"$work/clip.mp4"
"$PDMOUNT" fake seed --from "$work/seed" "$FAKE_DIR"
touch "$FAKE_DIR/.seeded-x20"
"$PDMOUNT" fake photo add --name clip.mp4 --from "$work/clip.mp4" --captured "$CAPTURED" "$FAKE_DIR" >/dev/null
MONTH=$(date -d "@$CAPTURED" +%Y/%m)
rm -f "$config_file"
fake_config "$PROFILE" dev-x20 "$MNT"
# Three different files opened within the window begin a scan (D111).
sed -i '/^\[scan\]/a opens = 3' "$config_file"
pd reset --keep-login >/dev/null
cat >>"$config_file" <<EOF

[[roots]]
kind = "photos"
mountpoint = "$PHOTOS"
EOF
saved_config=$(cat "$config_file")
mount_x20
wait_for "the Photos root" 30 root_line "mounted, read-only"
wait_for "the names of $MONTH" 30 lists "$PHOTOS/$MONTH" "clip.mp4|"
ls -l "$MNT/pdmount-test" "$PHOTOS/$MONTH" >/dev/null
# Cached before the network slows down.
dd if="$MNT/pdmount-test/cached.bin" of=/dev/null bs=1M iflag=direct status=none
dd if="$MNT/pdmount-test/warm.bin" of=/dev/null bs=1M iflag=direct status=none
slow
# Opens are counted for the scan guard: the next ones come after its window.
sleep 6

step "a read from the cache: no notification"
read_head "$MNT/pdmount-test/cached.bin"
((took < 5)) || fail "the cached read took $took s"
expect_eq "notifications" "$(notified)" 0

step "a slow read names the file and its size; the notification closes when the read has its bytes"
read_slowly "$MNT/pdmount-test/big.bin"
expect_one 0 "big.bin (6.0 MiB) is downloading; it opens as its blocks arrive."

step "a second slow read of the same file within 10 minutes: nothing"
pd cache wipe "$MNT/pdmount-test/big.bin" >/dev/null
read_slowly "$MNT/pdmount-test/big.bin"
sleep 1
expect_eq "notifications" "$(notified)" 1

step "a name with markup characters is escaped"
read_slowly "$MNT/pdmount-test/<b>&.bin"
expect_one 1 "&lt;b&gt;&amp;.bin (6.0 MiB) is downloading; it opens as its blocks arrive."

step "a scan (three files opened at once): its reads wait as long, and send nothing"
# Two files from the cache, then three cold ones at once: the third open
# begins the scan, so the cold files' first reads are scan reads.
python3 -I - "$MNT/pdmount-test" <<'PY' || fail "the scan's reads"
import os, sys, threading, time
folder = sys.argv[1]
for name in ("cached.bin", "warm.bin"):
    with open(os.path.join(folder, name), "rb") as f:
        f.read(4096)
files = [open(os.path.join(folder, name), "rb") for name in ("s1.bin", "s2.bin", "s3.bin")]
start = time.monotonic()
threads = [threading.Thread(target=f.read, args=(4096,)) for f in files]
for thread in threads:
    thread.start()
for thread in threads:
    thread.join()
took = time.monotonic() - start
print(f"the scan's reads took {took:.1f} s")
if took < 5:
    sys.exit("the scan's reads were not slow; the test proves nothing")
PY
sleep 1
expect_eq "notifications" "$(notified)" 2
# The scan ends at the next open after the window.
sleep 6

step "a photo of the Photos tree is named too"
read_slowly "$PHOTOS/$MONTH/clip.mp4"
expect_one 2 "clip.mp4 (6.0 MiB) is downloading; it opens as its blocks arrive."

step "notify.downloads = false: a slow read sends nothing"
fast
pd unmount "$MNT" >/dev/null
printf '%s\n\n[notify]\ndownloads = false\n' "$saved_config" >"$config_file"
mount_x20
wait_for "the Photos root" 30 root_line "mounted, read-only"
ls -l "$MNT/pdmount-test" >/dev/null
slow
read_slowly "$MNT/pdmount-test/off.bin"
sleep 1
expect_eq "notifications" "$(notified)" 3

step "the daemon stops"
fast
pd unmount "$MNT" >/dev/null
printf '%s\n' "$saved_config" >"$config_file"
mounted_at "$PHOTOS" && fail "$PHOTOS is still mounted after the daemon's stop"
mounted_at "$MNT" && fail "$MNT is still mounted"
echo "x20: PASS"
