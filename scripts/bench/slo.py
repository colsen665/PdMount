"""Checks the daemon's latencies against the DESIGN.md §5.7 SLOs (D057).

Usage: pdmount stats --json | slo.py
       slo.py <stats.json> ...     several rounds: the median of each key's p99 (D086)
Prints one PASS/FAIL line per histogram key (p99 against the limit) and one for
hot_path_network_calls == 0 (in every round). Exit status 1 if any line fails.

A key without samples is MISSING. That fails for the keys the m4.sh `slo` workload always
exercises, and is only reported for the others (readdir and readdirplus: the kernel picks
one; read_ram: the RAM cache may hold none of what is read).
"""
import json
import statistics
import sys

# key -> p99 limit in µs
LIMITS = {
    "getattr": 1000,
    "lookup": 1000,
    "readdir": 1000,
    "readdirplus": 1000,
    "create": 2000,
    "mkdir": 2000,
    "unlink": 2000,
    "rmdir": 2000,
    "rename": 2000,
    "setattr": 2000,
    "write": 1000,
    "read_ram": 1000,
    "read_disk": 5000,
    "fsync": 40000,  # D066, D086
}
OPTIONAL = {"readdir", "readdirplus", "read_ram"}

replies = []
if len(sys.argv) > 1:
    for path in sys.argv[1:]:
        with open(path) as file:
            replies.append(json.load(file))
else:
    replies.append(json.load(sys.stdin))


def rows_of(key):
    """The key's row in each round that has samples of it."""
    rows = []
    for reply in replies:
        for row in reply.get("latency", []):
            if row["name"] == key and row["count"] > 0:
                rows.append(row)
    return rows


failed = 0
for key, limit in LIMITS.items():
    rows = rows_of(key)
    if not rows:
        verdict = "MISSING" if key in OPTIONAL else "FAIL"
        failed += key not in OPTIONAL
        print(f"{verdict:7} {key:12} no samples (p99 < {limit} µs)")
        continue
    p99 = statistics.median(row["p99_us"] for row in rows)
    p50 = statistics.median(row["p50_us"] for row in rows)
    ok = p99 < limit
    failed += not ok
    print(f"{'PASS' if ok else 'FAIL':7} {key:12} p99 {p99:>6.0f} µs < {limit:>5} µs"
          f"  (n {sum(row['count'] for row in rows)}, p50 {p50:.0f},"
          f" max {max(row['max_us'] for row in rows)})")
calls = [reply.get("counters", {}).get("hot_path_network_calls") for reply in replies]
ok = all(count == 0 for count in calls)
calls = sum(count or 0 for count in calls) if all(count is not None for count in calls) else None
failed += not ok
print(f"{'PASS' if ok else 'FAIL':7} hot_path_network_calls = {calls}")
sys.exit(1 if failed else 0)
