#!/usr/bin/env bash
# M5.0 probe: what a user service sees when the session ends (docs/plans/m5.md, M5.0).
#
# Installed as an OpenRC user service (Gentoo) or a systemd user unit (Debian), it logs
# what PdMount's daemon would see at logout and shutdown: when SIGTERM, SIGHUP or SIGINT
# arrives, whether the network and the session bus still work, and how long the system
# lets it run after that. It sends no data: the network check is a DNS lookup and a TCP
# connect to PROBE_HOST:PROBE_PORT, closed at once.
#
# Before a signal it writes a liveness stamp every 10 s and a log line every 10 min.
# From the first signal, or from logind's PrepareForShutdown, it checks once a second
# until it is killed or PROBE_LINGER seconds have passed (default 120: the planned 90 s
# grace plus 30 s). It also logs logind's session, user and shutdown signals.
#
# Usage:
#   session-end-probe.sh install openrc|systemd [--inhibit] [--no-enable]
#   session-end-probe.sh uninstall
#   session-end-probe.sh summary     one block per run; times after the first signal are relative
#   session-end-probe.sh log         the full log
#   session-end-probe.sh run [--inhibit]   the service's entry point
#
# `install` copies this script to $XDG_DATA_HOME/pdmount-probe/, writes the unit, enables
# and starts it (not with --no-enable). The log is $XDG_STATE_HOME/pdmount-probe/probe.log.
# `uninstall` stops the service without the linger, removes the unit and keeps the log.
#
# Question 5 (respawn): SIGUSR1 makes `run` exit 0, SIGUSR2 exit 1; a new `start` line
# with another pid means the manager restarted it.
# Question 6 (inhibitors): --inhibit holds a logind delay lock for shutdown while running.
#
# Environment: PROBE_HOST (default drive-api.proton.me), PROBE_PORT (443), PROBE_LINGER (120).
set -uo pipefail

: "${HOME:=$(getent passwd "$(id -u)" | cut -d: -f6)}"
name=pdmount-probe
state_dir=${XDG_STATE_HOME:-$HOME/.local/state}/$name
share_dir=${XDG_DATA_HOME:-$HOME/.local/share}/$name
config_dir=${XDG_CONFIG_HOME:-$HOME/.config}
logf=$state_dir/probe.log
alive=$state_dir/alive
quiet=$state_dir/quiet
host=${PROBE_HOST:-drive-api.proton.me}
port=${PROBE_PORT:-443}
linger=${PROBE_LINGER:-120}
boot=$(head -c 8 /proc/sys/kernel/random/boot_id 2>/dev/null || echo unknown)

have() { command -v "$1" >/dev/null 2>&1; }

log() {
    local now up
    now=$(date +%FT%T.%3N)
    read -r up _ </proc/uptime
    printf '%s up=%s pid=%s boot=%s %s\n' "$now" "$up" "$$" "$boot" "$*" >>"$logf"
    sync "$logf" 2>/dev/null
}

# Centiseconds since boot (/proc/uptime has two decimals).
cs_now() {
    local up
    read -r up _ </proc/uptime
    echo $((${up%.*} * 100 + 10#${up#*.}))
}

init_system() {
    if [[ -d /run/systemd/system ]]; then
        echo systemd
    elif [[ -d /run/openrc ]]; then
        echo openrc
    else
        echo unknown
    fi
}

# A uint64 property of logind's manager, in seconds, or "-".
logind_secs() {
    local out
    have gdbus || { echo -; return; }
    out=$(timeout 2 gdbus call --system --dest org.freedesktop.login1 \
        --object-path /org/freedesktop/login1 --method org.freedesktop.DBus.Properties.Get \
        org.freedesktop.login1.Manager "$1" 2>/dev/null) || { echo -; return; }
    out=${out//[^0-9]/}
    out=${out#64} # the "uint64" type tag
    [[ -n $out ]] && echo "$((out / 1000000))s" || echo -
}

# Active NetworkManager connections as type:owner, where owner is "user" when the
# connection is limited to users (connection.permissions), else "system". No names.
nm_conns() {
    local uuid type perms out=
    have nmcli || { echo -; return; }
    while IFS=: read -r uuid type; do
        [[ -z $uuid || $type == loopback ]] && continue
        perms=$(timeout 2 nmcli -g connection.permissions connection show "$uuid" 2>/dev/null)
        out+="${out:+,}$type:$([[ -n $perms ]] && echo user || echo system)"
    done < <(timeout 2 nmcli -t -f UUID,TYPE connection show --active 2>/dev/null)
    echo "${out:--}"
}

tick() {
    local dns tcp route='' nm=- bus notif=- sys=- usr=- t0ms out
    if timeout 1 getent ahosts "$host" >/dev/null 2>&1; then dns=ok; else dns=fail; fi
    t0ms=$(date +%s%3N)
    # shellcheck disable=SC2016 # expanded by the inner bash
    if timeout 2 bash -c 'exec 3<>"/dev/tcp/$1/$2"' _ "$host" "$port" 2>/dev/null; then
        # A signal to the whole cgroup (systemd) can kill the `date` above.
        [[ -n $t0ms ]] && tcp="ok/$(($(date +%s%3N) - t0ms))ms" || tcp=ok
    else
        tcp=fail
    fi
    ip -o -4 route show default 2>/dev/null | grep -q . && route+=v4
    ip -o -6 route show default 2>/dev/null | grep -q . && route+=${route:+,}v6
    have nmcli && nm=$(timeout 1 nmcli -t -f STATE general 2>/dev/null || echo fail)
    if [[ -z ${DBUS_SESSION_BUS_ADDRESS:-} ]] || ! have gdbus; then
        bus=-
    elif out=$(timeout 1 gdbus call --session --dest org.freedesktop.DBus \
        --object-path /org/freedesktop/DBus --method org.freedesktop.DBus.NameHasOwner \
        org.freedesktop.Notifications 2>/dev/null); then
        bus=ok
        [[ $out == *true* ]] && notif=yes || notif=no
    else
        bus=fail
    fi
    case $(init_system) in
    systemd)
        sys=$(timeout 1 systemctl is-system-running 2>/dev/null)
        usr=$(timeout 1 systemctl --user is-system-running 2>/dev/null)
        ;;
    openrc)
        [[ -r /run/openrc/softlevel ]] && sys=$(</run/openrc/softlevel)
        [[ -r ${XDG_RUNTIME_DIR:-/nonexistent}/openrc/softlevel ]] && usr=$(<"$XDG_RUNTIME_DIR/openrc/softlevel")
        ;;
    esac
    log "tick$(rel) dns=$dns tcp=$tcp route=${route:-none} nm=${nm:-?} bus=$bus notif=$notif sys=${sys:-?} usr=${usr:-?}"
}

t0=
rel() {
    [[ -n $t0 ]] || return 0
    local d=$(($(cs_now) - t0))
    printf ' t=+%d.%02d' $((d / 100)) $((d % 100))
}

trigger() {
    [[ -n $t0 ]] && return 0
    if [[ -e $quiet ]]; then
        rm -f "$quiet"
        finish 0 uninstall
    fi
    t0=$(cs_now)
    log "trigger reason=$1 conns=$(nm_conns)"
}

finish() {
    log "end$(rel) reason=$2 code=$1"
    rm -f "$alive"
    # shellcheck disable=SC2046 # word splitting of the job pids is intended
    kill $(jobs -p) 2>/dev/null
    exit "$1"
}

# logind's manager signals (sessions, users, PrepareForShutdown) on the system bus.
# PrepareForShutdown(true) wakes the main loop with SIGALRM.
start_monitor() {
    local pds=()
    have gdbus || { log "monitor unavailable (no gdbus)"; return; }
    setpriv --help 2>&1 | grep -q pdeathsig && pds=(setpriv --pdeathsig KILL)
    "${pds[@]}" gdbus monitor --system --dest org.freedesktop.login1 \
        --object-path /org/freedesktop/login1 2>/dev/null |
        while IFS= read -r line; do
            case $line in
            *.Manager.PrepareForShutdown*'(true'*)
                log "login1 ${line##*.Manager.}"
                kill -ALRM $$
                ;;
            *.Manager.*) log "login1 ${line##*.Manager.}" ;;
            esac
        done &
}

# A delay lock for shutdown, released when this process ends (tail --pid).
start_inhibit() {
    local tool
    tool=$(command -v systemd-inhibit || command -v elogind-inhibit) || {
        log "inhibit unavailable (no systemd-inhibit or elogind-inhibit)"
        return
    }
    "$tool" --what=shutdown --mode=delay --who="$name" --why="PdMount M5.0 session-end probe" \
        tail --pid=$$ -f /dev/null 2>>"$state_dir/stderr.log" &
    local pid=$!
    sleep 1
    if kill -0 "$pid" 2>/dev/null; then
        log "inhibit delay lock held via ${tool##*/}"
    else
        log "inhibit failed (see stderr.log)"
    fi
}

run() {
    local inhibit=no busenv=set n=0 p_pid p_boot p_when
    [[ ${1:-} == --inhibit ]] && inhibit=yes
    mkdir -p "$state_dir"
    exec 2>>"$state_dir/stderr.log"
    rm -f "$quiet"
    if [[ -z ${DBUS_SESSION_BUS_ADDRESS:-} ]]; then
        if [[ -S ${XDG_RUNTIME_DIR:-/nonexistent}/bus ]]; then
            export DBUS_SESSION_BUS_ADDRESS="unix:path=$XDG_RUNTIME_DIR/bus"
            busenv=derived
        else
            busenv=none
        fi
    fi
    if [[ -r $alive ]]; then
        read -r p_pid p_boot p_when <"$alive"
        log "previous pid=$p_pid boot=$p_boot last-alive=$p_when (no end line: killed without a logged end)"
    fi
    trap 'log "signal$(rel) TERM"; trigger signal-TERM' TERM
    trap 'log "signal$(rel) HUP"; trigger signal-HUP' HUP
    trap 'log "signal$(rel) INT"; trigger signal-INT' INT
    trap 'trigger prepare-for-shutdown' ALRM
    trap 'finish 0 usr1' USR1
    trap 'finish 1 usr2' USR2
    log "start init=$(init_system) parent=$(cat "/proc/$PPID/comm" 2>/dev/null) host=$host:$port" \
        "linger=${linger}s inhibit=$inhibit busenv=$busenv xdg_runtime=$([[ -n ${XDG_RUNTIME_DIR:-} ]] && echo set || echo unset)" \
        "inhibit_delay_max=$(logind_secs InhibitDelayMaxUSec)" \
        "user_stop_delay=$(logind_secs UserStopDelayUSec) conns=$(nm_conns)"
    start_monitor
    [[ $inhibit == yes ]] && start_inhibit
    tick
    while :; do
        if [[ -n $t0 ]]; then
            tick
            printf '%s %s %s\n' "$$" "$boot" "$(date +%FT%T)" >"$alive"
            (($(cs_now) - t0 >= linger * 100)) && finish 0 linger-reached
            sleep 1 &
        else
            printf '%s %s %s\n' "$$" "$boot" "$(date +%FT%T)" >"$alive"
            ((++n % 60 == 0)) && log alive
            sleep 10 &
        fi
        wait $!
        kill $! 2>/dev/null
    done
}

# One block per run (pid and boot): the start, signals, logind events, ticks whose state
# changed (the TCP time ignored), the last line and the end. Times are wall-clock before
# the first trigger and relative to it after.
summary() {
    [[ -r $logf ]] || { echo "no log at $logf" >&2; return 1; }
    awk '
    function out(k, s) { if (!(k in buf)) order[++nruns] = k; buf[k] = buf[k] s "\n" }
    {
        k = $3 " " $4; up = substr($2, 4) + 0; ev = $5
        when = (k in trig) ? sprintf("+%.2fs", up - trig[k]) : substr($1, 12, 8)
        if (!(k in buf)) out(k, "run " k " started " $1)
        rest = $0; sub(/^([^ ]+ ){4}/, "", rest)
        if (ev == "trigger" && !(k in trig)) trig[k] = up
        if (match(rest, / t=\+[0-9.]+/)) when = "+" substr(rest, RSTART + 4, RLENGTH - 4) "s"
        else if (ev == "trigger") when = "+0.00s"
        last[k] = when " " rest
        if (ev == "tick") {
            s = rest; sub(/ t=[^ ]+/, "", s); gsub(/tcp=ok\/[0-9]+ms/, "tcp=ok", s)
            if (s == prev[k]) { held[k] = when " " rest; next }
            if (held[k] != "") out(k, "  " held[k] "  (unchanged until here)")
            held[k] = ""; prev[k] = s
        }
        if (ev == "alive") next
        if (held[k] != "") { out(k, "  " held[k] "  (unchanged until here)"); held[k] = "" }
        if (ev != "tick") prev[k] = ""
        if (ev == "end") ended[k] = 1
        out(k, "  " when " " rest)
    }
    END {
        for (i = 1; i <= nruns; i++) {
            k = order[i]; printf "%s", buf[k]
            if (held[k] != "") print "  " held[k] "  (unchanged until here)"
            print "  last line: " last[k]
            if (!ended[k]) print "  no end line: killed (or still running)"
            print ""
        }
    }' "$logf"
}

openrc_unit() {
    cat <<EOF
#!/sbin/openrc-run
# PdMount M5.0 session-end probe (written by session-end-probe.sh install).
description="PdMount session-end probe (M5.0)"
supervisor=supervise-daemon
command="$1"
command_args="run${2:+ $2}"
export DBUS_SESSION_BUS_ADDRESS="unix:path=\${XDG_RUNTIME_DIR}/bus"
retry="TERM/$((linger + 30))/KILL/5"
respawn_delay=5
respawn_max=3
respawn_period=60

depend() {
	need dbus
}
EOF
}

systemd_unit() {
    cat <<EOF
# PdMount M5.0 session-end probe (written by session-end-probe.sh install).
[Unit]
Description=PdMount session-end probe (M5.0)
After=dbus.socket

[Service]
Type=simple
ExecStart="$1" run${2:+ $2}
Restart=on-failure
RestartSec=5
TimeoutStopSec=$((linger + 30))

[Install]
WantedBy=default.target
EOF
}

install_probe() {
    local manager=${1:-} inhibit='' enable=yes self target unit
    shift || true
    for arg; do
        case $arg in
        --inhibit) inhibit=--inhibit ;;
        --no-enable) enable=no ;;
        *) echo "unknown option: $arg" >&2; return 2 ;;
        esac
    done
    self=$(realpath "$0")
    target=$share_dir/session-end-probe.sh
    mkdir -p "$share_dir" "$state_dir"
    [[ $self == "$(realpath -m "$target")" ]] || install -m 0755 "$self" "$target"
    case $manager in
    openrc)
        unit=$config_dir/rc/init.d/$name
        mkdir -p "${unit%/*}"
        openrc_unit "$target" "$inhibit" >"$unit"
        chmod 0755 "$unit"
        echo "wrote $unit"
        if [[ $enable == yes ]]; then
            rc-update --user add "$name" default && rc-service --user "$name" start
        fi
        ;;
    systemd)
        unit=$config_dir/systemd/user/$name.service
        mkdir -p "${unit%/*}"
        systemd_unit "$target" "$inhibit" >"$unit"
        echo "wrote $unit"
        if [[ $enable == yes ]]; then
            systemctl --user daemon-reload && systemctl --user enable --now "$name.service"
        fi
        ;;
    *)
        echo "usage: $0 install openrc|systemd [--inhibit] [--no-enable]" >&2
        return 2
        ;;
    esac
    echo "log: $logf"
}

uninstall_probe() {
    local openrc=$config_dir/rc/init.d/$name systemd=$config_dir/systemd/user/$name.service
    mkdir -p "$state_dir"
    touch "$quiet"
    if [[ -e $openrc ]]; then
        rc-service --user "$name" stop
        rc-update --user del "$name" default
        rm -f "$openrc"
        echo "removed $openrc"
    fi
    if [[ -e $systemd ]]; then
        systemctl --user disable --now "$name.service"
        rm -f "$systemd"
        systemctl --user daemon-reload
        echo "removed $systemd"
    fi
    rm -f "$quiet" "$share_dir/session-end-probe.sh"
    rmdir "$share_dir" 2>/dev/null
    echo "log kept: $logf"
}

case ${1:-} in
run) shift; run "$@" ;;
install) shift; install_probe "$@" ;;
uninstall) uninstall_probe ;;
summary) summary ;;
log) cat "$logf" ;;
*)
    sed -n '2,/^set -uo/{/^set -uo/d;s/^# \{0,1\}//;p}' "$0" >&2
    exit 2
    ;;
esac
