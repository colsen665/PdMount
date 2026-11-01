## Area 5 (SDK fork patches P1–P15, repo scripts): findings

Fork files: `pdmount:crates/proton-drive-sdk/src/...` (fork `<fork>`, branch `pdmount` = c837e03). Repo files relative to the worktree.

### A5-1: BlockMap reads trust unauthenticated block hashes (manifest not verified for random access)
- **Rating**: Medium
- **Location**: fork `node/file/block_map.rs:304-341` (`get_block_map`), `:350-380` (`download_block`); `node/download.rs:711-717`; `node/file/download.rs:163-173`. PdMount uses this for every read (`crates/pdmount-backend/src/proton.rs:513-560`, `download_verified` with `entry.hash_digest`).
- **Actor**: T3/storage side
- **Attack**: the per-block check compares the blob's SHA-256 with the hash served with the block list; the manifest signature that authenticates the hash list is verified only by `download_to_stream` after a full download. Swapping listing entry + blob (e.g. block 3 as block 2; same content key) passes.
- **Impact**: silent content substitution/reordering on random-access reads (A4).
- **Fix**: in `get_block_map`, build the manifest from the listed hashes and verify `revision.manifest_signature` with the keys `verify_manifest` uses; fail (→ `BackendError::Integrity`) if it does not verify; reject empty hashes.
- **Confidence**: high on the gap; medium on exploitability.

### A5-2: `Retry-After` used unclamped; 429 retries have no per-request attempt cap
- **Rating**: Low
- **Location**: fork `error.rs:191-194` (`parse_retry_after`), `http/retry.rs:115-122`, `:234-263` (only exit: breaker at 50 consecutive 429s), `block/download.rs:150-163`, `:191-193`.
- **Actor**: T3
- **Attack**: `Retry-After: 86400` (or u64::MAX) parks a request; middleware 429 path unbounded per request.
- **Impact**: slow-path ops hang for attacker-chosen time; only daemon deadlines recover.
- **Fix**: clamp (e.g. 300 s) in `parse_retry_after`; per-request 429 attempt limit (e.g. 5).
- **Confidence**: high on behaviour, medium on exploitability.

### A5-3: Empty listed hash silently disables the per-block check
- **Rating**: Low
- **Location**: fork `block/download.rs:100-110` (`verify_block_hash`: empty → Ok); `api/block.rs:37-40` (`hash_digest` `#[serde(default)]`).
- **Attack**: listing omits `Hash` → `download_verified` accepts any blob; BlockMap reads have no manifest check (A5-1).
- **Fix**: `BlockMap::from_parts`/`download_block` fail with `DataIntegrityException` on an empty hash.
- **Confidence**: high

### A5-4: Idempotent 5xx retry can replay an applied rename/move and surface a false conflict
- **Rating**: Low
- **Location**: fork `http/retry.rs:93-103` (PUT/DELETE idempotent), `:123-125`; P14 `node/operations.rs` rename/move with `OriginalHash`.
- **Attack/accident**: first PUT applied, response lost as 5xx, retry with stale `OriginalHash` → code 2000 → PdMount treats its own rename as a concurrent one.
- **Impact**: spurious rename_rename handling.
- **Fix**: on 2000 after a retry, refetch and treat "name already as requested" as success; or don't 5xx-retry hash-conditional PUTs.
- **Confidence**: low-medium

### A5-5: `rm -rf "$DATASET"` deletes any directory the user points DATASET at
- **Rating**: Medium (accident)
- **Location**: `tests/scenarios/lib.sh:144-151` (`ensure_dataset`); same pattern `lib.sh:159-164` (`FAKE_DIR` unless `.seeded-m1`), `scripts/bench/latency.sh:25-27`, `scripts/bench/read.sh:83-88`.
- **Attack**: `DATASET=<dir> tests/scenarios/r6.sh` deletes `<dir>`.
- **Fix**: delete only owned subpaths (`$DATASET/pdmount-m1`); `FAKE_DIR` only when empty or with the fake's layout; otherwise refuse.
- **Confidence**: high

### A5-6: Predictable shared-tmpfs root `/dev/shm/pdmount-*` with no ownership check
- **Rating**: Medium (T1)
- **Location**: `tests/scenarios/lib.sh:49-56`, `:78-80`, `:340-343`; `tests/soak.sh:34,54`; `scripts/bench/latency.sh:19-20`, `scripts/bench/read.sh:77-78`. Real-account scenarios also use it for `$work`.
- **Attack**: another user pre-creates `/dev/shm/pdmount-tests`; `mkdir -p` accepts it; they swap `work.XXXXXX`; `g2.sh:9-14`/`g6.sh:11-15` run git in planted repos (hooks/fsmonitor → code exec as developer); `r*.sh` upload attacker content to the real account.
- **Impact**: code execution as the developer; unintended uploads.
- **Fix**: `mkdir -p -m 0700`, verify `-O` and not a symlink, or per-uid name `/dev/shm/pdmount-$UID`; real-account `$work` not on shared tmpfs.
- **Confidence**: medium-high

### A5-7: `$dir` interpolated into `sh -c` strings
- **Rating**: Info
- **Location**: `tests/scenarios/a1.sh:26`, `a2.sh:14-15,21`, `a3.sh:19,22`.
- **Fix**: `sh -c 'printf more >>"$1"' sh "$dir/file"`.

## Checked and fine (area 5)
- P2 full-download verification sound (`node/download.rs:148-181`, `:717-760`); integrity errors never retried; decrypt after hash check.
- P1 pagination contiguity enforced, bounded loop.
- P3 block math: u64 saturating, claimed sizes validated, plaintext length == claimed; BlockMap Debug redacts the key.
- P11 draft cleanup only own client uid; PdMount passes `override_existing_draft_by_other_client=false` (`proton.rs:681-690`).
- P9 auth: no tokens in Display/logs; no mutex across await.
- P8 error bodies bounded to 512 bytes; no headers/tokens; storage client has no logging middleware.
- P12 breaker counters per instance; P4/P5 xattr decode as official SDKs; P13/P14/P15 fine.
- Scripts: real-account writes confined to `pdmount-test`; fake-only commands; no `eval`; other `rm -rf` targets script-owned; `set -euo pipefail`; no secrets in scripts; Python with `-I`.
- `ci.sh:379-380` `rm -rf "$XDG_RUNTIME_DIR"` depends on lib.sh having set it under SCENARIO_ROOT (hardening: use the explicit path).
