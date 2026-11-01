# SDK fork patches

PdMount depends on `proton-drive-sdk` 0.4 and `proton-sdk-rs2` 0.4 through a fork pinned in the workspace `Cargo.toml`:

```toml
[patch.crates-io]
proton-drive-sdk = { git = "file://<local bare repo>", rev = "<pinned sha on pdmount>" }
proton-sdk-rs2   = { git = "file://<local bare repo>", rev = "<pinned sha on pdmount>" }
```

## Fork repository

Decision D022.

| Item | Value |
|---|---|
| Location | `<fork>` (local bare repo) |
| Remote `upstream` | `https://github.com/tirbofish/proton-sdk-rs2` (fetch: `git --git-dir=… fetch upstream`) |
| Base | tag `base/0.4.0` = `409ca119` = upstream `main` = the commit of the published 0.4.0 crates (`.cargo_vcs_info.json`) |
| Integration branch | `pdmount` (bare-repo HEAD). PdMount pins a commit on it with `rev = …` |
| Patch branches | one per patch, created from `base/0.4.0`, named as in the tracking table below; merged into `pdmount` when tested |
| Crates in the repo | `crates/proton-drive-sdk`, `crates/proton-sdk-rs2` (and `crates/pdcli`, unused) |

Working on a patch:

```sh
git clone <fork> <working copy>   # working copy
cd <working copy>
git switch -c fix/block-pagination base/0.4.0
# … edit, test with cargo test -p proton-drive-sdk …
git push origin fix/block-pagination
git switch pdmount && git merge --no-ff fix/block-pagination && git push origin pdmount
```

**Rebasing onto a new upstream release:**
1. `git fetch upstream`.
2. Rebase each unmerged patch branch onto the new upstream tag.
3. Rebuild `pdmount` from the new base plus those branches.
4. Tag the new base as `base/<version>`.

**Open:**
- Upstream submission is deferred (D023, TODO.md Q8). Local CI reads the fork through `file://`.

Rules:

- One patch is one self-contained, upstream-ready branch, rebased on upstream `main`, each with its own tests. Submitting
  upstream is deferred (D023).
- The fork carries only unmerged patches. When upstream merges a patch, the next fork rebase drops it.
- PdMount code depends on the `Backend` trait (DESIGN.md §3), never on fork-only API outside `pdmount-backend/src/proton/`.
  If a patch is rejected upstream, an adapter there replaces it.

Path prefixes: `sdk/` = `proton-drive-sdk-0.4.0/src/`, `rs2/` = `proton-sdk-rs2-0.4.0/src/`, `cs/` = `<official SDK checkout>/`
(the official Proton Drive SDK, used as the behaviour reference).

## Required by milestone

| Milestone | Patches |
|---|---|
| M0/M1 (read-only mount) | P1, P2, P4, P8 (P3 recommended) |
| M2 (writes) | P5, P6, P10, P13 |
| M3 (multi-device, robustness) | P9, P11, P12, P14, P15 |
| M4 (performance) | P7, P18 |

## Patches

### P1 — Paginate the revision block list
- **Where:** `sdk/node/revision.rs:225-249` (`create_download_state`), constants at `sdk/node/download.rs:18-19`
  (`MIN_BLOCK_INDEX = 1`, `DEFAULT_BLOCK_PAGE_SIZE = 10`).
- **Problem:** `get_revision(.., Some(MIN_BLOCK_INDEX), Some(DEFAULT_BLOCK_PAGE_SIZE), ..)` is called once. Only the first
  10 blocks (10 × 4 MiB = 40 MiB) are downloaded, so larger files come back truncated, in both `download_to_stream` and `download_range`.
- **Fix:** keep requesting with `from_block_index = last + 1` until a page has fewer than `page_size` blocks, as
  `cs/client/cs/src/Proton.Drive.Sdk/Nodes/Download/RevisionReader.cs` (`GetBlocksAsync`) and the JS `apiService.ts` do.
- **Test:** a fake API serving 25 blocks; checksum of a downloaded 100 MiB file.

### P2 — Verify block digests and the manifest signature
- **Where:** `sdk/node/download.rs:547-561` (`download_block_at`), `sdk/api/block.rs:33-50` (`BlockDto.hash_digest`).
- **Problem:** the downloaded block's SHA-256 is computed and discarded. `CompletedDownloadManifestVerificationException`
  is defined but never raised.
- **Fix:** compare each block's digest with `BlockDto.hash_digest`, and verify the manifest signature over the concatenated hashes as
  `RevisionReader.cs:341-357` does. Raise typed integrity errors. The manifest check covers the full-download path only;
  block-map reads get it from P17.

### P3 — Public reusable block map
- **Where:** `sdk/node/file/download.rs:173-212` (`download_range` rebuilds the download state on every call).
- **Problem:** FUSE reads need random block access without one or two API round trips per `read()`.
- **Fix:** public `BlockMap { content_key, blocks: Vec<{index, url, token, hash, plain_size}> }` with
  `fetch(revision) -> BlockMap`, `refresh_urls(&mut self)` (on a 404 / expired target, `sdk/error.rs:38-40`) and
  `download_block(&BlockMap, idx) -> Bytes`.

### P4 — Decrypt the active revision XAttr during node decode
- **Where:** `sdk/node/mod.rs:1366-1418`.
- **Problem:**
  - `claimed_size` is set from `storage_quota_consumption` (encrypted storage size), not from the plaintext size.
  - `claimed_modification_time` is set from the revision `creation_time`.
  - `claimed_digests.sha1` and `additional_claimed_metadata` are `None`. The XAttr field on the revision DTO (`sdk/api/revision.rs:33`) is
    available but unused.
  - Every consumer that uses these values for `st_size`/`st_mtime` shows wrong attributes, including pdcli (`pdcli/fs.rs:371-372`).
- **Fix:** decrypt the active revision XAttr with the node key and fill `Common.Size`, `Common.ModificationTime`,
  `Common.Digests.SHA1`, `Common.BlockSizes` and the flattened additional metadata (`sdk/api/attr.rs:53-56`). The same logic
  already exists for the explicit revision path (`sdk/client.rs:788-810`).
- **Acceptance:** PdMount scenario R1 in `fresh`/`second` mode. `rsync -ani` (quick check on size and mtime) prints nothing.

### P5 — Folder modification time
- **Where:** `sdk/node/folder.rs:35` (`_last_modification_time` ignored), `:106` (`extended_attributes: None`);
  DTO at `sdk/api/folder.rs:41`, `:65`.
- **Fix:** encrypt `{Common:{ModificationTime}}` as the folder XAttr on create, and decode the folder XAttr on read.
- **Note:** no folder XAttr update API is known. Directory mtimes that change after creation stay local-only (DESIGN.md §8).

### P6 — Upload returns the new revision id
- **Where:** `sdk/node/file/upload.rs:129-134`, `:185-222`.
- **Problem:** `upload_from_stream` returns only `NodeUid`, although `draft.uid` (the revision) is known.
- **Fix:** return `(NodeUid, RevisionUid)`, or a struct. PdMount needs the revision id for echo suppression and conflict
  detection.

### P7 — Configurable per-client transfer semaphore
- **Where:** `sdk/node/transfer.rs:19` (`file_semaphore: Arc::new(Semaphore::new(1))`).
- **Problem:** one whole-file transfer at a time per client, in each direction.
- **Fix:** add a client option `max_concurrent_files` (default 1 for compatibility). PdMount uses 4 uploads plus its own read
  scheduler.

### P8 — Typed API errors
- **Where:** `sdk/api/file.rs:163-185` and sibling API modules, `sdk/api/mod.rs:226-236`.
- **Problem:** errors become `anyhow` strings (`"API error {code}: {msg}"`, `"HTTP {status}: {body}"`).
- **Fix:** `ApiError { http_status, code: Option<i64>, message, retry_after: Option<Duration> }` that can be
  downcast from `anyhow::Error`. Codes are listed in `cs/client/js/src/internal/apiService/errorCodes.ts:17-41`.

### P9 — Surface token refresh failures; configurable base URL
- **Where:** `rs2/auth.rs:194-197` (`Err(_) => (current_access, current_refresh)`), `rs2/auth.rs:105`/`:138`
  (`subscribe_refresh_token_expired` never fires), `rs2/session.rs:511` (hard-coded `https://drive-api.proton.me`).
- **Fix:** return the refresh error and fire `refresh_token_expired` on invalid-grant responses. Use the configured base URL.

### P10 — Public move with rename
- **Where:** `sdk/client.rs:1016-1017` (`move_nodes` passes `None` for the new name); implementation at
  `sdk/node/operations.rs:342` (`move_single(.., Option<name>)`).
- **Fix:** `ProtonDriveClient::move_node(uid, new_parent, new_name: Option<String>)`, so a cross-directory `rename(2)` is
  one remote call.

### P11 — Typed revision-conflict error
- **Where:** `sdk/node/draft.rs:175-190` (new-revision draft sends `CurrentRevisionID`).
- **Fix:** map the server's rejection of a stale `CurrentRevisionID` to `RevisionConflict { current_revision }`. The same is needed
  for rename/move rejections on `OriginalHash` (`sdk/api/links.rs:562`, `:638`).
- **Open question (verify in M3):** which server error codes are returned, and whether these preconditions are enforced at draft creation, at commit, or both.

### P12 — Retry for idempotent metadata requests
- **Problem:** only block transfers retry (`sdk/node/download.rs:154-175`, `sdk/api/storage.rs:9`). Metadata GETs fail on the
  first 429 or 5xx.
- **Fix:** bounded retry with exponential backoff and jitter on 408/425/429/5xx for GET requests, honouring `Retry-After`.

### P13 — Batched fresh node fetch
- **Where:** `sdk/client.rs` (`get_node_uncached`, `enumerate_nodes`), `sdk/node/mod.rs` (`DtoToMetadataConverter`,
  `NodeBatchLoader`), `sdk/node/operations.rs` (`enumerate_nodes`).
- **Problem:** the only way to read a node past the entity cache, which revision commits and other devices' changes leave
  stale (DESIGN.md F14), is `get_node_uncached`: one `get_details` request per node. An event-following client refetches
  every node an event names, so it sends one request per node where the official SDKs send one per batch (C#: 50 links,
  `cs/client/cs/src/Proton.Drive.Sdk/BatchLoaderBase.cs:8`; JS: up to 100, `cs/client/js/src/internal/nodes/apiService.ts:32`).
  `enumerate_nodes` answers from the cache and runs 8 single-node requests at once; `NodeBatchLoader` needs the parent
  folder key up front and leaves stale cache entries.
- **Fix:** `ProtonDriveClient::get_nodes_uncached(volume_id, link_ids)`: evicts the links' cache entries, sends one
  `get_details` per 50 links (`FRESH_NODES_BATCH_SIZE`), one batch after the other, resolves parent keys as
  `get_fresh_node_metadata` does, and caches the decoded nodes and secrets. Missing links (the API omits them,
  `apiService.ts:217-221`) are missing from the result.
- **Test:** the batching helper sends 50, 50 and 20 links for 120 ids, nothing for none, and stops at the first failure.

### P14 — Conditional rename and move with a caller-supplied name hash
- **Where:** `sdk/node/operations.rs` (`rename`, `move_single`), `sdk/client.rs` (`rename_node`, `move_node`,
  `get_nodes_uncached`), `sdk/node/mod.rs` (`NodeMetadataResult`). Decision D038.
- **Problem:** rename and move send the node's name hash as `OriginalHash`, which the server compares with the stored
  hash (F11). The SDK fetches that hash fresh right before the request, so the check never fails: a rename by another
  client after the caller last looked is silently overwritten. Decoded nodes drop the hash, so callers cannot supply it.
- **Fix:** `rename_node_with_original_hash(uid, name, media_type, hash)` and
  `move_node_with_original_hash(uid, parent, name, hash)` send the caller's hash. `get_nodes_uncached_with_name_hashes`
  returns each node with its name hash; `get_nodes_uncached` is built on it. `rename_node`, `move_node` and `move_nodes`
  keep their behaviour. Folder listings carry no hash.
- **Test:** signature tests for the two calls; an empty fetch sends nothing. Checked against the live API in M3.1: a
  stale hash is refused for rename and move (code 2000), the fresh one is accepted.

### P15 — Trash time of v2 link details
- **Where:** `sdk/api/links.rs` (`LinkDto.trash_time`). Decision D044.
- **Problem:** link details come from `POST v2/volumes/{id}/links`, whose `LinkDto` names the trash timestamp
  `TrashTime` (`cs/client/js/src/internal/apiService/driveTypes.ts`, `LinkDto`; `apiService.ts` reads
  `link.Link.TrashTime`). The DTO reads `Trashed`, the v1 name, so `trash_time` is always `None`: `get_node`,
  `get_nodes_uncached` and the trash listing report trashed nodes as active (F16).
- **Fix:** `#[serde(rename = "TrashTime", alias = "Trashed")]`.
- **Test:** a DTO with `TrashTime`, with `Trashed`, with `null` and without the field. Checked against the live API in
  M3.1: a trashed file and folder report a trash time at once.

### P16 — No secrets or response bodies in logs
- **Where:** `rs2/account.rs:226`, `rs2/api/cache.rs:249`, `rs2/users.rs:83`, `rs2/secret.rs:63,67`, `rs2/session.rs`
  (`ensure_authenticated`); `sdk/api/share.rs` (`get_my_files_share`, `get_share`), `sdk/api/file.rs`
  (`get_thumbnail_blocks`), `sdk/api/links.rs` (`get_link_details`, `move_link`, `move_multiple`, `copy_link`),
  `sdk/api/trash.rs` (`get_trash`, `trash_multiple`). Security review finding S-1 (High), M4.0.2.1.
- **Problem:** debug logs print the user key passphrase, the cached address private keys (at every session resume), the
  serialized cached passphrase, the whole `/users` body and the session probe body. The drive SDK logs the raw share
  responses (encrypted share keys and passphrases), the thumbnail block list (download URLs and tokens), link move request
  bodies (encrypted node passphrases), and the move, copy, link-details and trash responses. The trash, share and
  thumbnail-block decode errors carry the whole body too.
- **Fix:** the logs are removed (the move requests still log their URL); the decode errors keep the serde error only.
- **Left as is:** the generic decode helpers (`sdk/api/mod.rs:249,284,298,307`), `storage.rs:139`, `docs.rs:238` and the
  shared-by-me/with-me and album decoders still put the body into the error of a malformed reply (error text, not a log
  call); PdMount logs such errors at WARN. TODO Known gaps ("Security (M4.0.1)").
- **Behaviour change:** none.
- **Test:** none in the fork (log output only); PdMount's `sdk_debug_records_never_reach_the_log` covers the filter that
  backs it up.

### P17 — Manifest verification on block-map reads
- **Where:** `sdk/node/file/block_map.rs` (`get_block_map`, new `verify_block_listing`), `sdk/node/download.rs`
  (`manifest_verification_keys`, shared with `RevisionReader::verify_manifest`), `rs2/api/response.rs`
  (`AddressPublicKeyListResponse`), `rs2/account.rs` (`get_address_public_keys`). Security review findings S-4 (Medium)
  and S-11 (Low), M4.0.2.2.
- **Problem:** `download_block` checks each block against its listed hash, but nothing checks the listed hashes: the
  manifest signature is verified only by full downloads (P2). A storage or server that lists other blocks (another revision
  of the same file, say) passes every check. A block listed with an empty hash skips the per-block check
  (`sdk/block/download.rs` `verify_block_hash`).
- **Fix:** `get_block_map` builds the manifest from the listed thumbnail hashes (by type) and block hashes (by index), as
  the full download and the JS SDK (`fileDownloader.ts`, which also uses the listed hashes) do, and verifies
  `ManifestSignature` with the node key when `SignatureEmail` is empty, else the address keys of that email. Failure:
  `CompletedDownloadManifestVerificationException`; an empty block hash: `DataIntegrityException`. PdMount maps both to
  `Integrity` (EIO) and does not cache the map.
- **Second fix, found by the real-account check:** `get_address_public_keys` never worked. `AddressPublicKeyListResponse`
  expected `Address` as a string and a top-level `AddressPublicKeys`; the server answers `Address: {Keys: [{PublicKey,
  Primary, Flags, Source}], SignedKeyList}` (`get_core-{_version}-keys-all` in `cs/client/js/src/internal/apiService/coreTypes.ts`),
  so every call failed to decode ("invalid type: map, expected a string at line 1 column 23"). With the first commit
  alone, 989 of the dataset's files read as EIO. Now `Address`, `CatchAll` and `Unverified` decode as key lists, and
  verification uses the `Address` keys not flagged compromised (`Flags` bit 1); `Unverified` keys are documented as not
  for verification. Before, node authorship checks (`node/authorship.rs`) never got their keys either, and each failed
  lookup was an uncached `keys/all` call; a successful lookup is cached for its TTL.
- **Test:** `verify_block_listing` with constructed listings: a signed listing passes; a tampered hash, a dropped thumbnail
  or block, another key's signature, a missing signature and an empty hash fail. `all_keys_answer_decodes_the_address_keys`
  decodes the schema's answer. Real account: see the M4.0.2.2 entry in `docs/handoff-log.md`.
- **Behaviour change:** a block map whose listing has a missing or invalid manifest signature, or a block without a hash,
  now fails, so every read of that revision is EIO; before, such reads worked.

### P18 — Configurable block transfer parallelism
- **Where:** `sdk/client.rs` (`ProtonDriveClient::new` → `from_components_with_api_url`, which fixes
  `DEFAULT_DEGREE_OF_BLOCK_TRANSFER_PARALLELISM = 6` for the block uploader and downloader), `sdk/node/transfer.rs`
  (`TransferQueue`). M4.7, D058.
- **Problem:** with P7's file limit of 4, uploads could have 6 blocks in flight, equal to the C# SDK's limit rather than
  below it as D033 asks; only the lower-level `from_components` constructor takes another value.
- **Fix:** `ProtonDriveClient::with_block_transfer_parallelism(n)`, next to `with_max_concurrent_file_transfers`, replaces
  the block semaphore of the upload and download queues (`TransferQueue::with_max_degree_of_parallelism`, which keeps
  the file semaphore, so the two builders work in either order). Thumbnail downloads and the block-processing
  semaphores derived from the constructor's value are unchanged.
- **Test:** `changing_the_block_parallelism_keeps_the_file_capacity` (queue), and
  `block_transfer_parallelism_applies_to_uploads_and_downloads` (client: 4 files and 5 blocks per direction, the sixth
  block and fifth file wait). PdMount cannot build a client without a session, so this fork test covers its builder.
- **Behaviour change:** none unless called.

## Upstream tracking

| Patch | Fork branch | Commit | Based on | Upstream | State |
|---|---|---|---|---|---|
| P1 | `fix/block-pagination` | `312d9d0` | `base/0.4.0` | deferred (D023) | merged into `pdmount` |
| P2 | `fix/block-verification` | `f8513ca` | `base/0.4.0` | deferred (D023) | merged into `pdmount` |
| P3 | `feat/block-map` | `9ef1094` | P1 + P2 merged | deferred (D023) | merged into `pdmount` |
| P4 | `fix/revision-xattr-decode` | `2aa1e1f` | `base/0.4.0` | deferred (D023) | merged into `pdmount` |
| P5 | `feat/folder-mtime` | `4036915` | `base/0.4.0` | deferred (D023) | merged into `pdmount` |
| P6 | `feat/upload-revision-uid` | `3a6186a` | `base/0.4.0` | deferred (D023) | merged into `pdmount` |
| P7 | `feat/transfer-concurrency` | `e157e1d` | `base/0.4.0` | deferred (D023) | merged into `pdmount` |
| P8 | `feat/typed-api-error` | `28d124f` | `base/0.4.0` | deferred (D023) | merged into `pdmount` |
| P9 | `fix/token-refresh-errors` | `6b794ac` | `base/0.4.0` | deferred (D023) | merged into `pdmount` |
| P10 | `feat/move-with-rename` | `c7bf10b` | `base/0.4.0` | deferred (D023) | merged into `pdmount` |
| P11 | `feat/revision-conflict-error` | `1e777f3` | P8 | deferred (D023) | merged into `pdmount` |
| P12 | `feat/metadata-retry` | `59fca73` | P8 | deferred (D023) | merged into `pdmount` |
| P13 | `feat/fresh-node-batch` | `7b4dcf9` | `base/0.4.0` | deferred (D023) | merged into `pdmount` |
| P14 | `feat/caller-name-hash` | `2137adc` | `pdmount` at `4548aa7` (needs P10, P13) | deferred (D023) | merged into `pdmount` |
| P15 | `fix/link-trash-time` | `30c061a` | `base/0.4.0` | deferred (D023) | merged into `pdmount` |
| P16 | `fix/no-secret-logs` | `ed2a9a6`, `1de736a` (rustfmt) | `pdmount` at `d1c8994` | deferred (D023) | merged into `pdmount` |
| P17 | `feat/block-map-manifest` | `d6e26ba`, `b0da02f` (keys decoding) | `pdmount` at `0c3d32c` (after P16) | deferred (D023) | merged into `pdmount` |
| P18 | `feat/block-parallelism` | `cc50981` | `pdmount` at `28cacff` (needs P7) | deferred (D023) | merged into `pdmount` |

Integration: `pdmount` = `base/0.4.0` + all branches above, merged with `--no-ff`. It also contains `9176b0f`, a test-only fix
setting the new `FolderNode` field (from P5) in a test helper added by P10. Checks on 2026-10-19: 236 SDK tests pass (baseline
150), pdcli builds and its tests pass, and `cargo fmt --check` passes. The fork's own tests are all offline. P13 was merged on 2026-10-21 (`4548aa7`, pinned by PdMount); the
fork's workspace tests (278) pass on it. Through PdMount, the patches ran against the live API in the M2.8 real-account matrix
(60 of 60 runs passed, 2026-10-22). P14 (`e3c576a`) and P15 (`d1c8994`, pinned by PdMount) were merged on 2026-10-22;
the fork's workspace tests (281) pass, and both ran against the live API in the M3.1 probe.
P16 was merged on 2026-10-27 (`e931883`; its rustfmt follow-up `0c3d32c`); the fork's workspace tests (281) pass. P17 was
merged on 2026-10-27 (`8181251`, then `28cacff` with the keys-decoding fix); the fork's workspace tests (288) pass.
P18 was merged on 2026-10-28 (`2ae4aa3`, pinned by PdMount); the fork's workspace tests (290) pass.

## Behaviour changes

These changes differ from 0.4.0 and can affect existing callers such as pdcli. Approved by the user on 2026-10-21 (P2,
P8, P9, P11, P12; P13 only adds an API and changes no existing behaviour) and on 2026-10-22 (P15; P14 only adds API):

| Patch | Change |
|---|---|
| P2 | A full download (`download_to_stream`) of a revision whose manifest signature is missing or cannot be verified now fails with `CompletedDownloadManifestVerificationException` after the content has been written, as in the C# SDK. A block hash mismatch fails with `DataIntegrityException` and is not retried; the JS SDK retries these. The uploader now orders thumbnail digests by type in the signed manifest, as C# does. |
| P8 | Errors keep the `"API error {code}: {message}"` / `"HTTP {status}: …"` wording. `ApiResponse::to_result` changed from `"API error: code X, message: Some(..)"` to `"API error X: …"`. Endpoints that ignored the HTTP status now return `ApiError` instead of a JSON parse error. |
| P9 | A failed token refresh returns `Err(TokenRefreshError)` instead of `Ok(old tokens)`. `refresh_token_expired` fires on Proton code 10013. `tokens_refreshed` no longer fires after a failure. |
| P11 | New-file drafts send `ClientUID`. With `override_existing_draft_by_other_client = true`, another client's draft that never got a committed revision is deleted and the create retried. Failures are `NodeWithSameNameExistsError` / `RevisionDraftConflictError`. |
| P12 | Drive API requests are retried: 429 for all methods; 5xx and timeouts for idempotent methods only; connection failures. Circuit breakers fail fast for 60 s after 50 consecutive 429s or 10 consecutive 5xx. |
| P17 | `get_block_map` fails for a revision whose manifest signature is missing or does not verify over the listed hashes, or that lists a block without a hash (2026-10-27, accepted with the M4.0.1 fix plan). `get_address_public_keys` now returns the address's non-compromised keys instead of failing, so authorship and manifest checks of address-signed nodes run for the first time. |
| P15 | Trashed nodes report their trash time (`trash_time`) in `get_node`, `get_nodes_uncached`, folder listings and the trash listing; before, it was always `None`. |

## Known gaps after M0.2

- **P3:** `download_range` builds a `BlockMap` on every call. PdMount keeps its own map per revision instead (DESIGN.md §6.1).
- **P4:** `Media` is not copied into `additional_claimed_metadata`. Anonymous revisions are not signature-checked. Legacy block-size ordering (JS sorts revisions from before 2025) is not applied.
- **P9:** `DefaultAuthenticationApiClient` still hard-codes `https://drive-api.proton.me/` for the auth endpoints. The Drive API clients still do not refresh on a 401; the PdMount backend adapter does that (DESIGN.md §12.3).
- **P10:** no name validation on rename or move (the C# SDK has `ValidateNodeName`). PdMount validates names itself (DESIGN.md §8.8).
- The legacy, unused `node::download::BlockDownloader` has no hash check.
