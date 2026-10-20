#!/usr/bin/env bash
# Generates the M1 exit-check dataset (DESIGN.md §16, scenarios R6 and G6 in §15.3).
#
# The output is deterministic: running the script twice, as any user, gives the same
# bytes for every file in src/ and big/ and the same commit hashes in r.git. The user
# uploads <dir>/pdmount-m1 with the official client; the scenario scripts regenerate
# it locally and compare against the mount.
#
# Usage: scripts/testdata/make-m1-dataset.sh [--no-big] <dir>
#   --no-big  skip big/2GiB.bin (for quick local runs)
#
# Size: about 2.3 GiB (2 GiB of it is big/2GiB.bin).
set -euo pipefail

big=1
if [[ "${1:-}" == "--no-big" ]]; then
    big=0
    shift
fi
[[ $# -eq 1 ]] || { echo "usage: $0 [--no-big] <dir>" >&2; exit 2; }

out="$1/pdmount-m1"
if [[ -e "$out" ]]; then
    echo "$out already exists" >&2
    exit 1
fi
mkdir -p "$out"/{src,big}

# Deterministic pseudo-random bytes: AES-256-CTR keystream with a fixed passphrase.
# openssl dies of SIGPIPE when head has enough, so pipefail is off inside.
stream() ( # <seed> <bytes>
    set +o pipefail
    openssl enc -aes-256-ctr -nosalt -pbkdf2 -pass "pass:pdmount-m1-$1" </dev/zero 2>/dev/null |
        head -c "$2"
)

echo "src/: 1000 small files in nested directories, one 100 MiB file"
python3 -I - "$out/src" <<'EOF'
import os, random, sys
root = sys.argv[1]
rng = random.Random(20261019)
names = ["alpha", "beta", "gamma", "delta", "Ünïcödé", "with space", "UPPER", "x.y.z"]
dirs = [""]
for i in range(60):
    parent = rng.choice(dirs)
    if parent.count("/") >= 3:
        parent = ""
    d = os.path.join(parent, f"{rng.choice(names)}-{i:02d}")
    dirs.append(d)
    os.makedirs(os.path.join(root, d), exist_ok=True)
for i in range(1000):
    d = rng.choice(dirs)
    size = rng.choice([0, 1, 100, 4095, 4096, 4097, 65536]) if i % 10 == 0 else rng.randrange(0, 64 * 1024)
    path = os.path.join(root, d, f"file-{i:04d}{rng.choice(['.txt', '.bin', '', '.tar.gz'])}")
    with open(path, "wb") as f:
        f.write(rng.randbytes(size))
    os.utime(path, (1700000000 + i * 37, 1700000000 + i * 37))
EOF
stream src-large $((100 * 1024 * 1024)) >"$out/src/large-100MiB.bin"

echo "big/: 5 MiB, 41 MiB$( ((big)) && echo ", 2 GiB")"
stream big-5 $((5 * 1024 * 1024)) >"$out/big/5MiB.bin"
stream big-41 $((41 * 1024 * 1024)) >"$out/big/41MiB.bin"
if ((big)); then
    stream big-2048 $((2 * 1024 * 1024 * 1024)) >"$out/big/2GiB.bin"
fi

echo "r.git: bare repository, 40 commits, one pack over 40 MiB"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
export GIT_AUTHOR_NAME=pdmount GIT_AUTHOR_EMAIL=pdmount@localhost
export GIT_COMMITTER_NAME=pdmount GIT_COMMITTER_EMAIL=pdmount@localhost
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1
# No background auto-gc: it races with the cleanup of the work tree.
export GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=gc.auto GIT_CONFIG_VALUE_0=0
git init -q --ref-format=files -b main "$work/w"
for i in $(seq 1 40); do
    stamp="@$((1700000000 + i * 3600)) +0000"
    mkdir -p "$work/w/dir$((i % 5))"
    printf 'line %s\n' $(seq 1 "$i") >"$work/w/notes.txt"
    # 1.25 MiB of incompressible data per commit, 50 MiB in total.
    stream "git-$i" $((1280 * 1024)) >"$work/w/dir$((i % 5))/blob$i.bin"
    if ((i % 7 == 0)); then
        git -C "$work/w" mv -k "dir$((i % 5))/blob$((i - 5)).bin" "dir$((i % 5))/renamed$i.bin" 2>/dev/null || true
    fi
    git -C "$work/w" add -A
    GIT_AUTHOR_DATE="$stamp" GIT_COMMITTER_DATE="$stamp" git -C "$work/w" commit -q -m "commit $i"
done
git clone -q --bare --ref-format=files "$work/w" "$out/r.git"
git -C "$out/r.git" gc -q
# The web client drops empty directories, and gc leaves refs/heads empty
# (everything is in packed-refs). A loose ref keeps refs/ present.
head=$(git -C "$out/r.git" rev-parse main)
echo "$head" >"$out/r.git/refs/heads/main"
echo "$head" >"$out/HEAD-COMMIT"

echo "MANIFEST.sha256"
(cd "$out" && find src big -type f -print0 | sort -z | xargs -0 sha256sum >MANIFEST.sha256)

pack=$(find "$out/r.git/objects/pack" -name '*.pack' -printf '%s\n')
echo
echo "done: $out ($(du -sh "$out" | cut -f1)); pack size $((pack / 1024 / 1024)) MiB; HEAD $(cat "$out/HEAD-COMMIT")"
echo "Upload the folder pdmount-m1 to the root of My Files with the official client."
