## Area 4 (backend crate, build configuration): findings

Paths relative to the worktree unless prefixed `sdk/` (= `~/.cargo/git/checkouts/proton-sdk-rs2-<hash>/c837e03/crates/proton-drive-sdk/src/`) or `rs2/` (`.../crates/proton-sdk-rs2/src/`).

### A4-1: Uploader-claimed block `plain_size` passed through unbounded, drives a per-block heap allocation
- **Rating**: Medium
- **Location**: `crates/pdmount-backend/src/proton.rs:638-648` (`blocks()` copies `entry.plain_size`); `crates/pdmount-core/src/cache.rs:119-135` (`Layout::new`), `cache.rs:933-935` + `crates/pdmount-core/src/pool.rs:284-286` (`Buffer::heap(len)`); origin `sdk/node/file/block_map.rs:79-106` (BlockSizes xattr; only checks non-negative i32, one per block).
- **Actor**: T2
- **Attack**: one-block file with encrypted xattr `BlockSizes: [2147483647]`; first read asks a 2 GiB buffer (no pool class fits → `vec![0; 2 GiB]`); readahead repeats; `size` advertises multi-GiB; only then `proton.rs:543-551` rejects with Integrity.
- **Impact**: daemon abort on allocation failure or repeated multi-GiB allocations (calloc mitigates residency).
- **Fix**: reject `plain_size > MAX_BLOCK_BYTES` in `block_map()`/`blocks()` with `Integrity` before caching; mirror in `Layout::new`; optionally cap `RemoteNode.size` in `convert_node`.
- **Confidence**: medium

### A4-2: Manifest signature never verified on the block-map read path, contrary to DESIGN §6.3
- **Rating**: Medium (same gap as A5-1)
- **Location**: `crates/pdmount-backend/src/proton.rs:186-211`; `sdk/node/file/block_map.rs:40-47`, `:304-340`; `verify_manifest_signature` only in `FileDownloader` (`sdk/node/download.rs:148`); DESIGN.md:500 and docs/sdk-patches.md:75-80 claim it.
- **Attack**: content key is per node, so blocks of an older revision listed under the active revision decrypt and match their listed hashes; only the manifest would catch rollback/mixing.
- **Fix**: verify the manifest once in `block_map()` (or in the fork's `get_block_map`), fail with `Integrity`, don't cache; or correct DESIGN/P2.
- **Confidence**: high (absent); medium (relevance)

### A4-3: Server `Retry-After` honoured without an upper bound by the event poller
- **Rating**: Low
- **Location**: `sdk/error.rs:191-194`; `proton.rs:358-361`, `:405-407`, `:444-447`; `crates/pdmount-core/src/events.rs:96-105` (`delay.max(*after)`), `:131`. Journal clamps (`journal.rs:434`, 300 s).
- **Attack**: one 429 with huge Retry-After on `poll_volume_events` stops event sync for the mount's life.
- **Fix**: clamp `retry_after` in the adapter's error mapping (e.g. 3600 s) or in `events::backoff`.
- **Confidence**: high

### A4-4: Remote `pdmount.mode` may carry setuid/setgid/sticky bits
- **Rating**: Low
- **Location**: `proton.rs:223-227` (accepts ≤ 0o7777; test `:887` accepts "4755"); core masks only 0o7777 (`vfs.rs:73,99`, `engine.rs:926-927`).
- **Attack**: mount is `nosuid` (`pdmount-fuse/src/lib.rs:964`), but `rsync -a`/`cp -a` copy the bits to local disk.
- **Fix**: mask 0o6000 in `parse_mode` (and on upload) unless round-trip is wanted.
- **Confidence**: high

### A4-5: Empty listed block hash disables per-block verification
- **Rating**: Low (same as A5-3)
- **Location**: `proton.rs:531-538`; `sdk/block/download.rs:103-116`.
- **Fix**: refuse a map with an empty `hash_digest` as `Integrity`.
- **Confidence**: medium

### A4-6: Daemon log world-readable while the backend logs decrypted names into it
- **Rating**: Medium (duplicate of A1-3)
- **Location**: `crates/pdmount/src/daemon.rs:149-157`; `proton.rs:303-314` logs `?degraded` (node uid, name, author email, errors; no key material).
- **Fix**: log 0600, state dir 0700.
- **Confidence**: high

### A4-7: Fake backend compiled into every build; reachability rests on a runtime `cfg!()` check
- **Rating**: Info
- **Location**: `crates/pdmount-backend/src/lib.rs:8` (`pub mod fake;` unconditional); `crates/pdmount/src/daemon.rs:17`, `:398-413` (Fake arm not cfg-gated), `:60-82` (`BackendChoice::parse` refuses `fake:` via `cfg!`); `main.rs` gates the `fake` subcommand. `ci.sh:62` release build cannot unify the feature in (verified with `cargo tree -e features`).
- **Fix**: `fake` feature in `pdmount-backend`, enabled by `pdmount/fake-backend` and dev-deps; cfg-gate the variant and arm.
- **Confidence**: high

### A4-8: Empty name-hash digest becomes an empty rename precondition
- **Rating**: Info
- **Location**: `proton.rs:625-635`, `:293-300` (`hash_from_hex("")` → `Some(vec![])`, test `:1007`), `:763-768`.
- **Fix**: map an empty digest to `None`.
- **Confidence**: medium

## Checked and fine (area 4)
- TLS: reqwest 0.13.5 rustls with platform verifier; only `danger_accept_invalid_certs` is behind `NoCertificateValidation`, default `Strict`, PdMount uses defaults (`session.rs:52-56`).
- Block hash checked before decrypt; plain-size check after (`proton.rs:543-551`); `copy_block` bounds; `MAX_BLOCK_BYTES`; core rejects `len != expected`, never caches failed blocks (`cache.rs:936-955`).
- Integer conversions via `try_from`; journal clamps backoff.
- 401 → one refresh-and-retry; 429 → RateLimited from all paths.
- No secrets logged by this crate; BlockMap Debug redacts key; `DegradedNode` holds no key material.
- Remote names validated in core `representable()` (`vfs.rs:573-579`).
- Unknown event types dropped with a warning.
- Block-map cache bounded; URL TTL 600 s.
- `xdg-open` argv only; honest app version and client UID; SDK caches 0600; `proton-root.json` atomic in 0700 dir.
- Fork pinned to c837e03, `--locked`; `unsafe_code = deny`; `panic = unwind`.
