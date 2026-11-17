#!/usr/bin/env bash
# X17 (DESIGN.md §14.5, D080, D081, D113): a Photos mount on a fake drive of
# its own. A few hundred photos over four years, a Live Photo (still and
# video), and one photo from a local file. Checks the years and months
# (from the timeline alone), a month's names (when it is opened), a read
# and its SHA-256, EROFS on writes, a new photo and a deleted one after
# their events, the status line, and a remount of the Photos root that reads
# neither the timeline nor the names again (the log says so). The opened
# month's thumbnails (M5.13b, D115) go to $XDG_CACHE_HOME/thumbnails, under
# $SCENARIO_ROOT, with the URI and mtime the mount reports, and are not
# written again on a second open nor after the remount.
# Fake, live mode only, its own profile and fake drive.
source "$(dirname "$0")/lib.sh"
[[ $BACKEND == fake ]] || { echo "x17 needs a fake drive of its own; skipped for BACKEND=$BACKEND"; exit 0; }
x_live_only
make_workdir

PROFILE=fake-x17
MNT=$SCENARIO_ROOT/fake-mnt-x17
PHOTOS=$SCENARIO_ROOT/fake-photos-x17
FAKE_DIR=$SCENARIO_ROOT/fake-drive-x17
backend_flag=(--backend "fake:$FAKE_DIR")
config_file=$XDG_CONFIG_HOME/pdmount/$PROFILE.toml
# The Photos volume is the user's own: it polls every sync.poll_interval
# (30 s, the least); a change shows within one poll plus its requests.
EVENT_WAIT=${EVENT_WAIT:-45}
COUNT=300
YEARS=4
# 2024-06-15 12:00 UTC; the seeded photos go back four years from it.
UNTIL=1718452800
STEP=$((YEARS * 365 * 86400 / COUNT))
# The Live Photo, the local photo and the new one: in the month of the
# newest seeded photo, which is opened before them.
LIVE_AT=$((UNTIL - 3600))
LOCAL_AT=$((UNTIL - 7200))
NEW_AT=$((UNTIL - 10800))

x17_cleanup() {
    if is_mounted; then pd unmount "$MNT" >/dev/null 2>&1 || true; fi
    if mounted_at "$PHOTOS"; then fusermount3 -u "$PHOTOS" 2>/dev/null || fusermount3 -uz "$PHOTOS" || true; fi
    scenario_cleanup
}
trap x17_cleanup EXIT

mounted_at() { awk -v mnt="$1" '$2 == mnt { found = 1 } END { exit !found }' /proc/self/mounts; }
# The freedesktop thumbnail cache (M5.13b): never the user's own.
THUMBS=$XDG_CACHE_HOME/thumbnails
[[ $THUMBS == "$SCENARIO_ROOT"/* ]] || fail "XDG_CACHE_HOME ($XDG_CACHE_HOME) is not under $SCENARIO_ROOT"
SIZES=(normal:128 large:256 x-large:512)
# The URI a file manager hashes (GLib's escaping) and the thumbnail's path.
thumb_uri() { python3 -I -c 'import sys, urllib.parse; print("file://" + urllib.parse.quote(sys.argv[1], safe="/!$&'"'"'()*+,:=@"))' "$1"; }
thumb_file() { # <size> <path in the mount>
    local uri
    uri=$(thumb_uri "$2")
    printf '%s/%s/%s.png\n' "$THUMBS" "$1" "$(printf %s "$uri" | md5sum | cut -d' ' -f1)"
}
# Every size of every photo of a month has its thumbnail.
month_thumbs() { # <year/month>
    local name size
    for name in $(ls "$PHOTOS/$1"); do
        for size in "${SIZES[@]}"; do
            [[ -f $(thumb_file "${size%%:*}" "$PHOTOS/$1/$name") ]] || return 1
        done
    done
}
# A thumbnail's PNG: its Thumb::URI, its Thumb::MTime, and its longest side.
check_thumb() { # <png> <uri> <mtime> <side>
    python3 -I - "$@" <<'PY' || fail "thumbnail $1"
import struct, sys
path, uri, mtime, side = sys.argv[1:5]
data = open(path, "rb").read()
if data[:8] != b"\x89PNG\r\n\x1a\n":
    sys.exit(f"{path}: not a PNG")
pos, text, size = 8, {}, None
while pos + 8 <= len(data):
    length, kind = struct.unpack(">I4s", data[pos:pos + 8])
    body = data[pos + 8:pos + 8 + length]
    pos += 12 + length
    if kind == b"IHDR":
        size = struct.unpack(">II", body[:8])
    elif kind == b"tEXt":
        key, value = body.split(b"\0", 1)
        text[key.decode("latin-1")] = value.decode("latin-1")
    elif kind == b"IEND":
        break
problems = []
if text.get("Thumb::URI") != uri:
    problems.append(f"Thumb::URI {text.get('Thumb::URI')!r}, not {uri!r}")
if text.get("Thumb::MTime") != mtime:
    problems.append(f"Thumb::MTime {text.get('Thumb::MTime')!r}, not {mtime!r}")
if size is None or max(size) != int(side):
    problems.append(f"{size}, not {side} on the long side")
if problems:
    sys.exit(f"{path}: " + "; ".join(problems))
PY
}
# Inode and mtime of the thumbnails of some photos of a month: a rewrite
# (a rename over the file) changes them.
thumb_stamps() { # <year/month> <name...>
    local month=$1 name size
    shift
    for name in "$@"; do
        for size in "${SIZES[@]}"; do
            stat -c '%i %Y' "$(thumb_file "${size%%:*}" "$PHOTOS/$month/$name")"
        done
    done
}
wait_for() { # <what> <seconds> <command...>
    local what=$1 limit=$2 start=$SECONDS
    shift 2
    until "$@"; do
        ((SECONDS - start < limit)) || fail "timed out waiting for $what; status: $(status_text)"
        sleep 0.5
    done
}
status_has() { status_text | grep -Eq "$1"; }
logged() { log_since_mark | grep -Eq "$1"; }
lists() { [[ $(names "$1") == "$2" ]]; } # <dir> <names>
root_line() { status_has "^Root: +$PHOTOS \\(photos\\): $1\$"; }
names() { ls -1 "$1" | tr '\n' '|'; }
has() { [[ -e $1 ]]; }
gone() { [[ ! -e $1 ]]; }
expect_erofs() { # <what> <command...>
    local what=$1 out
    shift
    if out=$("$@" 2>&1); then fail "$what succeeded"; fi
    grep -q 'Read-only file system' <<<"$out" || fail "$what: not EROFS: $out"
}
# The local year/month of a capture time, as the mount places it.
month_of() { date -d "@$1" +%Y/%m; }
# The years and the months of the seeded photos, and the names in one month.
expected_years() {
    local i
    for ((i = 0; i < COUNT; i++)); do date -d "@$((UNTIL - i * STEP))" +%Y; done | sort -u | tr '\n' '|'
}
expected_months() { # <year>
    local i
    for ((i = 0; i < COUNT; i++)); do date -d "@$((UNTIL - i * STEP))" +%Y/%m; done |
        sed -n "s|^$1/||p" | sort -u | tr '\n' '|'
}
expected_seeded() { # <year/month>
    local i
    for ((i = 0; i < COUNT; i++)); do
        [[ $(month_of $((UNTIL - i * STEP))) == "$1" ]] && printf 'IMG_%05d.jpg\n' "$i"
    done
}

step "a fake drive of its own with $COUNT photos over $YEARS years; the profile mounts Photos"
if is_mounted; then pd unmount "$MNT"; fi
if mounted_at "$PHOTOS"; then fusermount3 -u "$PHOTOS"; fi
remove_fake_dir "$FAKE_DIR"
rm -rf "$THUMBS"
mkdir -p "$work/seed/pdmount-test"
printf hello >"$work/seed/pdmount-test/hello"
head -c $((300 * 1024)) /dev/urandom >"$work/local.jpg"
head -c 5000 /dev/urandom >"$work/live.heic"
head -c 9000 /dev/urandom >"$work/live.mov"
head -c 7000 /dev/urandom >"$work/new.jpg"
"$PDMOUNT" fake seed --from "$work/seed" "$FAKE_DIR"
touch "$FAKE_DIR/.seeded-x17"
"$PDMOUNT" fake photo seed --count "$COUNT" --years "$YEARS" --until "$UNTIL" "$FAKE_DIR"
"$PDMOUNT" fake photo add --name LIVE_0001.HEIC --from "$work/live.heic" --captured "$LIVE_AT" --tag 3 "$FAKE_DIR" >/dev/null
"$PDMOUNT" fake photo add --name LIVE_0001.MOV --from "$work/live.mov" --captured "$LIVE_AT" --tag 2 \
    --related-to LIVE_0001.HEIC "$FAKE_DIR" >/dev/null
"$PDMOUNT" fake photo add --name LOCAL_0001.jpg --from "$work/local.jpg" --captured "$LOCAL_AT" "$FAKE_DIR" >/dev/null
rm -f "$config_file"
fake_config "$PROFILE" dev-x17 "$MNT"
pd reset --keep-login >/dev/null
cat >>"$config_file" <<EOF

[[roots]]
kind = "photos"
mountpoint = "$PHOTOS"
EOF

step "mount: years, then months, from the timeline alone"
log_mark
mount_now
wait_for "the Photos root" 30 root_line "mounted, read-only"
expect_eq "the years" "$(names "$PHOTOS")" "$(expected_years)"
expect_eq "the top level's mode" "$(stat -c %a "$PHOTOS")" 555
for year in $(ls "$PHOTOS"); do
    expect_eq "the months of $year" "$(names "$PHOTOS/$year")" "$(expected_months "$year")"
done
wait_for "the timeline read in the log" 10 logged 'photos: timeline read'

step "a month's names when it is opened; the Live Photo's video beside it"
month=$(month_of "$UNTIL")
[[ $(month_of "$LIVE_AT") == "$month" ]] || fail "the test's times are not in one month"
want=$({ expected_seeded "$month"; printf '%s\n' LIVE_0001.HEIC LIVE_0001.MOV LOCAL_0001.jpg; } | sort | tr '\n' '|')
wait_for "the names of $month" 30 lists "$PHOTOS/$month" "$want"
wait_for "the month's names in the log" 10 logged 'photos: names of a month fetched'
other=$(month_of $((UNTIL - (COUNT - 1) * STEP)))
want_other=$(expected_seeded "$other" | sort | tr '\n' '|')
wait_for "the oldest month's names" 30 lists "$PHOTOS/$other" "$want_other"

step "the opened month's thumbnails, in \$XDG_CACHE_HOME/thumbnails"
wait_for "the thumbnails of $month" 60 month_thumbs "$month"
mapfile -t thumbed < <(ls "$PHOTOS/$month")
for name in "${thumbed[@]}"; do
    uri=$(thumb_uri "$PHOTOS/$month/$name")
    mtime=$(stat -c %Y "$PHOTOS/$month/$name")
    for size in "${SIZES[@]}"; do
        check_thumb "$(thumb_file "${size%%:*}" "$PHOTOS/$month/$name")" "$uri" "$mtime" "${size#*:}"
    done
done
expect_eq "a thumbnail's mode" "$(stat -c %a "$(thumb_file normal "$PHOTOS/$month/LOCAL_0001.jpg")")" 600
expect_eq "the thumbnail folders' mode" "$(stat -c %a "$THUMBS" "$THUMBS/normal" "$THUMBS/x-large" | sort -u)" 700
wait_for "the thumbnails in the log" 10 logged 'photos: thumbnails written'
stamps=$(thumb_stamps "$month" "${thumbed[@]}")
# The Live Photo goes before the remount; its thumbnails stay in the cache.
kept=()
for name in "${thumbed[@]}"; do [[ $name == LIVE_* ]] || kept+=("$name"); done
kept_stamps=$(thumb_stamps "$month" "${kept[@]}")
ls "$PHOTOS/$month" >/dev/null
sleep 3
expect_eq "the thumbnails after a second open of the month" "$(thumb_stamps "$month" "${thumbed[@]}")" "$stamps"

step "a read, with its SHA-256; the photo's attributes"
expect_eq "LOCAL_0001.jpg" "$(sha256sum <"$PHOTOS/$month/LOCAL_0001.jpg")" "$(sha256sum <"$work/local.jpg")"
cmp -s "$work/live.mov" "$PHOTOS/$month/LIVE_0001.MOV" || fail "the Live Photo's video differs"
expect_eq "its size" "$(stat -c %s "$PHOTOS/$month/LOCAL_0001.jpg")" $((300 * 1024))
expect_eq "its mode" "$(stat -c %a "$PHOTOS/$month/LOCAL_0001.jpg")" 444
expect_eq "its mtime (the capture time)" "$(stat -c %Y "$PHOTOS/$month/LOCAL_0001.jpg")" "$LOCAL_AT"
first=$(ls "$PHOTOS/$month" | grep -m1 '^IMG_')
sha256sum "$PHOTOS/$month/$first" >/dev/null || fail "reading $first"

step "every change is EROFS"
photo=$PHOTOS/$month/LOCAL_0001.jpg
expect_erofs "a new file" touch "$PHOTOS/$month/new.jpg"
expect_erofs "a change of a photo" sh -c "printf x >>'$photo'"
expect_erofs "a truncation" truncate -s 0 "$photo"
expect_erofs "a folder in a month" mkdir "$PHOTOS/$month/new"
expect_erofs "a folder at the top" mkdir "$PHOTOS/1999"
expect_erofs "a rename" mv "$photo" "$PHOTOS/$month/moved.jpg"
expect_erofs "a delete" rm "$photo"
expect_erofs "a chmod" chmod 600 "$photo"
expect_erofs "a delete of a month" rmdir "$PHOTOS/$other"
expect_eq "My Files" "$(cat "$MNT/pdmount-test/hello")" hello

step "a new photo and a deleted one after their events"
"$PDMOUNT" fake photo add --name NEW_0001.jpg --from "$work/new.jpg" --captured "$NEW_AT" "$FAKE_DIR" >/dev/null
"$PDMOUNT" fake photo delete --name LIVE_0001.HEIC "$FAKE_DIR"
log_mark
wait_for "the new photo" "$EVENT_WAIT" has "$PHOTOS/$month/NEW_0001.jpg"
cmp -s "$work/new.jpg" "$PHOTOS/$month/NEW_0001.jpg" || fail "the new photo differs"
wait_for "the deleted Live Photo and its video to go" "$EVENT_WAIT" gone "$PHOTOS/$month/LIVE_0001.MOV"
gone "$PHOTOS/$month/LIVE_0001.HEIC" || fail "the Live Photo's still is still there"
expect_log 'photos: new photos from the events' "the new photo's event"

step "status: the Photos tree's line"
total=$((COUNT + 2))
status_has "^  Photos: read-only, 0 pending, 0 parked, last poll [0-9]+ s ago, every 30 s, cache .*, $total photos, [0-9]+ named, timeline read" ||
    fail "the Photos line: $(status_text)"
named=$(status_text | sed -n 's/^  Photos: .*, \([0-9]*\) named, .*/\1/p')

step "a remount of the Photos root reads neither the timeline nor the names again"
pd unmount "$PHOTOS" >/dev/null
mounted_at "$PHOTOS" && fail "$PHOTOS is still mounted"
log_mark
pd mount "$PHOTOS" >/dev/null
wait_for "the Photos root again" 30 root_line "mounted, read-only"
expect_log 'photos: timeline and names loaded from the store; the timeline is not read again' "the stored timeline"
expect_log "photos=$total named=$named" "the stored counts"
expect_eq "the years again" "$(names "$PHOTOS")" "$(expected_years)"
expect_eq "the month again" "$(names "$PHOTOS/$month")" \
    "$({ expected_seeded "$month"; printf '%s\n' LOCAL_0001.jpg NEW_0001.jpg; } | sort | tr '\n' '|')"
expect_eq "LOCAL_0001.jpg again" "$(sha256sum <"$PHOTOS/$month/LOCAL_0001.jpg")" "$(sha256sum <"$work/local.jpg")"
if log_since_mark | grep -Eq 'photos: (reading the timeline|names of a month fetched)'; then
    fail "the remount read the timeline or names again: $(log_since_mark | grep -E 'photos:')"
fi
# The month opened again in a new session: the new photo gets its
# thumbnails, those already there are not written again.
wait_for "the new photo's thumbnails" 60 month_thumbs "$month"
expect_eq "the thumbnails after the remount" "$(thumb_stamps "$month" "${kept[@]}")" "$kept_stamps"

step "the daemon's stop unmounts the Photos root too"
pd unmount "$MNT" >/dev/null
mounted_at "$PHOTOS" && fail "$PHOTOS is still mounted after the daemon's stop"
mounted_at "$MNT" && fail "$MNT is still mounted"
echo "x17: PASS"
