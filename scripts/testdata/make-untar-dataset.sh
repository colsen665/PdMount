#!/usr/bin/env bash
# Writes the M4 untar benchmark's tar (D057, scripts/bench/m4.sh): 5000 files of 0-16 KiB
# in 100 directories on 2 levels (10 top-level directories with 9 subdirectories each),
# mode 0644, fixed mtimes and owners.
#
# The output is deterministic: sizes and bytes come from a seeded generator, so running the
# script twice, as any user, gives the same tar byte for byte.
#
# Usage: scripts/testdata/make-untar-dataset.sh <out.tar>
set -euo pipefail

[[ $# -eq 1 ]] || { echo "usage: $0 <out.tar>" >&2; exit 2; }

python3 -I - "$1" <<'EOF'
import io, random, sys, tarfile

out = sys.argv[1]
rng = random.Random("pdmount-untar-v1")
mtime = 1_700_000_000
dirs = []
for top in range(10):
    dirs.append(f"untar/d{top:02}")
    dirs.extend(f"untar/d{top:02}/s{sub}" for sub in range(9))


def info(name, kind, size=0, mode=0o644):
    entry = tarfile.TarInfo(name)
    entry.type, entry.size, entry.mode, entry.mtime = kind, size, mode, mtime
    entry.uid = entry.gid = 0
    entry.uname = entry.gname = ""
    return entry


with tarfile.open(out, "w", format=tarfile.GNU_FORMAT) as tar:
    tar.addfile(info("untar", tarfile.DIRTYPE, mode=0o755))
    for d in dirs:
        tar.addfile(info(d, tarfile.DIRTYPE, mode=0o755))
    for i in range(5000):
        size = rng.randrange(0, 16 * 1024 + 1)
        data = rng.randbytes(size)
        name = f"{dirs[i % len(dirs)]}/f{i:04}.dat"
        tar.addfile(info(name, tarfile.REGTYPE, size), io.BytesIO(data))
EOF
