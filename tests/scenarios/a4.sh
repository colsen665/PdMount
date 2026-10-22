#!/usr/bin/env bash
# A4 (DESIGN.md §15.3): `access list`, `access list <dir>` and
# `access effective <file>` show the rules and the deciding rule.
source "$(dirname "$0")/lib.sh"
source "$(dirname "$0")/access-lib.sh"
prepare_mount
scenario_area a4
mkdir -p "$area/archive/inbox" "$area/other"
printf x >"$area/archive/inbox/a.txt"

step "A4 [$VERIFY_MODE]: list and effective"
pd access set ro "$area/archive"
pd access set rw "$area/archive/inbox"
check_rules() {
    local base=/pdmount-test/a4 list
    list=$(pd access list)
    echo "$list"
    grep -qE "^ro +$base/archive +$base/archive +cli +active$" <<<"$list" || fail "the ro rule is missing"
    grep -qE "^rw +$base/archive/inbox +$base/archive/inbox +cli +active$" <<<"$list" || fail "the rw rule is missing"
    grep -q '^(default: rw)$' <<<"$list" || fail "the default is missing"
    [[ "$(pd access list "$TEST_ROOT/a4/other" | grep -c '^r[ow] ')" == 0 ]] || fail "rules listed below other/"
    [[ "$(pd access list "$TEST_ROOT/a4/archive/inbox" | grep -c '^r[ow] ')" == 1 ]] || fail "list <dir> is wrong"
    [[ "$(pd access effective "$TEST_ROOT/a4/archive/inbox/a.txt")" == "rw  (rule: $base/archive/inbox)" ]] ||
        fail "effective a.txt: $(pd access effective "$TEST_ROOT/a4/archive/inbox/a.txt")"
    [[ "$(pd access effective "$TEST_ROOT/a4/other")" == "rw  (default)" ]] || fail "effective other"
    [[ "$(pd access effective "$TEST_ROOT/a4/archive")" == "ro  (rule: $base/archive)" ]] || fail "effective archive"
}
check_rules
verify_cycle
if rules_kept; then
    check_rules
    pd access unset "$TEST_ROOT/a4/archive/inbox"
    pd access unset "$TEST_ROOT/a4/archive"
fi
check_hot_path
echo "A4 [$VERIFY_MODE]: PASS"
