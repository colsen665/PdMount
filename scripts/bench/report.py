"""Prints p50/p99/max in µs per operation: report.py <label> <out.json>..."""
import json
import sys

label, files = sys.argv[1], sys.argv[2:]
lat = {}
for name in files:
    with open(name) as f:
        for op, values in json.load(f).items():
            lat.setdefault(op, []).extend(values)


def pct(values, p):
    values = sorted(values)
    return values[min(len(values) - 1, int(len(values) * p))] / 1000


print(f"{label:20} " + "  ".join(
    f"{op}:{pct(v, .5):.0f}/{pct(v, .99):.0f}/{max(v) / 1000:.0f}" for op, v in lat.items()))
