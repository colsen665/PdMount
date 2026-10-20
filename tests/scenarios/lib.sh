# shellcheck shell=bash
# Shared setup for the scenario scripts (DESIGN.md §15.3). Source it.
#
# Environment:
#   VERIFY_MODE  live (default): use the current mount, mounting if needed.
#                fresh: unmount, `reset --keep-login`, mount; everything is read from the API.
#   PDMOUNT      pdmount binary (default: target/release/pdmount, else target/debug/pdmount)
#   PROFILE      profile (default: default)
#   SECRETS      force (default): --force-file-secrets; allow: --allow-file-secrets; keyring: no flag
#                (D029; only keyring tests use the keyring)
#   MNT          mountpoint (default: ~/ProtonDrive)
#   DATASET      local copy of the M1 dataset, regenerated if missing
#                (default: ~/.cache/pdmount-scenarios/m1; see scripts/testdata/make-m1-dataset.sh)
set -euo pipefail

repo=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
VERIFY_MODE=${VERIFY_MODE:-live}
PROFILE=${PROFILE:-default}
MNT=${MNT:-$HOME/ProtonDrive}
SECRETS=${SECRETS:-force}
case $SECRETS in
    force) secrets_flag=(--force-file-secrets) ;;
    allow) secrets_flag=(--allow-file-secrets) ;;
    keyring) secrets_flag=() ;;
    *) echo "SECRETS must be force, allow or keyring" >&2; exit 2 ;;
esac
DATASET=${DATASET:-$HOME/.cache/pdmount-scenarios/m1}
if [[ -z "${PDMOUNT:-}" ]]; then
    PDMOUNT=$repo/target/release/pdmount
    [[ -x "$PDMOUNT" ]] || PDMOUNT=$repo/target/debug/pdmount
fi
REMOTE=$MNT/pdmount-m1
LOCAL=$DATASET/pdmount-m1

pd() { "$PDMOUNT" --profile "$PROFILE" "${secrets_flag[@]}" "$@"; }
step() { printf '\n==> %s\n' "$*"; }
fail() { echo "FAIL: $*" >&2; exit 1; }

is_mounted() { pd status 2>/dev/null | grep -q '^Mount: *mounted'; }

ensure_dataset() {
    if [[ ! -f "$LOCAL/MANIFEST.sha256" ]]; then
        step "generating the local dataset in $DATASET"
        rm -rf "$DATASET"
        mkdir -p "$DATASET"
        "$repo/scripts/testdata/make-m1-dataset.sh" "$DATASET"
    fi
}

prepare_mount() {
    case "$VERIFY_MODE" in
        live)
            is_mounted || pd mount "$MNT"
            ;;
        fresh)
            step "fresh: reset local state and mount"
            if is_mounted; then pd unmount "$MNT"; fi
            pd reset --keep-login
            pd mount "$MNT"
            ;;
        *) fail "VERIFY_MODE=$VERIFY_MODE is not supported in M1 (live, fresh)" ;;
    esac
    [[ -d "$REMOTE" ]] || fail "$REMOTE not found; upload pdmount-m1 to the root of My Files first"
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
    work=$(mktemp -d "${TMPDIR:-/tmp}/pdmount-scenario.XXXXXX")
    trap 'rm -rf "$work"' EXIT
}
