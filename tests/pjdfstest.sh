#!/usr/bin/env bash
# The pjdfstest subset (DESIGN.md §15.1) over the fake backend; never the
# real account (D030).
#
# Usage: tests/pjdfstest.sh [<dir>/<nn>.t | <dir> ...]   (default: the subset's directories)
#   PJDFSTEST  a pjdfstest checkout at the commit named in tests/pjdfstest.list, with the
#              binary built (`autoreconf -ifs && ./configure && make pjdfstest`); required.
#              Its directory must allow exec, so not /dev/shm.
#
# The tests run from a copy whose file-type loops cover regular files and
# directories only, since the mount has no other types (§8.7). The failures of
# every test file must be exactly the ones tests/pjdfstest.list expects:
# a new failure fails the run, and so does an expected one that passes now
# (update the list then).
BACKEND=fake
source "$(dirname "$0")/scenarios/lib.sh"

# Local tool locations (AGENTS.md, "Local paths").
[[ -f "$repo/.pdmount-local.env" ]] && source "$repo/.pdmount-local.env"

PJDFSTEST=${PJDFSTEST:?set PJDFSTEST to a pjdfstest checkout; see the header of this script}
list=$repo/tests/pjdfstest.list
[[ -x "$PJDFSTEST/pjdfstest" ]] ||
    fail "no pjdfstest binary in $PJDFSTEST; see the header of $0"
pinned=$(sed -n 's/^# commit: *\([0-9a-f]*\).*/\1/p' "$list")
actual=$(git -C "$PJDFSTEST" rev-parse HEAD)
[[ "$actual" == "$pinned"* ]] ||
    fail "$PJDFSTEST is at $actual, the list was made at $pinned (test numbers may differ)"

suite=$SCENARIO_ROOT/pjdfstest-suite
rm -rf "$suite"
mkdir -p "$suite"
cp -r "$PJDFSTEST/tests" "$suite/"
# A symlink, so the binary stays on an exec file system.
ln -s "$PJDFSTEST/pjdfstest" "$suite/pjdfstest"
sed -i -E \
    -e 's/for type in regular dir fifo block char socket( symlink)?;/for type in regular dir;/' \
    -e 's/for type in regular fifo block char socket( symlink)?;/for type in regular;/' \
    "$suite"/tests/*/*.t

args=("$@")
((${#args[@]})) || args=(open rename unlink mkdir rmdir truncate ftruncate chmod utimensat)
tests=()
for arg in "${args[@]}"; do
    if [[ -d "$suite/tests/$arg" ]]; then
        for t in "$suite/tests/$arg"/*.t; do tests+=("${t#"$suite/tests/"}"); done
    else
        [[ -f "$suite/tests/$arg" ]] || fail "no test $arg"
        tests+=("$arg")
    fi
done

prepare_mount
area=$TEST_ROOT/pjdfstest
rm -rf "$area"
mkdir -p "$area"
results=$SCENARIO_ROOT/pjdfstest-results
rm -rf "$results"
step "pjdfstest: ${#tests[@]} test files in $area"
for t in "${tests[@]}"; do
    if awk -v t="$t" '$1 == t && $2 == "skip" { found = 1 } END { exit !found }' "$list"; then
        continue
    fi
    mkdir -p "$results/${t%/*}"
    (cd "$area" && sh "$suite/tests/$t" >"$results/$t" 2>&1) || true
done

python3 -I - "$list" "$results" "${tests[@]}" <<'PY'
import re, sys
from pathlib import Path

list_file, results, tests = sys.argv[1], Path(sys.argv[2]), sys.argv[3:]

def numbers(spec):
    out = set()
    for part in spec.split(","):
        low, _, high = part.partition("-")
        out.update(range(int(low), int(high or low) + 1))
    return out

expected, skipped = {}, {}
for line in open(list_file):
    line = line.split("#", 1)[0].split()
    if not line:
        continue
    name, spec, reason = line[0], line[1], " ".join(line[2:])
    if spec == "skip":
        skipped[name] = reason
    else:
        expected[name] = numbers(spec)

bad = passed = failed = 0
for name in tests:
    if name in skipped:
        print(f"{name}: skipped ({skipped[name]})")
        continue
    text = (results / name).read_text(errors="replace")
    plan = re.search(r"^1\.\.(\d+)$", text, re.M)
    fails = set()
    for match in re.finditer(r"^not ok (\d+)(.*)$", text, re.M):
        if "# TODO" not in match.group(2):
            fails.add(int(match.group(1)))
    oks = len(re.findall(r"^ok \d+", text, re.M))
    passed += oks
    failed += len(fails)
    want = expected.get(name, set())
    new, fixed = sorted(fails - want), sorted(want - fails)
    problems = []
    if not plan or "Bail out!" in text or "not ok - could not" in text:
        problems.append("did not run cleanly")
    if new:
        problems.append(f"new failures {new}")
    if fixed:
        problems.append(f"expected failures pass now {fixed}")
    if problems:
        bad += 1
        print(f"{name}: FAIL: {'; '.join(problems)}")
        for line in text.splitlines():
            m = re.match(r"^not ok (\d+)", line)
            if m and int(m.group(1)) in new:
                print(f"    {line}")
print(f"{len(tests)} test files: {passed} assertions passed, {failed} failed")
if bad:
    sys.exit(f"pjdfstest: {bad} test files differ from tests/pjdfstest.list")
print("pjdfstest: PASS")
PY
rm -rf "$area"
check_hot_path
