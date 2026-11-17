#!/usr/bin/env bash
# Keyring tests against a real gnome-keyring (D024 reproduction, D029 modes).
#
# Each case runs in its own private D-Bus session (dbus-run-session) with HOME
# and the XDG directories under target/keyring-d024/, so the user's keyring is
# never touched. The path is kept short because gnome-keyring's control socket
# must fit the 108-byte Unix socket limit. Secrets are throwaway strings; no
# Proton login is involved. Cases share one profile directory and run in order.
#
# Keyring states: unlocked; locked with a stub prompter that never answers (the
# unnoticed dialog); locked with the real gcr-prompter and no display, which
# dismisses the prompt at once (what Cancel returns to the client).
#
# Cases (keyring state, secrets mode):
#   s0  unlocked, keyring: the raw keyring call and pdmount use the keyring
#   s1  unanswered, keyring: the raw call hangs; pdmount fails after the timeout
#   s1a unanswered, allow: pdmount falls back to the 0600 file after the timeout
#   s1b unanswered, keyring: `doctor` reports the lock without prompting
#   p0  unlocked, keyring: store a session under the CLI's profile (for s1c)
#   s1c locked, keyring: `status` reports the lock without prompting (D104)
#   s2  cancelled, keyring: load, save and cache key fail; no new cache key
#   s2a cancelled, allow: a new cache key goes to a file
#   s3  unlocked, keyring: the files from s1a/s2a move into the keyring
#   f1  unlocked, keyring: store a session for old@
#   f2  unlocked, force: store new@ in a file without calling the keyring
#   f3  unlocked, keyring: the file (new@) wins over the keyring (old@)
#   h1  unlocked, keyring: the process runs on a second bus without a Secret Service and reaches
#       this one, attached, through `pdmount keyring-helper` (D102): probe, save, load, remove
#   o1  unlocked, keyring: `doctor` on a second bus with no Secret Service and no activation file
#       says so at once, without waiting for a call to time out (D101)
#   o2  unlocked, keyring: `doctor` on a second bus whose activation file for org.freedesktop.secrets
#       starts a command that never claims the name: "did not start", after the 2 s call timeout (D101)
#   o3  unlocked, keyring: `doctor` on this bus, where gnome-keyring runs: owned, default collection unlocked
#   o4  unlocked, keyring: `doctor` on a second bus like o2's, with this bus (gnome-keyring) at
#       $XDG_RUNTIME_DIR/bus as the service's: the keyring is reached there through the helper,
#       unlocked (D102 amended 2026-11-14)
#   w1  unlocked, keyring: gnome-keyring is paused (SIGSTOP) for 3 s during a save, so a call
#       times out with NoReply; the save reads the value back, writes once more, and succeeds
#       (D106 amended, rule 3); a load then returns the new session
#
# Usage: scripts/repro/keyring-d024.sh [case ...]   (default: all, in order)
set -euo pipefail

repo=$(cd "$(dirname "$0")/../.." && pwd)
root=$repo/target/keyring-d024
password=d024-throwaway
raw_timeout=60

if [[ "${1:-}" == "--inner" ]]; then
    case_name=$2
    state=$3 # unlocked | unanswered | cancelled
    export PDMOUNT_REPRO_MODE=$4
    log() { printf '%s %s\n' "$(date +%T)" "$*"; }
    secret() {
        gdbus call --session --dest org.freedesktop.secrets "$@"
    }
    if [[ $state == unanswered ]]; then
        python3 -I "$repo/scripts/repro/stub-prompter.py" "$root/$case_name.prompter.log" &
        timeout 10 gdbus wait --session org.gnome.keyring.SystemPrompter
    fi
    if [[ $state == unlocked ]]; then
        printf %s "$password" | gnome-keyring-daemon --foreground --components=secrets --unlock \
            >"$root/$case_name.gkd.log" 2>&1 &
        gkd_pid=$!
    else
        gnome-keyring-daemon --foreground --components=secrets </dev/null \
            >"$root/$case_name.gkd.log" 2>&1 &
    fi
    trap 'kill $(jobs -p) 2>/dev/null || true' EXIT
    timeout 10 gdbus wait --session org.freedesktop.secrets
    sleep 0.5
    login=/org/freedesktop/secrets/collection/login
    if [[ $state != unlocked ]]; then
        secret --object-path /org/freedesktop/secrets \
            --method org.freedesktop.Secret.Service.Lock "['$login']" >/dev/null
    fi
    log "login collection locked: $(secret --object-path $login \
        --method org.freedesktop.DBus.Properties.Get org.freedesktop.Secret.Collection Locked)"

    run_test() { # name timeout [VAR=value ...]
        local name=$1 limit=$2 start rc=0
        shift 2
        start=$(date +%s)
        env "$@" timeout "$limit" "$TEST_BIN" --ignored --exact "credentials::tests::repro_d024::$name" \
            --nocapture --test-threads=1 >"$root/$case_name.$name.log" 2>&1 || rc=$?
        log "$name: exit=$rc after $(($(date +%s) - start)) s;" \
            "$(grep -ho 'REPRO .*' "$root/$case_name.$name.log" || echo 'no result (killed)')"
    }
    case $case_name in
        s0)
            run_test raw_set_password 30
            run_test save 60 PDMOUNT_REPRO_USER=s0@example.test
            run_test load 60
            run_test cache_key 60
            run_test remove 60
            ;;
        s1)
            run_test save 60
            run_test raw_set_password $raw_timeout
            ;;
        s1a)
            run_test save 60 PDMOUNT_REPRO_USER=s1a@example.test
            ;;
        s1b)
            start=$(date +%s)
            rc=0
            timeout 60 "$PDMOUNT_BIN" --profile repro-d024 doctor >"$root/$case_name.doctor.log" 2>&1 || rc=$?
            log "doctor: exit=$rc after $(($(date +%s) - start)) s"
            grep -E '\] (keyring|login)' "$root/$case_name.doctor.log" | sed 's/^/    /'
            ;;
        p0)
            run_test save 60 PDMOUNT_REPRO_USER=p0@example.test PDMOUNT_REPRO_PROFILE=repro-d024
            ;;
        s1c)
            start=$(date +%s)
            rc=0
            # A short runtime directory: the control socket path must fit SUN_LEN.
            short_run=$(mktemp -d)
            XDG_RUNTIME_DIR=$short_run timeout 60 "$PDMOUNT_BIN" --profile repro-d024 status \
                >"$root/$case_name.status.log" 2>&1 || rc=$?
            rm -rf "$short_run"
            log "status: exit=$rc after $(($(date +%s) - start)) s"
            grep -E '^(Session|error)' "$root/$case_name.status.log" | sed 's/^/    /' || true
            # The stub prompter logs every unlock dialog the client asked for.
            if grep -q BeginPrompting "$root/$case_name.prompter.log" 2>/dev/null; then
                log "status: FAIL, it opened an unlock prompt"
            elif grep -q 'keyring locked' "$root/$case_name.status.log"; then
                log "status: ok, no prompt"
            else
                log "status: FAIL, no 'keyring locked' line"
            fi
            ;;
        s2)
            run_test load 60
            run_test save 60
            run_test cache_key 60
            ;;
        s2a)
            run_test cache_key 60
            ;;
        s3)
            run_test load 60
            run_test cache_key 60
            run_test remove 60
            ;;
        f1)
            run_test save 60 PDMOUNT_REPRO_USER=old@example.test
            ;;
        f2)
            run_test save 60 PDMOUNT_REPRO_USER=new@example.test
            run_test load 60
            ;;
        f3)
            run_test load 60
            run_test remove 60
            ;;
        w1)
            run_test save 60 PDMOUNT_REPRO_USER=w0@example.test
            # Paused, gnome-keyring takes calls but answers none: the client's
            # 2 s per call runs out (NoReply), as with a slow file rewrite.
            kill -STOP "$gkd_pid"
            (sleep 3 && kill -CONT "$gkd_pid") &
            resume=$!
            run_test save 60 PDMOUNT_REPRO_USER=w1@example.test
            wait "$resume" || kill -CONT "$gkd_pid"
            grep -ho 'a keyring write timed out.*' "$root/$case_name.save.log" | sed 's/^/    /' || true
            run_test load 60
            if grep -q 'REPRO save=keyring' "$root/$case_name.save.log" &&
                grep -q 'keyring write timed out' "$root/$case_name.save.log" &&
                grep -q 'REPRO load=w1@example.test@keyring' "$root/$case_name.load.log"; then
                log "paused save: ok, the timed-out write was checked and the new session is stored"
            else
                log "paused save: FAIL"
            fi
            run_test remove 60
            ;;
        h1)
            # Bus A: the daemon's own, as the OpenRC user bus, with nothing on it.
            bus_a=$XDG_RUNTIME_DIR/h1-bus
            cat >"$root/h1-bus.conf" <<EOF
<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:path=$bus_a</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
EOF
            dbus-daemon --config-file="$root/h1-bus.conf" --nofork --nopidfile &
            for _ in $(seq 50); do [[ -S $bus_a ]] && break; sleep 0.1; done
            run_test attached 60 PDMOUNT_REPRO_ATTACH="$DBUS_SESSION_BUS_ADDRESS" \
                DBUS_SESSION_BUS_ADDRESS="unix:path=$bus_a" PDMOUNT_REPRO_USER=h1@example.test
            if grep -q 'REPRO attached=state:Unlocked,saved:keyring,loaded:h1@example.test@keyring,removed:true' \
                "$root/$case_name.attached.log"; then
                log "attached: ok, the helper used the attached bus"
            else
                log "attached: FAIL"
            fi
            grep -h 'REPRO direct_state' "$root/$case_name.attached.log" | sed 's/^/    /' || true
            ;;
        o1 | o2 | o4)
            # A bus of its own, as the OpenRC user bus: o1 has no provider, o2 an activation file
            # whose command runs for a long time without claiming the name. o4: o2's bus as the
            # desktop's, and this bus, where gnome-keyring runs, as the service's.
            bus=$XDG_RUNTIME_DIR/$case_name-bus
            mkdir -p "$root/$case_name-data/dbus-1/services"
            if [[ $case_name != o1 ]]; then
                printf '[D-BUS Service]\nName=org.freedesktop.secrets\nExec=/bin/sh -c "sleep 60"\n' \
                    >"$root/$case_name-data/dbus-1/services/org.freedesktop.secrets.service"
            fi
            cat >"$root/$case_name-bus.conf" <<CONF
<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:path=$bus</listen>
  <auth>EXTERNAL</auth>
  <servicedir>$root/$case_name-data/dbus-1/services</servicedir>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
CONF
            dbus-daemon --config-file="$root/$case_name-bus.conf" --nofork --nopidfile &
            for _ in $(seq 50); do [[ -S $bus ]] && break; sleep 0.1; done
            run_dir=$XDG_RUNTIME_DIR
            if [[ $case_name == o4 ]]; then
                # A short runtime directory (SUN_LEN) whose bus is this one.
                run_dir=$(mktemp -d)
                this_bus=${DBUS_SESSION_BUS_ADDRESS#unix:path=}
                ln -s "${this_bus%%,*}" "$run_dir/bus"
            fi
            start=$(date +%s)
            rc=0
            DBUS_SESSION_BUS_ADDRESS="unix:path=$bus" XDG_DATA_HOME="$root/$case_name-data" XDG_RUNTIME_DIR="$run_dir" \
                timeout 60 "$PDMOUNT_BIN" --profile repro-d024 doctor >"$root/$case_name.doctor.log" 2>&1 || rc=$?
            elapsed=$(($(date +%s) - start))
            [[ $case_name != o4 ]] || rm -rf "$run_dir"
            log "doctor: exit=$rc after $elapsed s"
            grep -E '\] keyring' "$root/$case_name.doctor.log" | sed 's/^/    /'
            if [[ $case_name == o1 ]] &&
                grep -q 'no Secret Service on the session bus' "$root/$case_name.doctor.log" && ((elapsed < 5)); then
                log "doctor: ok, no Secret Service, said at once"
            elif [[ $case_name == o2 ]] &&
                grep -q 'the Secret Service did not start' "$root/$case_name.doctor.log" &&
                grep -q 'sleep 60' "$root/$case_name.doctor.log"; then
                log "doctor: ok, the activation file's command never claimed the name"
            elif [[ $case_name == o4 ]] &&
                grep -q "Secret Service running on the service's session bus" "$root/$case_name.doctor.log" &&
                grep -q 'default collection unlocked' "$root/$case_name.doctor.log" && ((elapsed < 10)); then
                log "doctor: ok, the keyring on the service's bus"
            else
                log "doctor: FAIL"
            fi
            ;;
        o3)
            rc=0
            timeout 60 "$PDMOUNT_BIN" --profile repro-d024 doctor >"$root/$case_name.doctor.log" 2>&1 || rc=$?
            grep -E '\] keyring' "$root/$case_name.doctor.log" | sed 's/^/    /'
            if grep -q 'Secret Service running, default collection unlocked' "$root/$case_name.doctor.log"; then
                log "doctor: ok, owned and unlocked"
            else
                log "doctor: FAIL"
            fi
            ;;
    esac
    if [[ -s "$root/$case_name.prompter.log" ]]; then
        sed 's/^/    prompter: /' "$root/$case_name.prompter.log"
    fi
    exit 0
fi

for tool in dbus-run-session gnome-keyring-daemon gdbus python3; do
    command -v "$tool" >/dev/null || { echo "missing: $tool" >&2; exit 1; }
done
python3 -I -c 'import dbus, gi' 2>/dev/null || { echo "missing: python3 dbus and gi modules" >&2; exit 1; }

cases=("$@")
((${#cases[@]})) || cases=(s0 s1 s1a s1b p0 s1c s2 s2a s3 f1 f2 f3 h1 o1 o2 o3 o4 w1)

cd "$repo"
cargo build -q -p pdmount
TEST_BIN=$(cargo test -q -p pdmount --no-run --message-format=json 2>/dev/null |
    python3 -I -c 'import json, sys
for line in sys.stdin:
    m = json.loads(line)
    if m.get("reason") == "compiler-artifact" and m.get("executable") and m["target"]["name"] == "pdmount" and m["profile"]["test"]:
        print(m["executable"])')
PDMOUNT_BIN=$repo/target/debug/pdmount

rm -rf "$root"
mkdir -p "$root/home" "$root/run" "$root/profile"
chmod 700 "$root/run"
# A short timeout keeps the unanswered cases quick; the default is 120 s.
export HOME=$root/home XDG_DATA_HOME=$root/home/.local/share XDG_CONFIG_HOME=$root/home/.config \
    XDG_CACHE_HOME=$root/home/.cache XDG_STATE_HOME=$root/home/.local/state XDG_RUNTIME_DIR=$root/run \
    PDMOUNT_REPRO_DIR=$root/profile PDMOUNT_KEYRING_TIMEOUT_SECS=10 TEST_BIN PDMOUNT_BIN
unset DISPLAY WAYLAND_DISPLAY DBUS_SESSION_BUS_ADDRESS

# The first unlocked daemon creates the login keyring; every case needs it.
if [[ ${cases[0]} != s0 ]]; then
    dbus-run-session -- "$0" --inner none unlocked keyring >/dev/null 2>&1
fi
for c in "${cases[@]}"; do
    case $c in
        s0 | s3 | f1 | f3 | p0 | h1 | o1 | o2 | o3 | o4 | w1) args=(unlocked keyring) ;;
        f2) args=(unlocked force) ;;
        s1 | s1b | s1c) args=(unanswered keyring) ;;
        s1a) args=(unanswered allow) ;;
        s2) args=(cancelled keyring) ;;
        s2a) args=(cancelled allow) ;;
        *) echo "unknown case: $c" >&2; exit 2 ;;
    esac
    printf '\n==> %s (%s, %s)\n' "$c" "${args[@]}"
    dbus-run-session -- "$0" --inner "$c" "${args[@]}" 2>"$root/$c.bus.log"
done
printf '\nLogs: %s\n' "$root"
