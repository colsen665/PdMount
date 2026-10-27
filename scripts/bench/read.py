"""Times reads of one file on a mount from the client side.

Usage: read.py <file> <label> [pagecache-drop]
Runs three tests on the file and prints MiB/s and p50/p99 per read in µs:
  seq   sequential pread of 1 MiB (the largest FUSE read)
  rand  2000 preads of 128 KiB at random aligned offsets
  mmap  2000 copies of 128 KiB at random aligned offsets from a read-only mapping
With a third argument, the kernel page cache of the file is dropped before each test
(posix_fadvise DONTNEED), so only the daemon's own caches can answer.
"""
import mmap
import os
import random
import sys
import time

path, label = sys.argv[1], sys.argv[2]
drop = len(sys.argv) > 3
size = os.stat(path).st_size
chunk, small, count = 1 << 20, 128 << 10, 2000
rng = random.Random(42)
offsets = [rng.randrange(0, size // small) * small for _ in range(count)]


def pct(values, p):
    values = sorted(values)
    return values[min(len(values) - 1, int(len(values) * p))] / 1000


def report(name, lat, nbytes, elapsed):
    print(f"{label:26} {name:5} {nbytes / elapsed / (1 << 20):8.1f} MiB/s  "
          f"p50 {pct(lat, .5):8.0f} µs  p99 {pct(lat, .99):8.0f} µs")


def run(name, fn):
    fd = os.open(path, os.O_RDONLY)
    if drop:
        os.posix_fadvise(fd, 0, 0, os.POSIX_FADV_DONTNEED)
    lat, nbytes = [], 0
    start = time.perf_counter()
    for n, off in fn(fd):
        lat.append(n)
        nbytes += off
    elapsed = time.perf_counter() - start
    os.close(fd)
    report(name, lat, nbytes, elapsed)


def seq(fd):
    off = 0
    while off < size:
        t = time.perf_counter_ns()
        data = os.pread(fd, chunk, off)
        yield time.perf_counter_ns() - t, len(data)
        off += chunk


def rand(fd):
    for off in offsets:
        t = time.perf_counter_ns()
        data = os.pread(fd, small, off)
        yield time.perf_counter_ns() - t, len(data)


def mapped(fd):
    with mmap.mmap(fd, 0, prot=mmap.PROT_READ) as m:
        for off in offsets:
            t = time.perf_counter_ns()
            data = m[off:off + small]
            yield time.perf_counter_ns() - t, len(data)


run("seq", seq)
run("rand", rand)
run("mmap", mapped)
