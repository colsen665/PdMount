#!/usr/bin/env bash
# N1 (DESIGN.md §15.3, §8.7): link and symlink give EPERM,
# renameat2(RENAME_EXCHANGE) EINVAL, renameat2(RENAME_NOREPLACE) onto an
# existing file EEXIST; nothing changes.
source "$(dirname "$0")/lib.sh"
prepare_mount
scenario_area n1
printf a >"$area/a"
printf b >"$area/b"

step "N1 [$VERIFY_MODE]: unsupported operations"
python3 -I - "$area" <<'PY'
import ctypes, errno, os, sys
d = sys.argv[1]
a, b = os.path.join(d, "a"), os.path.join(d, "b")
libc = ctypes.CDLL(None, use_errno=True)

def renameat2(src, dst, flags):
    if libc.renameat2(-100, src.encode(), -100, dst.encode(), flags) != 0:
        code = ctypes.get_errno()
        raise OSError(code, os.strerror(code))

def expect(name, call, code):
    try:
        call()
    except OSError as error:
        if error.errno != code:
            sys.exit(f"{name}: {errno.errorcode.get(error.errno)}, expected {errno.errorcode[code]}")
        print(f"{name}: {errno.errorcode[code]}")
        return
    sys.exit(f"{name} succeeded, expected {errno.errorcode[code]}")

expect("link", lambda: os.link(a, os.path.join(d, "c")), errno.EPERM)
expect("symlink", lambda: os.symlink("a", os.path.join(d, "s")), errno.EPERM)
expect("RENAME_EXCHANGE", lambda: renameat2(a, b, 2), errno.EINVAL)
expect("RENAME_NOREPLACE", lambda: renameat2(a, b, 1), errno.EEXIST)
PY
verify_cycle
area=$TEST_ROOT/n1
[[ "$(cat "$area/a")$(cat "$area/b")" == ab ]] || fail "a and b changed"
[[ ! -e "$area/c" && ! -e "$area/s" ]] || fail "a link or symlink was created"
check_hot_path
echo "N1 [$VERIFY_MODE]: PASS"
