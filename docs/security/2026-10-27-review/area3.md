## Area 3 (local storage and the data path, `crates/pdmount-core/src/`): findings

### A3-1: A remote file's claimed size drives staging `set_len` and an unbounded `missing_blocks()` allocation
- **Rating**: Medium
- **Location**: `crates/pdmount-core/src/engine.rs:1815-1841` (`stage`: `Base { rev_size: node.size, size: node.size }`), `crates/pdmount-core/src/staging.rs:123-125` (`set_len(size)`), `staging.rs:312-319` (`missing_blocks` collects `0..base.size.div_ceil(GRID)` into a Vec), called from `journal.rs:1274`; size source `crates/pdmount-backend/src/proton.rs:264-273` (`claimed_size` from the uploader XAttr, no cross-check against the block list).
- **Actor**: T2
- **Attack**: small file with XAttr size 2^62; user appends (`echo x >> f`); `stage()` `set_len(2^62)` (sparse ok on btrfs); journal `snapshot()` → `missing_blocks()` builds a 2^40-element Vec → OOM/abort; `materialize` would try base.size/4 MiB block reads.
- **Impact**: remote-triggered daemon crash; mount ENOTCONN.
- **Fix**: validate claimed size against the block list at listing time (refuse/clamp); `missing_blocks` as a bounded iterator; bound `Base.size` before `set_len`.
- **Confidence**: medium-high

### A3-2: Declared per-block `plain_size` used unchecked as the allocation size for a block download
- **Rating**: Medium (duplicate of A4-1)
- **Location**: `cache.rs:116-137` (`Layout::new`), `cache.rs:933-935`, `cache.rs:789-811` (`class_for` None > 4 MiB → `Buffer::heap(len)`, `pool.rs:284-286`); fork `block_map.rs:76-106`.
- **Fix**: `Layout::new` rejects `plain_size > BLOCK_SIZE` (Integrity); `BlockCache::get` refuses `expected_len > MAX_BLOCK_BYTES` before allocating.
- **Confidence**: medium

### A3-3: Daemon log world-readable with remote names and local paths
- **Rating**: Medium (duplicate of A1-3/A4-6)
- **Location**: `crates/pdmount/src/daemon.rs:149-156`; `paths.rs:195-197`; contents `vfs.rs:1167,1171`, `journal.rs:963`, `:966-968`, `:986-989`, `cache.rs:413,417`, panic messages.
- **Fix**: state dir 0700, log 0600, chmod existing.
- **Confidence**: high

### A3-4: Config durations near `u64::MAX` panic in the journal and event tasks
- **Rating**: Low (T5)
- **Location**: `events.rs:98` (`interval * step`), `journal.rs:713,716` (`changed_at + quiet`); `units.rs:39-49`; `config.rs:291-318` lower bounds only.
- **Attack**: `poll_interval = "18446744073709551615s"` panics the event task; `debounce` panics `run_journal`; detached tasks die silently.
- **Fix**: upper bounds in `Config::validate` (e.g. 24 h) or checked/saturating arithmetic.
- **Confidence**: high

### A3-5: Remote names interpolated into log lines and parked reasons unescaped
- **Rating**: Low (overlaps A2-5)
- **Location**: `journal.rs:963` (`name = fresh.name` Display), `journal.rs:966-968`, `:986-989` (`format!` into `UploadError::Park`, shown by `status`, logged at `:1240`).
- **Fix**: `?` Debug escaping; escape names in parked reasons.
- **Confidence**: high

### A3-6: `BlockKey::encode` not injective if ids contain NUL
- **Rating**: Info
- **Location**: `cache.rs:64-73`.
- **Fix**: length-prefix components.
- **Confidence**: low (no realistic input)

### A3-7: `state.db`, `device-id` and SDK caches rely on the directory mode only
- **Rating**: Info (overlaps A1-6)
- **Location**: `store/mod.rs:107-110`, `device.rs:31-39`; data dir 0700 only via `create_private_dir` (`session.rs:42`, `credentials.rs:493-497`).
- **Fix**: `Store::open` creates the parent 0700 and sets 0600 on `sqlite_files(path)` (`paths.rs:109-115`); same in `device.rs`.

## Checked and fine (area 3)
- Cache crypto: derive_key, fresh 24-byte OsRng nonce per seal (`cache.rs:458`), AAD binds link/rev/index, reopen after seal.
- Cache paths only from blake3 under `blocks_dir`; eviction deletes only `self.path(key)`; dirs 0700, temp `create_new` 0600 + rename.
- AEAD failure → remove, count, miss; sealed size bounded before read.
- Download integrity chain (`download_verified`, plain-size, `copy_block`, `len != expected`).
- Pool: no stale bytes exposed (`pool.rs:297-303`, `cache.rs:276-278`, `:196-201`, heap fallback zeroed).
- Staging: 0700/0600, paths from inos only, sidecar parse errors no panic, short base reads rejected, snapshot private copy, write-then-sidecar ordering.
- Upload path: snapshot/base_rev/generations under the staging lock; SHA-1 from snapshot; clean only if `content_gen` unchanged; `in_flight` serialization; `.pdignore` nodes never planned; 1 MiB cap on `/.pdignore`.
- Store: parameterized SQL; newer schemas refused.
- Paths: profile names restricted; no `/tmp` in non-test code; socket fallback in 0700 data dir.
- Config: `deny_unknown_fields`; `~/` only; units overflow-checked.
- Device id `create_new`; events cursor ordering; batches of 100.
