"""One bare fdatasync's p99 on the disk of a directory, in ms (D086).

Usage: fdatasync.py <dir>
400 appends of 4 KiB, each followed by fdatasync, in a temporary file there. Takes about a
second on a quiet SATA SSD. m4.sh uses it to tell a slow disk from slow code.
"""
import os
import sys
import tempfile
import time

fd, path = tempfile.mkstemp(dir=sys.argv[1])
try:
    block = os.urandom(4096)
    times = []
    for _ in range(400):
        os.write(fd, block)
        start = time.perf_counter()
        os.fdatasync(fd)
        times.append(time.perf_counter() - start)
finally:
    os.close(fd)
    os.unlink(path)
times.sort()
print(f"{times[395] * 1000:.1f}")
