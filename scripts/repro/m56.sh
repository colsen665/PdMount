#!/usr/bin/env bash
# M5.6: the manual service runs (docs/plans/m5.md, "Running M5.6"). Part A on a fake drive,
# part B on the real account (writes only under pdmount-test/m56/, D028).
#
# Run as the desktop user, in a terminal inside the graphical session, with a test build
# of `pdmount` (`--features fake-backend`) on PATH. Part A touches nothing on Proton.
#
# Usage:
#   m56.sh setup          config, fake drive with the slow network profile, service install,
#                         then `doctor` and `service` (saved to the results)
#   m56.sh check-setup    `doctor` and `service` again (Gentoo: after the reboot for elogind)
#   m56.sh run A<n>       do step A1..A10: write the queue, mark the time, and either check at
#                         once or tell you what to do (log out, reboot) and what to run after
#   m56.sh check A<n>     after a logout or reboot: log lines since the step's mark, status,
#                         service state, the fake drive's changes; appended to the results
#   m56.sh queue <size>   one random file of <size> (head -c syntax: 30M) in the mount
#   m56.sh keyring        why the Secret Service does not answer (for part B): the session
#                         buses, who owns org.freedesktop.secrets, and for each keyring
#                         process whether it is on this terminal's bus
#   m56.sh report         the results file, to paste back
#   m56.sh cleanup        uninstall the service, remove the config and the fake drive
#
# Part B, profile m56real on ~/ProtonDrive-m56, after `pdmount --profile m56real login`:
#   m56.sh setup-b        config (if missing), service install without --backend, doctor
#   m56.sh run B1         10M into pdmount-test/m56/, then reboot; `check B1` after the login
#   m56.sh run B2         lock the keyring, restart the service; cancel the unlock dialog
#   m56.sh check B<n>     log lines, status (with a timeout), service, pdmount-test/m56/
#   m56.sh cleanup-b      remove pdmount-test/m56/, uninstall the service, optionally log out
#
# State and results: $XDG_STATE_HOME/pdmount-m56/ (results.txt, marks).
# Environment: PROFILE (default m56), FAKE_DIR (default $XDG_DATA_HOME/pdmount-m56-fake),
# REAL_PROFILE (default m56real).
set -uo pipefail

: "${HOME:=$(getent passwd "$(id -u)" | cut -d: -f6)}"
profile=${PROFILE:-m56}
data_home=${XDG_DATA_HOME:-$HOME/.local/share}
config_home=${XDG_CONFIG_HOME:-$HOME/.config}
state_home=${XDG_STATE_HOME:-$HOME/.local/state}
fake_dir=${FAKE_DIR:-$data_home/pdmount-$profile-fake}
mnt=$HOME/PdMount-$profile # betterleaks:allow
config=$config_home/pdmount/$profile.toml
state=$state_home/pdmount-m56
results=$state/results.txt
marks=$state/marks
daemon_log=$state_home/pdmount/$profile.log
# Where `queue` writes: the mount's root on the fake drive, pdmount-test/m56/ on the real one.
qdir=$mnt
real=0

# Part B: the real account's profile.
use_real() {
    real=1
    profile=${REAL_PROFILE:-m56real}
    mnt=$HOME/ProtonDrive-m56
    qdir=$mnt/pdmount-test/m56
    config=$config_home/pdmount/$profile.toml
    daemon_log=$state_home/pdmount/$profile.log
}

die() { echo "m56.sh: $*" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }
pd() { pdmount --profile "$profile" "$@"; }

if [[ -d /run/systemd/system ]]; then manager=systemd; else manager=openrc; fi
os=$(. /etc/os-release 2>/dev/null; echo "${ID:-unknown}")

# A bus address without its ",guid=…" part, for comparing two addresses of one bus.
bus_key() { sed 's/,guid=[0-9a-f]*//' <<<"$1"; }

# Prints and appends to the results.
out() { tee -a "$results"; }
section() { printf '\n===== %s  %s\n' "$*" "$(date '+%F %T %Z')" | out; }
# Runs a command, prints it with its output and exit code into the results.
record() {
    printf '$ %s\n' "$*" | out
    "$@" 2>&1 | out
    local rc=${PIPESTATUS[0]}
    printf '(exit %s)\n' "$rc" | out
    return "$rc"
}

service_status() {
    if [[ $manager == systemd ]]; then
        systemctl --user --no-pager status "pdmount@$profile" 2>&1 | head -n 5
    else
        rc-service --user "pdmount.$profile" status 2>&1
    fi
}

service_cmd() { # start|stop|restart -> the command's words
    if [[ $manager == systemd ]]; then
        echo systemctl --user "$1" "pdmount@$profile"
    else
        echo rc-service --user "pdmount.$profile" "$1"
    fi
}

# The last change number on the fake drive (0 when none, and on the real account).
fake_seq() {
    ((real)) && { echo 0; return; }
    local last
    last=$(pdmount fake changes "$fake_dir" 2>/dev/null | tail -n 1)
    echo "${last%% *}" | grep -E '^[0-9]+$' || echo 0
}

mark() { # step
    printf '%s %s %s\n' "$1" "$(date +%s)" "$(fake_seq)" >>"$marks"
}

mark_of() { # step -> "epoch seq"
    local line
    line=$(grep "^$1 " "$marks" 2>/dev/null | tail -n 1) || true
    [[ -n $line ]] || die "no mark for $1: run \`m56.sh run $1\` first"
    echo "${line#* }"
}

# Daemon log lines since an epoch, at most the last 150.
log_since() { # epoch
    if [[ $manager == systemd ]]; then
        journalctl --user -u "pdmount@$profile" --no-pager -o short-iso --since "@$1" 2>&1 | tail -n 150
    else
        local since
        since=$(date -u -d "@$1" +%FT%T)
        cat "$daemon_log.1" "$daemon_log" 2>/dev/null | sed 's/\x1b\[[0-9;]*m//g' |
            awk -v since="$since" '$1 ~ /^[0-9]{4}-[0-9]{2}-[0-9]{2}T/ && substr($1, 1, 19) >= since { on = 1 } on' |
            tail -n 150
    fi
}

queue() { # size
    mountpoint -q "$mnt" || die "$mnt is not mounted"
    local file
    mkdir -p "$qdir" || die "cannot create $qdir"
    file=$qdir/q$1-$(date +%s).bin
    head -c "$1" /dev/urandom >"$file" || die "cannot write $file"
    echo "queued $file ($1)" | out
}

pending() {
    timeout 15 pdmount --profile "$profile" status 2>/dev/null | sed -n 's/^Queue: *\([0-9]*\) pending.*/\1/p'
}

check() { # step
    local step=$1 epoch seq
    read -r epoch seq <<<"$(mark_of "$step")"
    section "check $step (mark $(date -d "@$epoch" '+%F %T'))"
    echo "--- daemon log since the mark" | out
    log_since "$epoch" | out
    echo "--- status (timeout 15 s: exit 124 means it hung)" | out
    record timeout 15 pdmount --profile "$profile" status
    echo "--- service" | out
    service_status | out
    if ((real)); then
        echo "--- $qdir" | out
        if mountpoint -q "$mnt"; then ls -l "$qdir" 2>&1 | out; else echo "$mnt is not mounted" | out; fi
    else
        echo "--- fake drive changes since the mark" | out
        pdmount fake changes --after "$seq" "$fake_dir" 2>&1 | out
        echo "--- mount" | out
        if mountpoint -q "$mnt"; then ls "$mnt" | out; else echo "$mnt is not mounted" | out; fi
    fi
}

confirm() { # prompt
    local answer
    read -r -p "$1 [y/N] " answer
    [[ $answer == [yY]* ]]
}

after_session_end() { # step what
    cat <<EOF

Now: $2
After logging in again, in a terminal:
    $0 check $1
EOF
}

reboot_now() { # step command...
    local step=$1; shift
    echo
    echo "Next: \`$*\`. After logging in again: $0 check $step"
    confirm "Reboot now?" || { echo "Not rebooted. Run \`$*\` yourself, then \`$0 check $step\`."; return; }
    sync
    "$@"
}

run() { # step
    local step=${1:-}
    case $step in
    A1)
        section "run A1: readiness"
        mark A1
        echo "--- service" | out
        service_status | out
        record ls "$mnt"
        ;;
    A2)
        section "run A2: crash restart"
        mark A2
        record pkill -KILL -f "pdmount --profile $profile mount"
        echo "waiting 15 s" | out
        sleep 15
        check A2
        echo "Expected: restarted within about 10 s, the log shows the stale mount removed, the listing works."
        ;;
    A3)
        section "run A3: unmount under the service"
        mark A3
        record pd unmount
        if mountpoint -q "$mnt"; then echo "mount still there" | out; else echo "MOUNT GONE" | out; fi
        echo "Expected: refused, exit 1, names the stop command; the mount stays."
        ;;
    A4)
        section "run A4: unmount --force"
        mark A4
        record pd unmount --force
        echo "waiting 15 s" | out
        sleep 15
        check A4
        if [[ $manager == systemd ]]; then
            echo "Expected (systemd): the unit stays stopped. Start it again: systemctl --user start pdmount@$profile"
        else
            echo "Expected (OpenRC): the daemon was restarted."
        fi
        ;;
    A5)
        section "run A5: logout with an empty queue"
        record pd status
        [[ $(pending) == 0 ]] || echo "WARNING: the queue is not empty; wait for it or run \`pdmount --profile $profile sync\`" | out
        mark A5
        after_session_end A5 "log out at once, then log in again."
        ;;
    A6)
        section "run A6: logout with a queue"
        queue 30M
        mark A6
        after_session_end A6 "log out at once, wait 2 minutes at the greeter, log in again."
        ;;
    A7)
        section "run A7: reboot with a queue"
        queue 30M
        mark A7
        if [[ $manager == systemd ]]; then reboot_now A7 systemctl reboot; else reboot_now A7 loginctl reboot; fi
        ;;
    A8)
        section "run A8: reboot with more than the grace"
        queue 100M
        mark A8
        if [[ $manager == systemd ]]; then reboot_now A8 systemctl reboot; else reboot_now A8 loginctl reboot; fi
        ;;
    A9)
        section "run A9: offline at logout"
        # The fault must end before the next login: the fake backend cannot start a
        # daemon while offline (unlike Proton, which starts from the stored root).
        record pdmount fake fault offline --for 60 "$fake_dir"
        queue 5M
        mark A9
        after_session_end A9 "log out at once, wait at least 70 s at the greeter (the offline fault ends after 60 s), log in again. \`check A9\` then waits for the upload."
        ;;
    A10)
        section "run A10: root's poweroff with a queue"
        queue 30M
        mark A10
        echo "Next: \`su -c poweroff\` (root's password). After the next login: $0 check A10"
        confirm "Power off now?" && { sync; su -c poweroff; }
        ;;
    B1)
        section "run B1: reboot with a queue on the real account"
        record timeout 15 pdmount --profile "$profile" status
        queue 10M
        mark B1
        echo "After the reboot, check in the web client that the file above is in pdmount-test/m56/." | out
        if [[ $manager == systemd ]]; then reboot_now B1 systemctl reboot; else reboot_now B1 loginctl reboot; fi
        ;;
    B2)
        section "run B2: service start with a locked keyring"
        record timeout 15 pdmount --profile "$profile" status
        lock_keyring || return 1
        mark B2
        echo "Restarting the service. If an unlock dialog opens, CANCEL it: the test is a start without the keyring. Note whether a sign-in notification appears." | out
        local started=$SECONDS
        # shellcheck disable=SC2046 # the command's words
        record timeout 200 $(service_cmd restart)
        echo "restart returned after $((SECONDS - started)) s; waiting 10 s" | out
        sleep 10
        check B2
        echo
        echo "Expected: status answers (no exit 124) and says the session needs a login, or the start"
        echo "failed with a keyring error; a sign-in notification; the service does not hang."
        echo "Then unlock the keyring (Seahorse, or the dialog of \`pdmount --profile $profile login\` if"
        echo "status asks for a login), run \`$(service_cmd restart)\` if the service is not started,"
        echo "and \`$0 check B2\` again: online."
        ;;
    *) die "run A1..A10 or B1, B2" ;;
    esac
}

# Locks the default Secret Service collection (reads no secret).
lock_keyring() {
    local path
    path=$(timeout 5 dbus-send --session --print-reply --dest=org.freedesktop.secrets /org/freedesktop/secrets \
        org.freedesktop.Secret.Service.ReadAlias string:default 2>/dev/null |
        sed -n 's/.*object path "\([^"]*\)".*/\1/p')
    if [[ -z $path || $path == / ]]; then
        echo "no default keyring collection on the session bus: run \`$0 keyring\`" | out
        return 1
    fi
    record timeout 5 dbus-send --session --print-reply --dest=org.freedesktop.secrets /org/freedesktop/secrets \
        org.freedesktop.Secret.Service.Lock "array:objpath:$path"
}

check_step() { # step
    case ${1:-} in
    A[1-9] | A10 | B1 | B2) ;;
    *) die "check A1..A10 or B1, B2" ;;
    esac
    check "$1"
    [[ $1 == B1 ]] && echo "Check in the web client that the file of run B1 is in pdmount-test/m56/."
    if [[ $1 == A9 ]]; then
        section "A9: clearing the fault, waiting up to 5 min for the queue"
        record pdmount fake fault clear "$fake_dir"
        local i
        for ((i = 0; i < 60; i++)); do
            [[ $(pending) == 0 ]] && break
            sleep 5
        done
        check A9
    fi
}

setup() {
    mkdir -p "$state" "$config_home/pdmount"
    section "setup on $os ($manager)"
    have pdmount || die "pdmount is not on PATH"
    record pdmount --version
    pdmount fake --help >/dev/null 2>&1 || die "pdmount is not a test build: build with --features fake-backend"
    local want
    # The profile's own test mountpoint, not a path of this machine.
    want=$(printf 'mountpoint = "~/PdMount-%s"\n[device]\nname = "m56-%s"\n[timeouts]\nshutdown_grace = "90s"\n' "$profile" "$os") # betterleaks:allow
    if [[ -e $config ]]; then
        [[ $(cat "$config") == "$want" ]] || die "$config exists with other content; move it away first"
    else
        printf '%s\n' "$want" >"$config"
        echo "wrote $config" | out
    fi
    mkdir -p "$fake_dir"
    record pdmount fake profile --rtt 80 --up 500000 "$fake_dir"
    record pd service install --backend "fake:$fake_dir"
    if [[ $manager == systemd ]]; then
        record systemd-analyze --user verify "$config_home/systemd/user/pdmount@.service"
    else
        echo "Gentoo: if \`install\` printed a file for root, create it as root and reboot once, then run \`$0 check-setup\`." | out
    fi
    check_setup
}

setup_b() {
    mkdir -p "$state" "$config_home/pdmount"
    section "setup B ($profile) on $os ($manager)"
    have pdmount || die "pdmount is not on PATH"
    record pdmount --version
    if [[ -e $config ]]; then
        grep -Eq '^mountpoint *= *"~/ProtonDrive-m56"' "$config" ||
            die "$config exists without mountpoint = \"~/ProtonDrive-m56\"; add it or move the file away"
        echo "using $config" | out
    else
        printf 'mountpoint = "~/ProtonDrive-m56"\n[device]\nname = "%s-%s"\n[timeouts]\nshutdown_grace = "90s"\n' \
            "$profile" "$os" >"$config"
        echo "wrote $config" | out
    fi
    record timeout 15 pdmount --profile "$profile" status
    record pd service install
    if [[ $manager == systemd ]]; then
        record systemd-analyze --user verify "$config_home/systemd/user/pdmount@.service"
    fi
    local i
    for ((i = 0; i < 30; i++)); do
        mountpoint -q "$mnt" && break
        sleep 2
    done
    echo "--- service" | out
    service_status | out
    if mountpoint -q "$mnt"; then
        mkdir -p "$qdir" && echo "ready: $qdir" | out
    else
        echo "$mnt is not mounted after 60 s; see the log" | out
    fi
    check_setup
}

cleanup_b() {
    confirm "Remove $qdir on the drive, uninstall the service of $profile, remove $config?" || exit 0
    section "cleanup B"
    if mountpoint -q "$mnt" && [[ -d $qdir ]]; then
        rm -rf -- "$qdir"
        echo "removed $qdir; waiting up to 2 min for the queue" | out
        local i
        for ((i = 0; i < 24; i++)); do
            [[ $(pending) == 0 ]] && break
            sleep 5
        done
        record timeout 15 pdmount --profile "$profile" status
    fi
    record pd service uninstall
    rm -f -- "$config"
    rmdir -- "$mnt" 2>/dev/null
    if confirm "Log out profile $profile (deletes its session from the keyring)?"; then
        record pd logout
    fi
}

check_setup() {
    section "doctor and service"
    record pd doctor
    record pd service
}

keyring() {
    mkdir -p "$state"
    section "keyring diagnosis"
    echo "session bus: ${DBUS_SESSION_BUS_ADDRESS:-<unset>}" | out
    echo "--- session buses" | out
    pgrep -a -u "$(id -u)" dbus-daemon 2>&1 | out
    pgrep -a -u "$(id -u)" dbus-broker 2>&1 | out
    echo "--- owner of org.freedesktop.secrets" | out
    timeout 5 dbus-send --session --print-reply --dest=org.freedesktop.DBus /org/freedesktop/DBus \
        org.freedesktop.DBus.GetConnectionUnixProcessID string:org.freedesktop.secrets 2>&1 | out
    echo "--- D-Bus activation files for it" | out
    local f
    for f in /usr/share/dbus-1/services/*.service "$data_home"/dbus-1/services/*.service; do
        [[ -f $f ]] && grep -q org.freedesktop.secrets "$f" && grep -H '^Exec' "$f" | out
    done
    echo "--- providers" | out
    local pid
    for pid in $(pgrep -u "$(id -u)" -f 'gnome-keyring|kwalletd|keepassxc|secret-service'); do
        echo "== $(ps -o pid=,ppid=,lstart=,args= -p "$pid")" | out
        local env bus
        env=$(tr '\0' '\n' <"/proc/$pid/environ" 2>/dev/null)
        printf '%s\n' "$env" | grep -E '^(DBUS_SESSION_BUS_ADDRESS|XDG_RUNTIME_DIR)=' | sed 's/^/   /' | out
        bus=$(printf '%s\n' "$env" | sed -n 's/^DBUS_SESSION_BUS_ADDRESS=//p')
        if [[ -z $bus ]]; then
            echo "   -> no bus in its environment (normal for the PAM-started daemon: \`--start\` hands it the address)" | out
        elif [[ $(bus_key "$bus") == "$(bus_key "${DBUS_SESSION_BUS_ADDRESS:-}")" ]]; then
            echo "   -> same bus as this terminal" | out
        else
            echo "   -> a DIFFERENT bus from this terminal" | out
        fi
    done
    echo "--- ${XDG_RUNTIME_DIR:-<XDG_RUNTIME_DIR unset>}/keyring" | out
    ls -l "${XDG_RUNTIME_DIR:-/nonexistent}/keyring" 2>&1 | out
    # The service's bus can differ from the desktop's (OpenRC: $XDG_RUNTIME_DIR/bus).
    echo "--- names on each bus (secrets: the keyring; Notifications: the desktop's popups)" | out
    local daemon_pid daemon_bus="" seen=" " label addr name
    daemon_pid=$(pgrep -u "$(id -u)" -f "pdmount --profile $profile mount" | head -n 1)
    [[ -n $daemon_pid ]] && daemon_bus=$(tr '\0' '\n' <"/proc/$daemon_pid/environ" 2>/dev/null |
        sed -n 's/^DBUS_SESSION_BUS_ADDRESS=//p')
    for label in "terminal=${DBUS_SESSION_BUS_ADDRESS:-}" \
        "runtime-dir=${XDG_RUNTIME_DIR:+unix:path=$XDG_RUNTIME_DIR/bus}" \
        "daemon of $profile (pid ${daemon_pid:-none})=$daemon_bus"; do
        addr=${label#*=}
        label=${label%%=*}
        if [[ -z $addr ]]; then
            echo "$label: no bus" | out
            continue
        fi
        echo "$label: $addr" | out
        [[ $seen == *" $(bus_key "$addr") "* ]] && { echo "   (same as above)" | out; continue; }
        seen+="$(bus_key "$addr") "
        for name in org.freedesktop.secrets org.freedesktop.Notifications; do
            local owned
            owned=$(timeout 5 dbus-send --bus="$addr" --print-reply --dest=org.freedesktop.DBus \
                /org/freedesktop/DBus org.freedesktop.DBus.NameHasOwner string:"$name" 2>&1 |
                sed -n 's/^ *boolean //p;s/^Error /error: /p')
            if [[ $owned == false ]] && timeout 5 dbus-send --bus="$addr" --print-reply \
                --dest=org.freedesktop.DBus /org/freedesktop/DBus \
                org.freedesktop.DBus.ListActivatableNames 2>/dev/null | grep -q "\"$name\""; then
                owned="false (not running; D-Bus starts it on first use)"
            fi
            printf '   %-32s %s\n' "$name" "${owned:-cannot connect}" | out
        done
    done
    echo "--- starting it by name (waits up to 25 s)" | out
    timeout 30 dbus-send --session --print-reply --dest=org.freedesktop.DBus /org/freedesktop/DBus \
        org.freedesktop.DBus.StartServiceByName string:org.freedesktop.secrets uint32:0 2>&1 | out
}

cleanup() {
    confirm "Uninstall the service of profile $profile, remove $config and $fake_dir?" || exit 0
    section "cleanup"
    record pd service uninstall
    rm -f -- "$config"
    if [[ -f $fake_dir/snapshot.json || -e $fake_dir/.seeded ]] || compgen -G "$fake_dir/.seeded*" >/dev/null; then
        rm -rf -- "$fake_dir"
        echo "removed $fake_dir" | out
    elif [[ -d $fake_dir ]]; then
        echo "$fake_dir does not look like a fake drive; left in place" | out
    fi
    rmdir -- "$mnt" 2>/dev/null
    echo "Kept: $results. Gentoo: remove /etc/elogind/logind.conf.d/pdmount.conf as root if not wanted." | out
}

mkdir -p "$state"
[[ ${2:-} == B* || ${1:-} == *-b ]] && use_real
case ${1:-} in
setup) setup ;;
check-setup) check_setup ;;
setup-b) setup_b ;;
cleanup-b) cleanup_b ;;
run) run "${2:-}" ;;
check) check_step "${2:-}" ;;
queue) [[ -n ${2:-} ]] || die "queue <size>"; queue "$2" ;;
keyring) keyring ;;
report) cat "$results" ;;
cleanup) cleanup ;;
*) sed -n '2,/^set -uo/{/^set -uo/d;s/^# \{0,1\}//;p}' "$0"; exit 2 ;;
esac
