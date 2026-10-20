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
#   s2  cancelled, keyring: load, save and cache key fail; no new cache key
#   s2a cancelled, allow: a new cache key goes to a file
#   s3  unlocked, keyring: the files from s1a/s2a move into the keyring
#   f1  unlocked, keyring: store a session for old@
#   f2  unlocked, force: store new@ in a file without calling the keyring
#   f3  unlocked, keyring: the file (new@) wins over the keyring (old@)
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
((${#cases[@]})) || cases=(s0 s1 s1a s1b s2 s2a s3 f1 f2 f3)

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
        s0 | s3 | f1 | f3) args=(unlocked keyring) ;;
        f2) args=(unlocked force) ;;
        s1 | s1b) args=(unanswered keyring) ;;
        s1a) args=(unanswered allow) ;;
        s2) args=(cancelled keyring) ;;
        s2a) args=(cancelled allow) ;;
        *) echo "unknown case: $c" >&2; exit 2 ;;
    esac
    printf '\n==> %s (%s, %s)\n' "$c" "${args[@]}"
    dbus-run-session -- "$0" --inner "$c" "${args[@]}" 2>"$root/$c.bus.log"
done
printf '\nLogs: %s\n' "$root"
