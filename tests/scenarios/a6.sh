#!/usr/bin/env bash
# A6 (DESIGN.md §15.3, §11.4): open a file for writing, write, set its
# parent `ro`, write again (EROFS), close. The first write still uploads.
source "$(dirname "$0")/lib.sh"
source "$(dirname "$0")/access-lib.sh"
prepare_mount
scenario_area a6
mkdir -p "$area/dir"

step "A6 [$VERIFY_MODE]: switch to ro while a writer is open"
python3 -I - "$area/dir/file" "$PDMOUNT" "$PROFILE" "${secrets_flag[@]}" <<'PY'
import errno, os, subprocess, sys
path, pdmount, profile, *flags = sys.argv[1:]
fd = os.open(path, os.O_WRONLY | os.O_CREAT, 0o644)
os.write(fd, b"first")
subprocess.run([pdmount, "--profile", profile, *flags, "access", "set", "ro", os.path.dirname(path)],
               check=True, stdout=subprocess.DEVNULL)
try:
    os.write(fd, b"second")
    sys.exit("the second write succeeded")
except OSError as error:
    if error.errno != errno.EROFS:
        sys.exit(f"the second write failed with {errno.errorcode.get(error.errno)}, expected EROFS")
os.close(fd)
print("second write: EROFS")
PY
wait_queue_empty
if rules_kept; then pd access unset "$area/dir"; fi
verify_cycle
[[ "$(cat "$TEST_ROOT/a6/dir/file")" == first ]] || fail "content is '$(cat "$TEST_ROOT/a6/dir/file")', expected 'first'"
if [[ $VERIFY_MODE != fresh ]]; then pd access unset "$TEST_ROOT/a6/dir" >/dev/null 2>&1 || true; fi
check_hot_path
echo "A6 [$VERIFY_MODE]: PASS"
