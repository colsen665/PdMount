# shellcheck shell=bash
# Helpers for the access-rule scenarios A1-A6 (DESIGN.md §11, §15.3).
# Rules are local state: `reset --keep-login` removes them, so in fresh mode
# the scenarios check only what reached the backend. Device B of the `second`
# mode never had the rules, so it is checked like a fresh mount.

# Runs a command that must fail with "Read-only file system" (EROFS, D019).
expect_erofs() { # <what> <command...>
    local what=$1 err
    shift
    if err=$("$@" 2>&1); then fail "$what succeeded in a read-only subtree"; fi
    grep -qi 'read-only file system' <<<"$err" || fail "$what failed differently: $err"
    echo "$what: Read-only file system"
}

expect_ok() { # <what> <command...>
    local what=$1 err
    shift
    err=$("$@" 2>&1) || fail "$what failed: $err"
}

rules_kept() { [[ $VERIFY_MODE != fresh && $VERIFY_MODE != second ]]; }
