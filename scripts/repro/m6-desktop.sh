#!/usr/bin/env bash
# M6: the notification checks by hand on the user's desktops, GNOME and KDE (docs/plans/m6.md,
# "Running the M6 checks"). A fake drive only: nothing touches Proton.
#
# Run as the desktop user, in a terminal inside the graphical session, with a test build of
# `pdmount` (`--features fake-backend`) on PATH. The daemon is started from this terminal, so
# it uses this session's bus, as a daemon started at login does. Each step says what should
# appear on screen and asks what did; the answers go into the results.
#
# Usage:
#   m6-desktop.sh setup      config, a fake drive, the mount on this session's bus; `doctor`
#   m6-desktop.sh conflict   an edit_edit conflict: its notification, then Show (click it)
#   m6-desktop.sh missed     conflicts made while the daemon had no notification service: shown
#                            when its next start finds one (as at a login), and when the desktop's
#                            bus is handed over (`session attach`, as the login entry does)
#   m6-desktop.sh slow       a slow network, a large file opened: "PdMount: downloading" and its close
#   m6-desktop.sh settings   each [notify] key off in turn: conflicts, conflict_names, downloads
#   m6-desktop.sh report     the results file, to paste back
#   m6-desktop.sh cleanup    unmount, remove the config, the fake drive and the mountpoint
#
# State and results: $XDG_STATE_HOME/pdmount-m6/ (results.txt). Environment: PROFILE (default m6),
# FAKE_DIR (default $XDG_DATA_HOME/pdmount-m6-fake). For the script's test only: M6_ANSWER answers
# every question and confirmation with its value, and M6_SHOW_WAIT (default 120) is how many
# seconds `conflict` waits for the click on Show.
set -uo pipefail

: "${HOME:=$(getent passwd "$(id -u)" | cut -d: -f6)}"
profile=${PROFILE:-m6}
data_home=${XDG_DATA_HOME:-$HOME/.local/share}
config_home=${XDG_CONFIG_HOME:-$HOME/.config}
state_home=${XDG_STATE_HOME:-$HOME/.local/state}
fake_dir=${FAKE_DIR:-$data_home/pdmount-$profile-fake}
mnt=$HOME/PdMount-$profile # betterleaks:allow
config=$config_home/pdmount/$profile.toml
state=$state_home/pdmount-m6
results=$state/results.txt
daemon_log=$state_home/pdmount/$profile.log
desktop=${XDG_CURRENT_DESKTOP:-unknown}
# The profile's own folder on the fake drive, and a large file in it for `slow`.
remote=/m6
clip=clip.mp4

die() { echo "m6-desktop.sh: $*" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }
# File secrets: the profile never touches the keyring, so no dialog opens (D029).
pd() { pdmount --profile "$profile" --force-file-secrets "$@"; }

out() { tee -a "$results"; }
section() { printf '\n===== %s  %s  (%s)\n' "$*" "$(date '+%F %T %Z')" "$desktop" | out; }
record() {
    printf '$ %s\n' "$*" | out
    "$@" 2>&1 | out
    local rc=${PIPESTATUS[0]}
    printf '(exit %s)\n' "$rc" | out
    return "$rc"
}
# A question about the screen; the answer goes into the results.
ask() { # <question>
    local answer
    if [[ -n ${M6_ANSWER:-} ]]; then
        answer=$M6_ANSWER
    else
        read -r -p "$1 [y/n, or a few words] " answer
    fi
    printf 'Q: %s\nA: %s\n' "$1" "$answer" | out
}
confirm() { # <prompt>
    local answer=${M6_ANSWER:-}
    [[ -n $answer ]] || read -r -p "$1 [y/N] " answer
    [[ $answer == [yY]* ]]
}

mounted() { mountpoint -q "$mnt"; }
need_mounted() { mounted || die "$mnt is not mounted: run \`$0 setup\` first"; }
status_text() { timeout 15 pdmount --profile "$profile" --force-file-secrets status 2>/dev/null; }

# Waits until the queue is empty (two minutes at most).
wait_queue() {
    local i line
    for ((i = 0; i < 120; i++)); do
        line=$(status_text | grep '^Queue:' || true)
        [[ -z $line || $line =~ ^Queue:\ +0\ pending ]] && return 0
        sleep 1
    done
    die "the queue did not drain: $line"
}

# The daemon's log from the last log_mark on.
log_mark() { log_offset=$(stat -c %s "$daemon_log" 2>/dev/null || echo 0); }
log_since_mark() { tail -c +"$((${log_offset:-0} + 1))" "$daemon_log" 2>/dev/null | sed 's/\x1b\[[0-9;]*m//g'; }
wait_log() { # <extended regex> <count> <seconds>; false when they did not come
    local deadline=$((SECONDS + $3))
    until (($(log_since_mark | grep -Ec "$1") >= $2)); do
        ((SECONDS < deadline)) || return 1
        sleep 0.5
    done
}
show_log() { # <extended regex>
    echo "--- daemon log" | out
    log_since_mark | grep -E "$1" | tail -n 20 | out
}

# Starts the daemon with <bus> as its session bus (default: this terminal's).
mount_on() { # [<bus address>]
    local bus=${1:-${DBUS_SESSION_BUS_ADDRESS:-}}
    mkdir -p "$mnt"
    DBUS_SESSION_BUS_ADDRESS=$bus record pd mount --backend "fake:$fake_dir" "$mnt" ||
        die "the mount did not start; see $daemon_log"
}
unmount() { if mounted; then record pd unmount --wait "$mnt"; fi; }
restart() { unmount; mount_on "$@"; }

# The config: the base written by setup, plus the given TOML lines.
set_config() { # <toml line...>
    { cat "$state/config.base"; printf '%s\n' "$@"; } >"$config"
}

# An edit_edit conflict of <name>: written here and uploaded, then written here again while
# another device (`pdmount fake write`) uploads its own revision; the upload after the close
# conflicts. The copy of this device's content is named "<name> (conflict …)".
make_conflict() { # <name>
    local file=$mnt$remote/$1 fd
    printf 'base\n' >"$file"
    wait_queue
    exec {fd}>"$file"
    printf 'written on this desktop\n' >&"$fd"
    printf 'written on another device\n' >"$state/there"
    pdmount fake write --path "$remote/$1" --from "$state/there" "$fake_dir" >/dev/null ||
        die "fake write failed"
    exec {fd}>&-
}
# <n> conflicts recorded since the mark, within a minute.
wait_conflicts() { # <n>
    wait_log 'conflict recorded' "$1" 60 || die "no $1 conflicts recorded within 60 s; see $daemon_log"
}
stamp() { date +%H%M%S; }

# A private session bus without a notification server, for `missed`: no activation files, so
# D-Bus cannot start one there either. Prints its address.
private_bus() {
    have dbus-daemon || die "dbus-daemon is not installed"
    stop_private_bus
    local socket=$state/bus.sock
    rm -f -- "$socket"
    cat >"$state/bus.conf" <<EOF
<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:path=$socket</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
EOF
    dbus-daemon --config-file="$state/bus.conf" --nofork --nopidfile </dev/null >/dev/null 2>&1 &
    echo $! >"$state/bus.pid"
    local i
    for ((i = 0; i < 50; i++)); do
        [[ -S $socket ]] && break
        sleep 0.1
    done
    [[ -S $socket ]] || die "the private bus did not start"
    echo "unix:path=$socket"
}
stop_private_bus() {
    [[ -f $state/bus.pid ]] || return 0
    kill "$(cat "$state/bus.pid")" 2>/dev/null
    rm -f -- "$state/bus.pid" "$state/bus.sock" "$state/bus.conf"
}
# The desktop's bus kept by the last `session attach`; a daemon's start reads it again.
forget_attach() { rm -f "${XDG_RUNTIME_DIR:-/nonexistent}/pdmount/$profile.attach"; }

setup() {
    section "setup"
    have pdmount || die "pdmount is not on PATH"
    record pdmount --version
    pdmount fake --help >/dev/null 2>&1 || die "pdmount is not a test build: build with --features fake-backend"
    echo "session bus: ${DBUS_SESSION_BUS_ADDRESS:-<unset>}" | out
    [[ -n ${DBUS_SESSION_BUS_ADDRESS:-} ]] || die "this terminal has no session bus: run it inside the desktop"
    mkdir -p "$config_home/pdmount"
    # The profile's own test mountpoint, not a path of this machine.
    printf 'mountpoint = "~/PdMount-%s"\n[device]\nname = "m6-%s"\n' "$profile" "${desktop%%:*}" >"$state/config.base" # betterleaks:allow
    if [[ -e $config ]] && ! cmp -s "$config" "$state/config.base"; then
        die "$config exists with other content; move it away first"
    fi
    set_config
    if [[ ! -f $fake_dir/.seeded-m6 ]]; then
        [[ ! -e $fake_dir ]] || [[ -f $fake_dir/snapshot.json ]] || die "$fake_dir exists and is not a fake drive"
        unmount
        rm -rf -- "$fake_dir"
        mkdir -p "$state/seed$remote"
        printf 'M6 checks\n' >"$state/seed$remote/readme.txt"
        head -c 64M /dev/urandom >"$state/seed$remote/$clip"
        record pdmount fake seed --from "$state/seed" "$fake_dir"
        rm -rf -- "$state/seed"
        touch "$fake_dir/.seeded-m6"
    fi
    forget_attach
    mounted || mount_on
    record ls "$mnt$remote"
    record pd doctor
    echo "Expected: the doctor's \`notifications\` line is ok (owned, or activatable)." | out
}

conflict() {
    section "conflict"
    need_mounted
    local name
    name=conflict-$(stamp).txt
    log_mark
    make_conflict "$name"
    wait_conflicts 1
    cat <<EOF | out
Expected now: a notification "PdMount: conflict" with the text
    $remote/$name
    Changed here and on another device; both copies kept.
and a button "Show".
EOF
    ask "Did it appear like that? (Note the button's name if it differs.)"
    echo "Now click Show (in the notification list if the popup has gone). Waiting ${M6_SHOW_WAIT:-120} s for it." | out
    if wait_log 'conflict: (the file manager shows|no file manager answered|the bus did not answer|cannot)' 1 "${M6_SHOW_WAIT:-120}"; then
        show_log 'conflict: '
        echo "Expected: the file manager opens $mnt$remote with \"${name%.txt} (conflict …).txt\" selected." | out
        ask "Did the file manager open the folder with the copy selected?"
    else
        echo "No click seen in the log." | out
        ask "What happened when you clicked Show (or was there no button)?"
    fi
}

missed() {
    section "missed: made while the daemon had no notification service, shown at its next start"
    local bus a b
    bus=$(private_bus)
    forget_attach
    restart "$bus"
    log_mark
    a=missed-a-$(stamp).txt
    b=missed-b-$(stamp).txt
    make_conflict "$a"
    make_conflict "$b"
    wait_conflicts 2
    wait_queue
    ask "Did any PdMount notification appear just now? (Expected: no; the daemon has no notification service.)"
    echo "Restarting the daemon on this desktop's bus, as at the next login." | out
    log_mark
    restart
    wait_log 'conflicts recorded while no notification service ran are announced' 1 60 ||
        echo "No announcement in the log within 60 s." | out
    show_log 'announced|conflict'
    echo "Expected: two notifications \"PdMount: conflict\", for $remote/$a and $remote/$b." | out
    ask "Did both appear (once each)?"

    section "missed: shown when the desktop's bus is handed over (session attach)"
    bus=$(private_bus)
    forget_attach
    restart "$bus"
    log_mark
    a=missed-c-$(stamp).txt
    make_conflict "$a"
    wait_conflicts 1
    wait_queue
    ask "Did any PdMount notification appear just now? (Expected: no.)"
    echo "Handing this desktop's bus to the daemon, as the login entry does." | out
    record pd session attach
    wait_log 'conflicts recorded while no notification service ran are announced' 1 60 ||
        echo "No announcement in the log within 60 s." | out
    show_log 'announced|attached|conflict'
    echo "Expected: one notification \"PdMount: conflict\" for $remote/$a, within a few seconds of the attach." | out
    ask "Did it appear?"
    echo "Back to a daemon on this desktop's bus." | out
    forget_attach
    restart
    stop_private_bus
}

# Reads the first 64 KiB of the large file, uncached, over the slow profile; prints the time.
slow_read() {
    record pd cache wipe "$mnt$remote/$clip" >/dev/null
    record pdmount fake profile --rtt 80 --down 500000 "$fake_dir"
    local start=$SECONDS
    echo "Reading the first 64 KiB of $clip (about 8 s on this profile) ..." | out
    dd if="$mnt$remote/$clip" of=/dev/null bs=64k count=1 iflag=direct status=none
    echo "read in $((SECONDS - start)) s" | out
    record pdmount fake profile clear "$fake_dir"
}

slow() {
    section "slow download"
    need_mounted
    log_mark
    slow_read
    cat <<EOF | out
Expected: about 5 s after the start, a notification "PdMount: downloading" with the text
    $clip (64 MiB) is downloading; it opens as its blocks arrive.
which goes away by itself once the read is done (a few seconds later).
EOF
    ask "Did it appear like that, and did it go away by itself?"
    echo "Optional: open $mnt$remote/$clip in a video player after \`pdmount --profile $profile cache wipe $mnt$remote/$clip\` and \`pdmount fake profile --rtt 80 --down 500000 $fake_dir\`; clear the profile afterwards (\`pdmount fake profile clear $fake_dir\`)." | out
}

settings() {
    need_mounted
    section "settings: notify.conflicts = false"
    set_config '[notify]' 'conflicts = false'
    restart
    log_mark
    make_conflict "off-$(stamp).txt"
    wait_conflicts 1
    sleep 3
    ask "Did a conflict notification appear? (Expected: no.)"

    section "settings: notify.conflict_names = false"
    set_config '[notify]' 'conflict_names = false'
    restart
    log_mark
    make_conflict "unnamed-$(stamp).txt"
    wait_conflicts 1
    echo "Expected: \"PdMount: conflict\" with only \"Changed here and on another device; both copies kept.\" (no file name), and Show." | out
    ask "Did it appear without the file's name?"

    section "settings: notify.downloads = false"
    set_config '[notify]' 'downloads = false'
    restart
    slow_read
    ask "Did a \"PdMount: downloading\" notification appear? (Expected: no.)"

    set_config
    restart
}

cleanup() {
    confirm "Unmount profile $profile, remove $config and $fake_dir?" || exit 0
    section "cleanup"
    pdmount fake profile clear "$fake_dir" >/dev/null 2>&1
    unmount
    stop_private_bus
    forget_attach
    rm -f -- "$config" "$state/config.base" "$state/there"
    if [[ -f $fake_dir/snapshot.json || -e $fake_dir/.seeded-m6 ]]; then
        rm -rf -- "$fake_dir"
        echo "removed $fake_dir" | out
    elif [[ -d $fake_dir ]]; then
        echo "$fake_dir does not look like a fake drive; left in place" | out
    fi
    rmdir -- "$mnt" 2>/dev/null
    echo "Kept: $results. The profile's own state: \`pdmount --profile $profile reset --keep-login\`." | out
}

mkdir -p "$state"
case ${1:-} in
setup) setup ;;
conflict) conflict ;;
missed) missed ;;
slow) slow ;;
settings) settings ;;
report) cat "$results" ;;
cleanup) cleanup ;;
*) sed -n '2,/^set -uo/{/^set -uo/d;s/^# \{0,1\}//;p}' "$0"; exit 2 ;;
esac
