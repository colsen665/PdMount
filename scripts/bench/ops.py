"""Times namespace operations on a mount from the client side.

Usage: ops.py <dir> <iterations> <out.json>
Each iteration: create, write 4 KiB, close, chmod, rename, mkdir, rmdir, and
every second one an unlink; every tenth one also an fsync after the write. Writes the latencies (ns) per operation as JSON.
"""
import json
import os
import sys
import time

d, n, out = sys.argv[1], int(sys.argv[2]), sys.argv[3]
os.makedirs(d, exist_ok=True)
lat = {k: [] for k in ("create", "write", "fsync", "close", "setattr", "rename", "mkdir", "rmdir",
                      "unlink")}
buf = b"x" * 4096


def timed(op, f, *args):
    start = time.perf_counter_ns()
    result = f(*args)
    lat[op].append(time.perf_counter_ns() - start)
    return result


for i in range(n):
    path, moved, sub = f"{d}/f{i}", f"{d}/g{i}", f"{d}/d{i}"
    fd = timed("create", os.open, path, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o644)
    timed("write", os.write, fd, buf)
    if i % 10 == 0:
        timed("fsync", os.fsync, fd)
    timed("close", os.close, fd)
    timed("setattr", os.chmod, path, 0o600)
    timed("rename", os.rename, path, moved)
    timed("mkdir", os.mkdir, sub)
    timed("rmdir", os.rmdir, sub)
    if i % 2:
        timed("unlink", os.unlink, moved)
with open(out, "w") as f:
    json.dump(lat, f)
