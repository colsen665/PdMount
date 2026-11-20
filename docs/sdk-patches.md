# SDK fork patches

PdMount depends on `proton-drive-sdk` 0.5 and `proton-sdk-rs2` 0.5 (0.4 until M6.0.1) through a fork pinned in the workspace
`Cargo.toml`:

```toml
[patch.crates-io]
proton-drive-sdk = { git = "https://github.com/colsen665/proton-sdk-rs2", rev = "<pinned sha on pdmount-0.5>" }
proton-sdk-rs2   = { git = "https://github.com/colsen665/proton-sdk-rs2", rev = "<pinned sha on pdmount-0.5>" }
```

**State (M6.0.4, D099):** the patches are rebased onto upstream 0.5.1, with P19 added on top (M6.0.1, pin `70f10ff`),
and P16 and P13 amended for M6.0.3 V-2 and V-3 (M6.0.4.2, M6.0.4.3), then the parity patches P20–P28 and the amended
P2, P3, P4 and P19 (M6.0.4.5–M6.0.4.16), then P29 and P19 amended again (M6.0.7), then P30 (M6.0.11). PdMount pins
`pdmount-0.5` at `19cc34d` (tag `pin/19cc34d`). The `0.5/*`
branches, `pdmount-0.5`, `base/0.5.1` and the pin tags are on GitHub.

## Fork repository

Decisions D022, D089, D099.

| Item | Value |
|---|---|
| Location | `https://github.com/colsen665/proton-sdk-rs2`, a public GitHub fork; Actions stay off. Its `main` is upstream's plus the user's README note |
| Working copy | `<working copy>` (AGENTS.md, "Local paths"), `origin` = the fork over SSH; authentication is set in its repo-local git config |
| Remote `upstream` | `https://github.com/tirbofish/proton-sdk-rs2` (fetch only) |
| Base | tag `base/0.5.1` = `e2e45bbb` = upstream tag `v0.5.1`, the commit of the published 0.5.1 crates (`.cargo_vcs_info.json`). Before M6.0.1: `base/0.4.0` = `409ca119`, the commit of the 0.4.0 crates, kept for old pins |
| Integration branch | `pdmount-0.5`, built from `base/0.5.1`. PdMount pins a commit on it with `rev = …`. The old `pdmount` (on `base/0.4.0`) stays as it is, for old pins |
| Publication | The fork goes up before PdMount, after the publishing scan and after the maintainer has been told (D089). Push `pdmount-0.5`, the `0.5/*` patch branches, `pin/*` and `base/0.5.1`; the scan covers everything the target remote does not already hold |
| Pin tags | `pin/<short sha>` on every commit PdMount has pinned, so a rebuilt integration branch never orphans one; push the tag before the PdMount commit that pins it |
| Patch branches | one per patch, `0.5/<name>`, created from `base/0.5.1` (or from `pdmount-0.5` where the tracking table says so); merged into `pdmount-0.5` when tested. The branches on `base/0.4.0` keep their names without the prefix |
| Crates in the repo | `crates/proton-drive-sdk`, `crates/proton-sdk-rs2` (and `crates/pdcli`, unused) |

Working on a patch:

```sh
cd <working copy>
git switch -c 0.5/fix/block-pagination base/0.5.1
# … edit, test with cargo test -p proton-drive-sdk …
git push origin 0.5/fix/block-pagination
git switch pdmount-0.5 && git merge --no-ff 0.5/fix/block-pagination && git push origin pdmount-0.5
```

**Rebasing onto a new upstream release** (done for 0.5.1 in M6.0.1):
1. `git fetch upstream`; read the changelog and the diff of both crates since the old base. Per patch, decide: merged
   upstream (drop it), superseded (drop it and adapt PdMount), or still needed.
2. Tag the release commit `base/<version>`, after checking that it is the commit of the published crates
   (`.cargo_vcs_info.json`).
3. Rebase each branch still needed onto it as `<major.minor>/<old name>` (`0.5/fix/block-pagination`), with its tests. A
   branch that was based on the integration branch is based on the new one at the same point. Record conflicts in the
   commit message.
4. Build the integration branch `pdmount-<major.minor>` from the new base with `--no-ff` merges in the old order
   ("Merge 0.5/<branch> into pdmount-0.5"); run the fork's tests on every branch and on the result.
5. Keep the old branches, the old base tag, the old integration branch and every `pin/*` tag: old PdMount commits build
   from them.

**Open:**
- Upstream PRs go from the fork once the patches are stable, when the user says so (D089, TODO.md Q8). CI stays local.

Rules:

- One patch is one self-contained, upstream-ready branch, rebased on the newest upstream release, each with its own tests.
  Submitting upstream waits until the user says the patches are stable (D089).
- The fork carries only unmerged patches. When upstream merges a patch, the next fork rebase drops it.
- PdMount code depends on the `Backend` trait (DESIGN.md §3), never on fork-only API outside `pdmount-backend/src/proton/`.
  If a patch is rejected upstream, an adapter there replaces it.

Path prefixes: `sdk/` = `proton-drive-sdk-0.5.1/src/`, `rs2/` = `proton-sdk-rs2-0.5.1/src/` (upstream's published crates,
not the fork), `cs/` = `<official SDK checkout>/` (the official Proton Drive SDK, used as the behaviour reference). Each
patch's **0.5.1** line says what the rebase found and changed.

## Required by milestone

| Milestone | Patches |
|---|---|
| M0/M1 (read-only mount) | P1, P2, P4, P8 (P3 recommended) |
| M2 (writes) | P5, P6, P10, P13 |
| M3 (multi-device, robustness) | P9, P11, P12, P14, P15 |
| M4 (performance) | P7, P18 |
| M6 (SDK 0.5) | P19; P20–P28 (parity with the official SDK, M6.0.4); P29 (M6.0.7); P30 (M6.0.11) |

## Patches

### P1 — Paginate the revision block list
- **Where:** `sdk/node/revision.rs:233-270` (`create_download_state`) and its helpers `:303-325`, constants at
  `sdk/node/download.rs:18-19` (`MIN_BLOCK_INDEX = 1`, `DEFAULT_BLOCK_PAGE_SIZE = 10`).
- **Problem:** in 0.4.0, `get_revision(.., Some(MIN_BLOCK_INDEX), Some(DEFAULT_BLOCK_PAGE_SIZE), ..)` was called once. Only
  the first 10 blocks (10 × 4 MiB = 40 MiB) were downloaded, so larger files came back truncated, in both
  `download_to_stream` and `download_range` (F2).
- **Fix:** keep requesting with `from_block_index = last + 1` until a page has fewer than `page_size` blocks, as
  `cs/client/cs/src/Proton.Drive.Sdk/Nodes/Download/RevisionReader.cs` (`GetBlocksAsync`) and the JS `apiService.ts` do.
- **0.5.1:** still needed, reduced. Upstream 0.5.0 fixed the truncation (`bbcbf9c`, "download every block of files over
  40 MiB") with a loop inside `create_download_state`, but offers no public listing and fails on a page not sorted by
  index, which C# sorts. The patch now only replaces that loop and its helpers with `node::download::list_all_blocks`
  (sorts each page, requires indices from 1 without a gap) and the public `get_revision_with_all_blocks`, which
  `create_download_state` uses; upstream's two tests give way to the patch's nine.
- **Test:** 25 blocks over pages of 10, an exact multiple, a missing middle or last index, a listing not starting at 1,
  unsorted pages, a single block, an empty revision and a failing page.

### P2 — Verify block digests and the manifest signature
- **Where:** `sdk/node/download.rs:547-561` (`download_block_at`), `sdk/api/block.rs:33-50` (`BlockDto.hash_digest`).
- **Problem:** the downloaded block's SHA-256 is computed (`sdk/node/download.rs:245-246`) and discarded.
  `CompletedDownloadManifestVerificationException` is defined but never raised.
- **Fix:** compare each block's digest with `BlockDto.hash_digest`, and verify the manifest signature over the concatenated hashes as
  `RevisionReader.cs:366-382` does. Raise typed integrity errors. The manifest check covers the full-download path only;
  block-map reads get it from P17.
- **0.5.1:** still needed; rebased without conflict.
- **Amended 2026-11-19 (parity R6, Medium; M6.0.4.12):** a second commit on the branch.
  - *Where:* `sdk/block/download.rs` (`download_with_retries`, `classify_error`; fork lines 81-108 before the change).
  - *Problem:* a block whose expired target (404) the refresh did not find in the revision's block listing, or whose
    fresh target answered 404 too, failed with the raw 404, which a caller cannot tell from any other missing resource;
    C#'s `BlockDownloader` raises `DataIntegrityException("File contents are incomplete")`
    (`cs/client/cs/src/Proton.Drive.Sdk/Nodes/Download/BlockDownloader.cs:39-61`). A hash mismatch failed at once, while
    the JS SDK retries any failed block once for bit flips (`cs/client/js/src/internal/download/fileDownloader.ts:329-347`).
  - *Fix:* both cases of a block still missing after the refresh fail with `DataIntegrityException` ("File contents are
    incomplete: block #n …", with the 404 in the message). A hash mismatch is fetched once more, with a warning; a
    second one fails with `DataIntegrityException` as before. A connection cut in the body is still not retried (R6's
    PdMount part; not in the patch).
  - *Behaviour change:* one extra block request on a hash mismatch; a block gone after the refresh is a
    `DataIntegrityException` instead of an `HttpTransferError` 404.
  - *PdMount:* maps `DataIntegrityException` to `Integrity` (EIO, the block not cached, logged at ERROR) as before; the
    404 was `Other` (EIO). No code change; DESIGN §6.1 now says what the refresh does (the block's target, not the map's
    URLs), §6.3 the retry.
  - *Tests:* a mismatch that repeats fails after two fetches, one that does not succeeds on the second; a block the
    refresh does not list, or whose fresh target is missing, is a `DataIntegrityException` after one refresh (all failed
    before). PdMount: `a_block_missing_after_its_refresh_is_an_integrity_error`.
  - *Real account (checked 2026-11-19, `crates/pdmount/src/probe_parity.rs`):* a block's storage token is a JWT that
    expires 1800 s after it is issued, and the bare URL carries no expiry of its own; the target answered 200 at once.
    Whether an expired target answers 404 is still **not proven**: a probe would have to hold a block map for more than
    30 minutes (its `PROBE_P2_WAIT`). A gone blob of an active revision cannot be provoked from a client, so that case
    stays unproven too.

### P3 — Public reusable block map
- **Where:** `sdk/node/file/download.rs:173-212` (`download_range` rebuilds the download state on every call).
- **Problem:** FUSE reads need random block access without one or two API round trips per `read()`.
- **Fix:** public `BlockMap { content_key, blocks: Vec<{index, url, token, hash, plain_size}> }` with
  `fetch(revision) -> BlockMap`, `refresh_urls(&mut self)` (on a 404 / expired target, `sdk/error.rs:38-40`) and
  `download_block(&BlockMap, idx) -> Bytes`.
- **0.5.1:** still needed; on P1 and P2 as before. Upstream's new `FileDownloader::open_block_reader`
  (`sdk/node/file/download.rs:218`) reads blocks of one download state, but has no URL refresh and no manifest check.
- **Amended 2026-11-19 (parity R2, R3; M6.0.4.11, with P4):** a commit on the branch. `get_block_map` read
  `Common.BlockSizes` through the typed XAttr decode (`decrypt_claimed_block_sizes`), so another field that did not
  decode hid the block sizes, and unsorted pre-2025 sizes were taken as written. `claimed_block_sizes` now reads the
  field from the JSON on its own (invalid sizes: none, with a warning) and sorts those of revisions created before
  2025-01-01 in descending order, as P4 does for the node. Tests: `claimed_block_sizes_are_read_on_their_own` (the typed
  decode fails on the same XAttr), `claimed_block_sizes_before_2025_are_sorted`.

### P4 — Decrypt the active revision XAttr during node decode
- **Where:** `sdk/node/mod.rs:1373-1425`.
- **Problem:**
  - `claimed_size` is set from `storage_quota_consumption` (encrypted storage size), not from the plaintext size.
  - `claimed_modification_time` is set from the revision `creation_time`.
  - `claimed_digests.sha1` and `additional_claimed_metadata` are `None`. The XAttr field on the revision DTO (`sdk/api/revision.rs:33`) is
    available but unused.
  - Every consumer that uses these values for `st_size`/`st_mtime` shows wrong attributes, including pdcli (`pdcli/fs.rs:371-372`).
- **Fix:** decrypt the active revision XAttr with the node key and fill `Common.Size`, `Common.ModificationTime`,
  `Common.Digests.SHA1`, `Common.BlockSizes` and the flattened additional metadata (`sdk/api/attr.rs:53-56`). The same logic
  already exists for the explicit revision path (`sdk/client.rs:894-916`).
- **Acceptance:** PdMount scenario R1 in `fresh`/`second` mode. `rsync -ani` (quick check on size and mtime) prints nothing.
- **0.5.1:** still needed; upstream's new tests in `node/revision.rs` are kept beside the patch's.
- **Amended 2026-11-19 (parity R2, R3, both High; M6.0.4.11):** a second commit on the branch.
  - *Problem:* the XAttr was decoded into typed structs in one go (`sdk/utils/serde.rs:172-181`, used at
    `sdk/api/attr.rs:14-17`), so one field that did not decode (a time that is not RFC 3339, a size written as a string)
    discarded the whole XAttr, and the revision then took the encrypted storage size as its claimed size, without block
    sizes or digest: PdMount showed the encrypted size and reads ended in EIO. Block sizes a Windows client wrote unsorted
    before 2025 were taken as written. The official SDKs decode each field on its own (JS
    `cs/client/js/src/internal/nodes/extendedAttributes.ts:146-220`; C# `CommonExtendedAttributes.cs:11-12` with
    `Iso8601DateTimeOffsetResultJsonConverter.cs`), and JS sorts the block sizes of revisions created before
    2025-01-01 in descending order (`:212-218`).
  - *Fix:* `ClaimedRevisionAttributes::parse` reads the decrypted JSON and each `Common` field on its own: `Size` (a
    non-negative integer), `ModificationTime` (RFC 3339, else ISO 8601 without an offset as UTC, with a space, or a date
    alone), `BlockSizes` (positive 32-bit integers; empty is none), `Digests.SHA1` (hex or base64). An invalid field is
    left out and named in `invalid_fields`, logged at WARN with the revision; only text that is not a JSON object fails
    the XAttr. `claimed_size` is the claimed size or `None`: the storage size is never taken for it (the modification
    time still falls back to the revision's creation time). Block sizes of a revision created before 2025-01-01 are
    sorted in descending order. The typed `from_extended_attributes` gives way to `parse`.
  - *Behaviour change:* `Revision::claimed_size` is `None` for a revision without a readable claimed size, where it was
    the encrypted size.
  - *PdMount:* takes the size from the block sizes when there are any (as before); without either, the adapter shows the
    storage size, an upper bound, logged once per node at WARN (DESIGN §6.1). The block map reads its block sizes
    through P3's own decode, amended the same way (P3).
  - *Tests:* an XAttr with an invalid time keeps its size, block sizes, digest and metadata, one with an invalid size,
    block size and digest keeps its valid time; five time forms parse; block sizes of a revision created one second
    before 2025 are sorted, at 2025-01-01 not; without an XAttr, or with an unreadable one, the claimed size is `None`
    (all failed before). PdMount: `a_file_without_a_claimed_size_shows_its_encrypted_size`.
  - *Real account (checked 2026-11-19, `crates/pdmount/src/probe_parity.rs`):* every file of My Files (2 801), of the
    two folder shares below their roots (117) and of the Photos timeline (3), decoded without the entity cache: no XAttr
    with an invalid or unreadable field (no WARN line), every file has a claimed size, and no active revision was
    created before 2025, so the sorting of pre-2025 block sizes has nothing to act on there; it stays proven by the unit
    tests only. 141 files of My Files have no `BlockSizes`.

### P5 — Folder modification time
- **Where:** `sdk/node/folder.rs:121` (`_last_modification_time` ignored), `:111` (`extended_attributes: None`);
  DTO at `sdk/api/folder.rs:41`, `:65`.
- **Fix:** encrypt `{Common:{ModificationTime}}` as the folder XAttr on create, and decode the folder XAttr on read.
- **Note:** no folder XAttr update API is known. Directory mtimes that change after creation stay local-only (DESIGN.md §8).
- **0.5.1:** still needed. Upstream split `FolderOperations::create` into `prepare_creation` (`sdk/node/folder.rs:33`, also
  used by the Easy Switch import, which passes no time); `prepare_creation` now takes the claimed time and builds the
  XAttr. The XAttr is encrypted at the server time, like upstream's other crypto (behaviour change 2 below).

### P6 — Upload returns the new revision id
- **Where:** `sdk/node/file/upload.rs:134-139`, `:190-214`.
- **Problem:** `upload_from_stream` returns only `NodeUid`, although `draft.uid` (the revision) is known.
- **Fix:** return `(NodeUid, RevisionUid)`, or a struct. PdMount needs the revision id for echo suppression and conflict
  detection.
- **0.5.1:** still needed. Upstream split the upload into `write_draft` and a commit step that deletes the draft when
  writing fails (behaviour change 1 below); the result is built from the draft uid kept for that clean-up.

### P7 — Configurable per-client transfer semaphore
- **Where:** `sdk/node/transfer.rs:19` (`file_semaphore: Arc::new(Semaphore::new(1))`).
- **Problem:** one whole-file transfer at a time per client, in each direction.
- **Fix:** add a client option `max_concurrent_files` (default 1 for compatibility). PdMount uses 4 uploads plus its own read
  scheduler.
- **0.5.1:** still needed; rebased without conflict.

### P8 — Typed API errors
- **Where:** `sdk/api/file.rs:163-185` and sibling API modules, `sdk/api/mod.rs:237-247`.
- **Problem:** errors become `anyhow` strings (`"API error {code}: {msg}"`, `"HTTP {status}: {body}"`).
- **Fix:** `ApiError { http_status, code: Option<i64>, message, retry_after: Option<Duration> }` that can be
  downcast from `anyhow::Error`. Codes are listed in `cs/client/js/src/internal/apiService/errorCodes.ts:17-41`.
- **0.5.1:** still needed, with more coverage. Upstream's new `error::is_transient_error` (`sdk/error.rs:197`) also treats
  a retryable `ApiError` (408, 425, 429, 5xx) as transient, and the new `delete_album` parser reports an unparsable
  failure as `ApiError::from_http`, which keeps the body out of the error beyond an excerpt.

### P9 — Surface token refresh failures; configurable base URL
- **Where:** `rs2/auth.rs:194-220` (`get_refreshed_access_token`: a failed refresh yields the old pair, and the call
  returns `Ok`), `rs2/session.rs:511` (hard-coded `https://drive-api.proton.me`).
- **Fix:** return the refresh error and fire `refresh_token_expired` on invalid-grant responses. Use the configured base URL.
- **0.5.1:** still needed; upstream made part of it (`900107c`, in 0.5.0). A failed refresh now logs a warning, fires
  `refresh_token_expired` when the error text contains code 10013, and no longer broadcasts the old pair; but it still
  returns `Ok(old access token)`, and the probe URL is still hard-coded. `auth.rs` takes the patch's version, which
  covers upstream's change with a typed error instead of text matching; upstream's test that expected `Ok(old token)`
  is dropped, its case covered by the patch's.

### P10 — Public move with rename
- **Where:** `sdk/client.rs:1124-1125` (`move_nodes` passes `None` for the new name); implementation at
  `sdk/node/operations.rs:364` (`move_single(.., Option<name>)`).
- **Fix:** `ProtonDriveClient::move_node(uid, new_parent, new_name: Option<String>)`, so a cross-directory `rename(2)` is
  one remote call.
- **0.5.1:** still needed; upstream's new available-name helpers and tests in `node/operations.rs` are kept beside the
  patch's.

### P11 — Typed revision-conflict error
- **Where:** `sdk/node/draft.rs:194-209` (new-revision draft sends `CurrentRevisionID`), `:130` (new-file draft sends no
  `ClientUID`).
- **Fix:** map the server's rejection of a stale `CurrentRevisionID` to `RevisionConflict { current_revision }`. The same is needed
  for rename/move rejections on `OriginalHash` (`sdk/api/links.rs:562`, `:638`).
- **Open question (verify in M3):** which server error codes are returned, and whether these preconditions are enforced at draft creation, at commit, or both.
- **0.5.1:** still needed, on P8. Upstream's new `RevisionDraftProvider::delete_draft` (behaviour change 1 below) sits next
  to the patch's recovery of another client's stale draft.

### P12 — Retry for idempotent metadata requests
- **Problem:** only block transfers retry (`sdk/node/download.rs:154-175`, `sdk/api/storage.rs:9`). Metadata GETs fail on the
  first 429 or 5xx.
- **Fix:** bounded retry with exponential backoff and jitter on 408/425/429/5xx for GET requests, honouring `Retry-After`.
- **0.5.1:** still needed, on P8. Upstream wraps the Drive API, storage and Photos HTTP clients in its new
  `ServerTimeMiddleware` (behaviour change 2 below). The Drive API and Photos clients carry the retry middleware outside
  it, so every response, a retried one included, updates the server time.

### P13 — Batched fresh node fetch
- **Where:** `sdk/client.rs` (`get_node_uncached`, `enumerate_nodes`), `sdk/node/mod.rs` (`DtoToMetadataConverter`,
  `NodeBatchLoader`), `sdk/node/operations.rs` (`enumerate_nodes`).
- **Problem:** the only way to read a node past the entity cache, which revision commits and other devices' changes leave
  stale (DESIGN.md F14), is `get_node_uncached`: one `get_details` request per node. An event-following client refetches
  every node an event names, so it sends one request per node where the official SDKs send one per batch (C#: 50 links,
  `cs/client/cs/src/Proton.Drive.Sdk/Nodes/NodeProvider.cs:18`; JS: up to 100, `cs/client/js/src/internal/nodes/apiService.ts:32`).
  `enumerate_nodes` answers from the cache and runs 8 single-node requests at once; `NodeBatchLoader` needs the parent
  folder key up front and leaves stale cache entries.
- **Fix:** `ProtonDriveClient::get_nodes_uncached(volume_id, link_ids)`: evicts the links' cache entries, sends one
  `get_details` per 50 links (`FRESH_NODES_BATCH_SIZE`), one batch after the other, resolves parent keys as
  `get_fresh_node_metadata` does, and caches the decoded nodes and secrets. Missing links (the API omits them,
  `apiService.ts:231-235`) are missing from the result.
- **Test:** the batching helper sends 50, 50 and 20 links for 120 ids, nothing for none, and stops at the first failure.
- **0.5.1:** still needed (only `get_node_uncached`, `sdk/client.rs:601`); rebased without conflict.
- **Amended 2026-11-18 (M6.0.3 V-3, M6.0.4.3; D130):** each link's parent key was resolved with `?` inside the loop over
  all links, so one link whose parent folder does not decrypt (its secrets degraded in the cache, or the parent, or a folder
  above it, failing to decrypt when fetched) failed the call for every link, and an event-following client asked again at
  every poll. Now a link whose parent key fails with an `UndecryptableNodeError` is returned as a `DegradedNodeMetadata`
  built from what the link states in clear (uid, parent, times, owner, name hash; a file's media type and size), with the
  error "Parent folder key is degraded", and not cached; the others are decoded. Any other error (a request, an API
  error, a cache failure) still fails the call. The kinds are told apart by type: the new `error::UndecryptableNodeError`
  is what `FolderOperations::get_secrets` returns for a degraded folder (cached or fresh) and what the node converter
  returns when a folder or file does not decrypt (`Decryption failed for …`, `Failed to decrypt … passphrase`), with the
  old text. The converter itself never returns `PotentialObject::Degraded` (a node that does not decrypt is an `Err`), so
  the degraded node is built in the batch instead of passing no parent key to the converter. Tests: a batch of three
  links with the middle one under a parent whose secrets are cached as degraded returns the other two decoded and cached
  and the middle one degraded and uncached (failed before the fix); `get_secrets` of a degraded folder is an
  `UndecryptableNodeError`, behind a context too, and an untyped error with the same text is not. Not covered (R1's
  remainder, `docs/sdk-parity.md`): a link that fails to decrypt by itself still fails the batch, and a folder listing
  through `NodeBatchLoader` (upstream code) still fails for one bad child.

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
- **0.5.1:** still needed; based, as before, on the integration branch after P10 and P13 (`pdmount-0.5` at `720f6e9`).

### P15 — Trash time of v2 link details
- **Where:** `sdk/api/links.rs` (`LinkDto.trash_time`). Decision D044.
- **Problem:** link details come from `POST v2/volumes/{id}/links`, whose `LinkDto` names the trash timestamp
  `TrashTime` (`cs/client/js/src/internal/apiService/driveTypes.ts`, `LinkDto`; `apiService.ts` reads
  `link.Link.TrashTime`). The DTO reads `Trashed`, the v1 name, so `trash_time` is always `None`: `get_node`,
  `get_nodes_uncached` and the trash listing report trashed nodes as active (F16).
- **Fix:** `#[serde(rename = "TrashTime", alias = "Trashed")]`.
- **Test:** a DTO with `TrashTime`, with `Trashed`, with `null` and without the field. Checked against the live API in
  M3.1: a trashed file and folder report a trash time at once.
- **0.5.1:** still needed (`sdk/api/links.rs:441` reads `Trashed`); rebased without conflict.

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
- **Left as is:** the generic decode helpers (fork `pdmount-0.5`: `api/mod.rs:260,295,309,318`), `api/storage.rs:193`,
  `api/docs.rs:231`, the shared-by-me/with-me and album decoders, and, new in 0.5, the search parser (`search.rs:223`) and
  the Docs `ddocs get` decoder (`docs.rs:1016`) still put the body into the error of a malformed reply (error text, not a
  log call); PdMount logs such errors at WARN. TODO Known gaps ("Security (M4.0.1)").
- **Behaviour change:** none.
- **Test:** none in the fork (log output only); PdMount's `sdk_debug_records_never_reach_the_log` covers the filter that
  backs it up.
- **0.5.1:** still needed (upstream still logs the share responses, `sdk/api/share.rs:269`, `:307`, and the passphrase,
  `rs2/account.rs:226`). The commit and its rustfmt follow-up are squashed into one, based on `pdmount-0.5` at `bb73ff8`
  (after P15). Upstream's new log calls were checked: none prints a secret or a response body.
- **Amended (M6.0.4.2, security review 2026-11-18 V-2, Medium; D071 amended):** a second commit on the branch keeps the
  session-fork selector out of error texts.
  - *Where:* `rs2/auth.rs:736-749` (`poll_session_fork`: the selector is a path segment, `send().await?`),
    `:492-514` (`error_for_status_with_body`: `for url ({url})`); `rs2/session.rs:276` (`begin_via_web`: any poll error
    ends the login).
  - *Problem:* a reqwest transport error ends with `for url (<url>)`, and the status error wrote the URL itself, so a
    failed poll returned the selector, which alone collects the fork's `UID` and tokens; the caller logged it. The login
    ended on that first error, leaving the fork open on the server for a sign-in the user may still complete.
  - *Fix:* `poll_session_fork` returns a typed `SessionForkPollError` (`is_transient`, `retry_after`): transport and body
    errors go through `reqwest::Error::without_url()`; the status error names `auth/v4/sessions/forks/<selector>`
    (`SESSION_FORK_PATH`; `error_for_status_with_body` is split into `status_error(response, target)`, other callers keep
    `for url (…)`, their URLs carry no secret); a malformed 200 body reports serde's category and position only.
    `begin_via_web`'s loop (`collect_session_fork`) retries a transport failure, a poll over 30 s, and 408/429/5xx at
    the poll interval or after `Retry-After` (seconds) when longer, until `FORK_MAX_WAIT`. A login that ends without a
    session sends `DELETE auth/v4/sessions/forks/<selector>` (the official API's "Invalidates the forking selector,
    without forking the session", `cs/incubating/account/js/src/api-auth-types.ts:119-143`; the official client does not
    call it), best effort within 10 s, errors ignored. New trait method
    `AuthenticationApiClient::invalidate_session_fork`, with a default that does nothing.
  - *Behaviour change:* a 503 or a dropped connection during the sign-in no longer ends it; a lost answer to a poll that
    the server did serve (the fork is then spent) may wait for the 10 min limit instead of failing at once, if the
    server answers the next polls with 422 (not tested).
  - *Tests:* `session::tests::fork_poll` against a local HTTP server: 503, a dropped connection, 429 with `Retry-After`
    and 400 give errors whose `{:#}` and `{:?}` texts lack the selector (failed before the fix with the URL in both the
    status and the transport text); 503 then 422 then 200, and a dropped connection then 200, complete the login; a
    timed-out and a rejected sign-in send the `DELETE`. PdMount: `sign_in_errors_hide_the_fork_selector`.

### P17 — Manifest verification on block-map reads
- **Where:** `node/file/block_map.rs` (P3: `get_block_map`, new `verify_block_listing`), `sdk/node/download.rs`
  (`manifest_verification_keys`, shared with `RevisionReader::verify_manifest`), `rs2/api/response.rs`
  (`AddressPublicKeyListResponse`), `rs2/account.rs` (`get_address_public_keys`). Security review findings S-4 (Medium)
  and S-11 (Low), M4.0.2.2.
- **Problem:** `download_block` checks each block against its listed hash, but nothing checks the listed hashes: the
  manifest signature is verified only by full downloads (P2). A storage or server that lists other blocks (another revision
  of the same file, say) passes every check. A block listed with an empty hash skips the per-block check
  (`block/download.rs` `verify_block_hash`, P2).
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
- **0.5.1:** still needed (`rs2/api/response.rs:217-223` still expects `Address` as a string); two commits as before,
  based on `pdmount-0.5` at `4bca0d5` (after P16).

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
- **0.5.1:** still needed (`sdk/client.rs:98` still fixes 6); based on `pdmount-0.5` at `f0b77c4` (after P17), needs P7.

### P19 — Ignore one-second jitter of the server time offset
- **Where:** `sdk/server_time.rs:24-26` (`update`), read by `now()` for new keys (`sdk/crypto.rs:27`) and by `check_now()`
  for signatures and encryption (`sdk/pgp.rs:50`, `:61`, `sdk/node/crypto.rs`, `sdk/block/upload.rs` and others).
- **Problem:** every response's `x-pm-date` (else `Date`) overwrites the process-wide offset, server time minus device
  time, both in whole seconds (behaviour change 2 below). With the clocks in sync, a response stamped just before a second
  boundary and received just after it sets the offset to -1. If that lands between generating a node key (dated S) and
  signing with it (dated S-1), rPGP rejects the key: "Failed to select signing key: No valid user-id with
  self-certification found ... Signature has a creation time in the future". G3 on the real account hit it about once in
  100 operations (M6.0.1); PdMount's retry hid it.
- **Fix:** `update` keeps the stored offset unless the observed one differs from it by 2 s or more. The decision is the
  pure `adopted_offset(current, observed)`, applied with `AtomicI64::fetch_update`, so concurrent updates stay lock-free
  and each is decided against the stored value. A real clock difference is still adopted on the first response; the
  stored offset stays within a second of the true one.
- **Test:** ±1 s is ignored, 2 s or more (either way, and an extreme jump) is adopted, 0, -1, 0, -1 stays at 0, and jitter
  around a large adopted offset is ignored while a further 2 s change is adopted.
- **Behaviour change:** the offset ignores changes of one second, so keys and signatures may be dated up to about a second
  off the server clock, but a signature is no longer dated before the key it uses. An offset change of 2 s or more
  between a key's creation and its signature can still produce the error above.
- **0.5.1:** new, from upstream 0.5.0's server-time sync (behaviour change 2 below); upstream's code unchanged since.
  Branch from `base/0.5.1`.
- **Amended (M6.0.4.16, parity finding A1, Medium):** a second commit on the branch keeps the crypto time from going
  back.
  - *Problem:* an offset change of 2 s or more backwards, or the device clock stepped back, still moved the crypto time
    back, so a signature made right after a key could be dated before it (the error above). C# keeps the latest `Date`
    seen and never moves back (`cs/client/cs/src/Proton.Sdk/Cryptography/CryptographyTimeProvider.cs:7-30`), without a
    bound: one answer with a far-future `Date` pins its crypto time ahead for the rest of the process (the concern of the
    M6.0.3 review's V-9, `docs/security/2026-11-18-review/verification.md`).
  - *Fix:* the offset and a floor move into `CryptoClock` (one static instance, as before). `now_unix_seconds` returns
    device time plus offset, but never less than the latest time returned while that floor is at most
    `MAX_FLOOR_LEAD_SECONDS` (60 s) ahead; a larger step back moves the time back with it and logs a WARN. The floor is a
    lock-free `fetch_update`, as the offset; the 2 s rule is unchanged.
  - *Behaviour change:* after the offset or the device clock goes back by up to 60 s, keys and signatures keep the
    latest time until the clock passes it, so they may be dated up to 60 s ahead of the newest answer's server time for
    up to 60 s; that lengthens V-8's window (signed at server time, verified at device time) by as much. One far-future
    answer still dates what is made before the next answer ahead (as before, V-9), but no longer than that. A step back
    of more than 60 s can still date a signature before its key.
  - *Tests:* a 5 s smaller offset and a device clock 30 s back hold the time, which moves on once passed; a floor 60 s
    ahead holds, 61 s does not; a year-ahead answer (and `i64::MAX`) does not pin the time after a right answer. The
    first three failed before the amendment; the bound tests fail with C#'s unbounded floor.
  - *PdMount:* no change; PdMount reads no crypto time.
- **Amended (M6.0.7, security finding V-9, Low; design accepted by the user):** a third commit takes the offset from the
  Drive API only and bounds it, since P29 verifies signatures at this time too.
  - *Problem:* every answer of the API, block storage, Photos and public-link clients set the offset
    (`sdk/client.rs:197-198`, `:294-295`, `sdk/photo.rs:129`, `sdk/sharing.rs:917`, `:921`, `sdk/http.rs:68`), adopted
    without a bound once it differed by 2 s. With P29 a wrong offset makes fresh signatures fail or future-dated ones
    pass, besides misdating what is made meanwhile; one stray answer was enough.
  - *Fix:* `ServerTimeMiddleware::for_api(api_base_url)` takes the time only from answers to requests to the Drive
    API's scheme, host and port; `client_with_server_time` takes that base URL. It wraps the Drive API client
    (`ProtonDriveClient` from a session and from an HTTP client factory) and the Photos API client (`photo.rs`, on the
    same host). Block storage clients, the public-link clients (`sharing.rs`) and `http::create_client_with_timeout`
    (no host of its own) set no time any more. The first offset adopted after a start must be within
    `MAX_FIRST_OFFSET_SECONDS` (a day) of the device clock: a larger one is refused with a WARN (once per refused
    value) and the device clock stays, as it does for the next answers until one is within a day. Later, a change of
    more than `MAX_UNCONFIRMED_CHANGE_SECONDS` (60 s) is adopted only when a second answer confirms it (its offset
    within 2 s of the pending one); until then the old offset stays, with a WARN per pending value. An answer that
    confirms nothing replaces the pending value, one that agrees with the current offset clears it. Changes up to 60 s
    follow the 2 s rule as before. `offset_seconds()` returns the adopted offset, `None` before the first. The decision
    is the pure `decide`; the offset stays one lock-free `fetch_update` with the pending and refused values in atomics
    beside it (a race between two answers can at most delay an adoption by one answer); the floor is unchanged; an
    observation is clamped off `i64::MIN`, which stands for none.
  - *Behaviour change:* storage and public-link answers no longer move the crypto time. A device more than a day off
    the server signs and verifies at its own clock, as before upstream's offset existed (a clock ahead makes keys from
    the future, which the API rejects), and the WARN says why. A real correction of more than a minute (NTP stepping the device clock) is adopted at the
    second answer after it, so for one answer keys and signatures stay dated with the old offset. A pair of agreeing
    wrong answers still moves the offset; only the first one is bounded.
  - *Tests:* a far-future and a far-past first answer (and `i64::MAX`, `i64::MIN`) are refused, also when repeated; a
    2 h and a one-day first offset are adopted; a 10 min jump either way is held until a second answer agrees; one odd
    answer between two right ones changes nothing, and a non-confirming one replaces the pending value; 60 s is
    adopted and 61 s held; an answer from another host than the API's is ignored (two local HTTP servers). The
    refusal, jump, odd-answer and one-minute tests failed before; the host test needs the new API. The floor tests take
    two answers for a change of more than a minute.
  - *Branch:* commit `a6786c0` on the branch, then `0.5/feat/metadata-retry` (P12) merged into it (`c368c40`): P12
    builds the API and Photos clients with its retry middleware, so the amendment conflicted with it in `client.rs`
    and `photo.rs` when merged into `pdmount-0.5`. Resolved in the branch, as P21 carries P20: the retry middleware
    stays outermost, the API clients take `ServerTimeMiddleware::for_api`, the storage client is not wrapped.
  - *PdMount:* `doctor`'s clock line shows the offset (D133, DESIGN §14.2).

### P20 — Verify each encrypted block before upload
- **Where:** `sdk/block/verify.rs:10-47` (`BlockVerifier::verify_block`), `sdk/block/upload.rs:78`, `:201` (its callers
  `encrypt_block` and `upload_content`). Parity finding W1 (High), M6.0.4.5.
- **Problem:** `verify_block` ignored its session key and the plaintext prefix and only XORed the verification code with
  the packet prefix. A block that does not decrypt (a bit flipped by faulty memory or CPU, a wrong content key) was
  uploaded and committed; after the commit PdMount seeds the block cache from the plaintext snapshot (DESIGN §7.4 step 8),
  so only another device's read found it. Both official SDKs check every block before upload and encrypt it once more on
  a failure: C# decrypts a prefix with the session key from the verification endpoint and compares it with the plaintext
  prefix (`cs/client/cs/src/Proton.Drive.Sdk/Nodes/Upload/Verification/BlockVerifierBase.cs:20-46`,
  `Nodes/Upload/ContentEncryptionOperations.cs:77-88`); JS decrypts the whole block "to try to detect bitflips / bad
  hardware" (`cs/client/js/src/internal/upload/cryptoService.ts:203-231`, `streamUploader.ts:76`, `:315-353`).
- **Fix:** `verify_block(encrypted_data, plain_data)` decrypts the whole block with the verification session key and
  compares it with the plaintext, then computes the token as before. Whole block rather than C#'s prefix: the blocks this
  SDK writes are SEIPDv1 (the content key of a new file is a v4 AES-256 key) unless the file's content key is a v6 one,
  and a SEIPDv1 packet's MDC covers the whole packet, so a decrypted prefix proves nothing about the rest; JS's reason
  holds too. The cost was measured at about 3 ms per 4 MiB block in a release build (`verification_cost`, ignored test),
  against about 7 ms for the encryption alone and some 200 ms to send the block at 20 MB/s. A failure is an
  `error::IntegrityException` (the type existed, unused; JS throws its generic `IntegrityError`). `encrypt_block` and
  `upload_content` share `encrypt_content` and `encrypt_and_verify`, which encrypts again (signature included) up to
  `MAX_BLOCK_ENCRYPTION_RETRIES` = 1 time, as both official SDKs, and then returns the error with the block number.
- **PdMount:** `classify` maps `IntegrityException` to `Integrity`, so the upload is retried with the journal's backoff and
  parked after five attempts, as a size mismatch at the commit (DESIGN Appendix A).
- **Test:** a block that decrypts to its plaintext gets the token; a flipped bit in the middle or the tail, another
  session key and other plaintext fail with `IntegrityException` (the last three failed before the fix); an empty block
  and a SEIPDv2 key verify; a block that fails once is encrypted again and passes, one that keeps failing is an
  `IntegrityException` after two encryptions; an encryption error is not retried. PdMount:
  `integrity_and_unknown_errors_are_classified`, `an_upload_that_fails_its_integrity_check_is_uploaded_again`.
- **Behaviour change:** each block is decrypted once more before upload; a block that fails twice fails the upload with
  `IntegrityException` instead of being uploaded. A caller that passed prefixes to the public `verify_block` gets an
  error now: it takes the whole block.
- **Branch:** `0.5/fix/block-upload-verification` from `base/0.5.1`.

### P21 — Small files in one request
- **Where:** `sdk/node/file/upload.rs` (`FileUploader`), `sdk/node/draft.rs` (the draft providers), `sdk/api/file.rs`
  (new requests), `sdk/node/revision.rs` (thumbnail and XAttr encryption shared with the commit). Parity finding W2
  (Medium), M6.0.4.7.
- **Problem:** every upload is a draft, a block request, the block upload and a commit: four requests or more for a git
  object of a few bytes. The official SDKs create and commit a file or revision under 128 KiB (with its thumbnails, × 1.1
  for encryption) in one request to `v2/volumes/{volumeID}/files/small` or `…/files/{linkID}/revisions/small`, behind the
  feature flag `DriveSmallFileUpload` (`cs/client/cs/src/Proton.Drive.Sdk/Nodes/Upload/RevisionUploadBackendFactory.cs:11-12`,
  `:113-127`; `Api/Files/SmallRevisionUploadBackend.cs`; `Api/Files/FilesApiClient.cs:118-205`; JS
  `cs/client/js/src/internal/upload/index.ts:13`, `:40-47`, `smallFileUploader.ts`, `apiService.ts:342-488`).
- **The feature flag:** neither official SDK reads it from the server. Both take a `FeatureFlagProvider` from the
  application (C# `IFeatureFlagProvider`, default `AlwaysDisabledFeatureFlagProvider`; JS `featureFlagProvider`, default
  `NullFeatureFlagProvider`); the web apps fill it from their own flag service, which is not in the official SDK code.
  rs2 already has the same shape: `ProtonClientOptions::feature_flag_provider`, default `AlwaysDisabledFeatureFlagProvider`.
  So the small path is off unless the application turns the flag on, and the flag's server source is not implemented.
- **Fix:** `FileUploader` takes the small path when the upload is not a photo, fits the limit, and the client's
  provider says `DriveSmallFileUpload` (`node::file::upload::DRIVE_SMALL_FILE_UPLOAD`) is on. It reads the content (at most
  128 KiB + 1 byte; a stream of another length than announced goes to the draft path with the bytes read) and asks the
  draft provider for `get_small_upload` (default `None`): a new file's keys and draft request, or a revision's keys and
  its content key packet read fresh with `get_details` (none when that fails: the draft path, as C#'s
  `TryGetContentKeyPacketAsync`). The block is encrypted and verified (P20) with the packet's session key and its last 32
  bytes as the verification code (C# `SmallFileUploadBlockVerifier`); thumbnails, the manifest signature and the XAttr are
  made as for a commit. The multipart body is `Metadata` (JSON: the draft's fields with `NameHash`, the commit's fields,
  `ContentBlockEncSignature`, `ContentBlockVerificationToken`, `Photo: null`), `ContentBlock` (none for an empty file) and
  `ThumbnailBlockType_<n>`. The result is `UploadResult` with the new revision's UID (P6).
- **Fallback (C# `FileUploader.IsSmallUploadFallbackException`):** only when nothing was sent, or on a clean conflict
  (code 2500 or HTTP 409), which maps to `NodeWithSameNameExistsError` for a new file and `RevisionDraftConflictError` for
  a revision, as C# maps it. The draft path then runs: it removes a stale draft of this client (P11) or fails with the
  same typed error as before, so PdMount sees the errors it relies on unchanged. Any other failure after the request
  (5xx, a dropped connection) is returned and not repeated: the request creates and commits at once and is not
  idempotent. P12 does not retry a POST on 5xx.
- **PdMount:** no code change. Its client options leave the provider at the default, so the small path stays off; it is
  for the real account to show which flag source to use (below).
- **Test (fork, against a local HTTP server):** a small file and a small revision go up in one request with the expected
  path, parts and metadata, the block decrypts to the content and carries the packet code's token, and the result has
  the new revision; an empty file sends no block; a name conflict and a revision conflict fall back to the draft path and
  return its `NodeWithSameNameExistsError` / `RevisionDraftConflictError`; a 503 is returned without a second request or
  a draft; the flag off, or a file or thumbnails over the limit, take the draft path. None compiled before the fix.
- **Behaviour change:** none with the default provider. `RevisionDraftProvider` gains `get_small_upload` (with a
  default) and `FilesApiClient` two methods (`upload_small_file`, `upload_small_revision`), which an outside
  implementation of the trait must add.
- **Branch:** `0.5/feat/small-file-upload`, based on P23's branch with P20's merged into it: it extends P23's draft
  provider and uses P20's block encryption and verification.
- **Real account (checked 2026-11-19, `crates/pdmount/src/probe_parity.rs`, the flag on in the probe's client only):**
  both endpoints accept the request for the test account: a new file, a revision and an empty file went up in one
  request each (route `small` in the upload telemetry), read back through the SDK with the right content and claimed
  size, and their name, node and content signatures verify. A name conflict on `files/small` answers 2500 (HTTP 422), as
  C# expects: the upload falls back to the draft path, which fails with `NodeWithSameNameExistsError` (PdMount:
  `AlreadyExists`). A stale `CurrentRevisionID` on `revisions/small` answers **2511 with HTTP 409** ("Current revision
  is no longer up to date"), not 2500; the upload still falls back, because the conflict rule also takes HTTP 409, and
  the draft path then fails with `RevisionDraftConflictError` (the same 409/2511; PdMount: `RevisionConflict`). A file
  uploaded this way (`My Files/pdmount-test/p21-small.txt`) opens in the web client with its full text (checked by the
  user, 2026-11-19). Still open before any client turns the flag on: the flag source.

### P22 — A commit answered "not in draft" checks the revision
- **Where:** `sdk/node/revision.rs:813-821` (`RevisionWriter::commit`, `update_revision`), new `seal_revision`. Parity
  finding W4 (Medium), M6.0.4.15. Branch `0.5/fix/commit-not-in-draft` from `0.5/feat/typed-api-error` (P8), for
  `ApiError`.
- **Problem:** the commit fails on any error of the revision update, and ignores a 2xx answer with an error `Code`. P12
  retries the `PUT` after a timeout or a 5xx, so a commit whose first answer was lost after the server applied it comes
  back as 2511 ("incompatible state": the revision is no longer a draft) and is reported as failed. C# reads the revision
  then and reports success when it is `Active` or `Obsolete`
  (`cs/client/cs/src/Proton.Drive.Sdk/Api/Files/RegularRevisionUploadBackend.cs:143-170`, `:267-280`).
- **Fix:** `seal_revision`: an answer with a non-success `Code` is an `ApiError`, 2xx or not. On 2511 it reads the
  revision (without block URLs) and succeeds when its state is `Active` or `Superseded` (C#'s `Obsolete`); a draft, or a
  revision that cannot be read, keeps the 2511 error (C# throws the read's error instead). The upload then returns the
  draft's revision id as for any commit (P6).
- **Test:** against a local HTTP server: 2511 then an active or an obsolete revision succeeds with one `PUT` and one
  `GET`; 2511 in a 200 answer too; 2511 then a draft, or then a failed read, fails with 2511; success, 2500 and a 200 with
  code 2000 send no `GET`. Before the fix all four tests failed.
- **PdMount:** no code change. The upload's revision id is the draft's in either case, and the journal records it in
  `own_revisions` (DESIGN §7.4 step 7), so the event of that commit is an echo (§7.5) instead of a revision the next
  plan has to read and adopt, with a `remote_newer` notice meanwhile. A 2511 whose revision is still a draft or gone stays
  `Transient` (DESIGN Appendix A).
- **Real account (checked 2026-11-19, `crates/pdmount/src/probe_parity.rs`):** the commit of an active revision sent
  again (the same request with the manifest signature by the primary key, without the XAttr) answers HTTP 422 with code
  2511 ("Revision to commit must be a draft"); the revision read without block URLs is then `Active`, so `seal_revision`
  reports success. The probe sends the two requests `seal_revision` sends and applies its rule, since the function is
  private; the file's content and signatures are unchanged afterwards. A commit whose first answer is lost still needs a
  proxy to provoke.

### P23 — Intended upload size on drafts
- **Where:** `sdk/node/draft.rs:131` (`intended_upload_size: None` on a new file's draft), `:194-197` (a revision draft
  without the field), `sdk/api/revision.rs` (`RevisionCreationRequest`), `sdk/node/file/upload.rs` (the uploader asks for
  the draft). Parity finding W6 (Low), M6.0.4.6.
- **Problem:** the size is known before the upload (DESIGN §7.4 step 3) but never reached the server, which can refuse an
  upload over quota at the draft only when it is told the size. Both official SDKs send `IntendedUploadSize`: C#
  the exact size on both drafts (`cs/client/cs/src/Proton.Drive.Sdk/Nodes/Upload/NewFileDraftProvider.cs:144`,
  `NewRevisionDraftProvider.cs:38`); JS rounds a new file's size down to 100 000 bytes and sends none below that ("The
  client shouldn't send the clear text size of the file"), and a revision's exact size
  (`cs/client/js/src/internal/upload/apiService.ts:102-123`, `:154`).
- **Fix:** `RevisionDraftProvider::get_draft(intended_upload_size)` (C#'s `GetDraftAsync(intendedUploadSize, …)`);
  `FileUploader` passes its size. A new file's draft sends it as JS does (rounded down to 100 000 bytes, none at or below
  that); a revision's draft sends the size, as both SDKs, and none for an empty revision (JS). The stricter of the two
  official behaviours for files, and the one both share for revisions, so each request is one an official client sends.
- **PdMount:** no code change; it already passes the size to `get_file_uploader` and `get_file_revision_uploader`. A
  quota refusal at the draft maps to `QuotaExceeded`, as at the commit (DESIGN Appendix A).
- **Test:** the rounding of both sizes; the serialized revision request with and without the size; an uploader asks its
  draft provider for the upload's size (none compiled before the fix: the parameter and the field did not exist). The
  test telemetry now records a non-JSON payload (`uploadError`) as text.
- **Behaviour change:** drafts carry the size; the trait method `get_draft` takes it, so an outside implementation of
  `RevisionDraftProvider` must add the parameter.
- **Branch:** `0.5/feat/intended-upload-size` from `pdmount-0.5` at `55adc5f`: it changes the draft code of P11 and the
  uploader of P6.
- **Real account (checked 2026-11-19, `crates/pdmount/src/probe_parity.rs`):** a new file's draft and a revision's draft
  announcing 0.25 GiB more than the remaining quota are refused at the draft, before any content is read: HTTP 422, code
  200002 ("Storage quota exceeded"), which PdMount maps to `QuotaExceeded`. No draft was left behind.


### P24 — Membership acceptance of shared nodes
- **Where:** `sdk/api/share.rs:787-812` (`ShareMembershipSummaryDto`), `sdk/node/mod.rs:455-459` (`NodeMembership`),
  `:1453-1525` (`build_membership_info`), `:1034-1058` (its report). Parity finding F3 (Medium), M6.0.4.13. Branch
  `0.5/feat/membership-acceptance` from `base/0.5.1`.
- **Problem:** the membership of a node shared with the user decodes neither the accepting address (`AddressID`) nor the
  member's acceptance signature (`InviteeSharePassphraseSessionKeySignature`), so a node has `shared_by` only. The JS
  SDK verifies and exposes the acceptance as `acceptedBy` since `3b8d4428`
  (`cs/client/js/src/internal/nodes/cryptoService.ts:401-530`, `verifyMembershipInvitee`;
  `cs/client/js/src/crypto/driveCrypto.ts:663-684`, `verifyAcceptInvitation`).
- **Fix:** both fields in the DTO, and `NodeMembership::accepted_by`, checked as JS does: the member key packet is
  decrypted with the private keys of the accepting address, and the signature over its session key is verified with that
  address's public keys in the `drive.share-member.member` context (the one `accept_invitation` signs in). The inviter and
  invitee checks run concurrently. A failure is a degraded author with JS's messages, never an error: the node is still
  returned, as the official clients show it with a warning. A failed inviter or invitee check is logged at WARN once per
  node and field (JS logs a warning per node) and reported as `membershipInviter` / `membershipInvitee`. A membership
  cached without the field reads as not loaded.
- **Test:** the DTO with and without the fields; an acceptance signed by the member verifies; another key, another
  context, a key packet for another key, a missing `AddressID`, a failed key lookup and no published keys are degraded,
  naming the member where JS does; an old cached membership deserializes. The tests need the new fields, so they did not
  compile before the fix.
- **PdMount:** nothing new is used. `accepted_by` names the user's own address, not who shared the node, so it cannot
  improve the owner suffix of share names (DESIGN §14.5, D107, D112), which stays the share's creator. The fork's WARN line
  reaches PdMount's log at the default level. The adapter builds against the field unchanged; no new fork API is called.
- **Real account (checked 2026-11-19, `crates/pdmount/src/probe_parity.rs`):** all three nodes shared with the test
  account (two folders, the editor test share among them, and a file) carry both fields: their acceptances verify (the
  armored signature parses and checks against the accepting address), as do their inviters; no WARN line.

### P25 — One request per share
- **Where:** `sdk/share_ops.rs:33` (`ShareOperations::get_share`), `:172` (`decrypt_share_with_creation_time`), its callers
  `sdk/node/operations.rs:64` (My Files) and `sdk/photo.rs:164` (Photos root). Parity finding F4 (Medium), M6.0.4.14.
  Branch `0.5/fix/share-single-get` from `base/0.5.1`.
- **Problem:** `get_share` fetched the share (`GET shares/{id}`), and the decryption fetched it again after decrypting,
  only to build the `Share` from the second answer: two requests for a share not in the cache, and one more for the My
  Files and Photos roots, which already hold the share from their v2 response. C# sends one per share
  (`cs/client/cs/src/Proton.Drive.Sdk/Shares/ShareOperations.cs:8-10`) and passes the root and type to the decryption
  (`Shares/ShareCrypto.cs:11-19`).
- **Fix:** the decryption takes the `Share` from its caller and sends no request. `get_share` builds it from its one
  response; the My Files and Photos roots from their v2 response, as C# does (root = the response's link on its volume,
  type `Main` or `Photos`, `NodeOperations.cs:452-460`, `PhotosNodeOperations.cs:32-40`). The public
  `ShareCrypto::decrypt_share`, which is given neither root nor type, keeps its signature and fetches the share once
  before decrypting instead of after.
- **Test:** `get_share` against a local HTTP server that counts requests: one GET for a share not in the cache, none
  for the cached share, and the share's root, address and type taken from that response (failed before: two GETs).
  `from_http_clients` becomes `pub(crate)` for the test.
- **PdMount:** unchanged. The shared-with-me listing keeps its own `GET shares/{id}` once per share and session for the
  creator and creation time (`crates/pdmount-backend/src/proton.rs`, `shared_listing`), which the SDK's `Share` does not
  carry; the SDK's decryption of a shared root now adds one GET per share instead of two, so a cold cache costs two
  per share instead of three.

### P26 — A node that does not decrypt is degraded in listings and batches
- **Where:** `sdk/node/mod.rs` (`DtoToMetadataConverter::convert_dto_to_node_metadata_inner`, `NodeBatchLoader`,
  `VolumeTrashBatchLoader`; P13's `decode_fresh_links`), used by `sdk/node/folder.rs` (`enumerate_children`,
  `enumerate_children_to_channel`) and `sdk/volume_operations.rs` (trash). Parity finding R1 (High), M6.0.4.9; D132.
- **Problem:** the converter returns an error for a node whose passphrase, key, name, hash key or content key does not
  decrypt (`sdk/node/mod.rs:1243`, `:1440-1448`), and the folder listing, the trash listing and P13's batch propagate
  it: one such child fails the listing of its whole folder, and one such node every event page that names it. The
  official SDKs return the node with an error per field (C# `Nodes/DtoToMetadataConverter.cs`; JS
  `cs/client/js/src/internal/nodes/cryptoService.ts:134-150`). P13 as amended in M6.0.4.3 covered only a node under an
  undecryptable parent.
- **Fix:** the converter builds a `DegradedNodeMetadata` when a node does not decrypt: what the link states in clear
  (uid, parent, times, owner, name hash; a file's media type and storage size), the parts that did decrypt (name, node
  key, passphrase, hash or content key) and a `CryptoError` per field that did not. The new
  `convert_dto_to_node_metadata_or_degraded` returns it as `PotentialObject::Degraded`; the folder listing, the trash
  listing and `decode_fresh_links` use it. A trash entry whose parent folder does not decrypt is degraded as in P13's
  batch (before, it was decrypted with the share key). Degraded nodes are not cached.
  `convert_dto_to_node_metadata_with_client` and `convert_dto_to_node_metadata` keep their contract (an
  `UndecryptableNodeError` with the old text), so single-node reads (`get_node`, `FolderOperations::get_secrets`) are
  unchanged, as are errors that are not decryption failures (a request, a missing DTO, a cache failure).
- **Behaviour change:** folder and trash listings and `get_nodes_uncached` return a node that does not decrypt as
  degraded instead of failing; callers that skipped degraded entries now see fewer errors and more degraded entries.
  `enumerate_nodes` (single-node reads, 8 at once) still fails on such a node.
- **PdMount:** `Backend::list_children_checked` names degraded children apart; a known one keeps its entry, and a
  folder deleted locally whose remote copy holds one is kept (D132, DESIGN §9.2).
- **Test:** a listing of four children, one whose passphrase is encrypted to another key and one whose hash key does not
  decrypt, returns two nodes and two degraded ones, the second with its name; a fresh batch with a link that does not
  decrypt by itself under a readable parent returns it degraded and uncached, the others decoded and cached (both
  failed before with "Decryption failed for folder"); a single-node conversion of such a link is still an
  `UndecryptableNodeError`. PdMount: see D132.
- **0.5.1:** new. Branch `0.5/fix/undecryptable-node` from `pdmount-0.5` at `55adc5f`: it builds on P13 as amended and
  changes the converter where P4 and P5 changed it (on `0.5/feat/fresh-node-batch` alone it conflicts with both).

### P27 — Signature verification failures kept on the node
- **Where:** `sdk/node/crypto.rs` (`decrypt_link`, `decrypt_hash_key`, `decrypt_content_key`, `decrypt_file`),
  `sdk/node/authorship.rs` (`AuthorshipClaim`), `sdk/node/mod.rs` (the converter's `name_author`, `author`,
  `content_author`). Parity finding R8 (Medium), M6.0.4.10; D132.
- **Problem:** `name_author` and `author` are always the claimed emails and `content_author` always the claimed author
  (`to_potential_author` never reports a failure, `sdk/node/authorship.rs:63-68`; `sdk/node/mod.rs:1189-1194`,
  `:1322-1327`), so a caller cannot see that a signature did not verify; failures reach telemetry only. The name is
  checked against the node's author (`SignatureEmail`), not the name's (`NameSignatureEmail`); an anonymous node's
  passphrase and name are not checked; the hash key is checked without the node key; a named author whose keys cannot be
  had counts as verified; `ContentKeySignature` is never checked (`sdk/node/crypto.rs:286-308`). The official SDKs keep
  each failure on the node (C# `Nodes/Cryptography/NodeCrypto.cs:180-347`, `DtoToMetadataConverter.cs:222-232`,
  `:389-400`; JS `nodes/cryptoService.ts:96-150`).
- **Fix:** as C#: the name is checked against its own author, the passphrase against the node's; for an anonymous node
  both against the parent's key (none for a root); the hash key and the content key against the node key and the node
  author's keys; `ContentKeySignature` is verified over the content key (missing or foreign: a failure); a named author
  without keys is a failure (`AuthorshipVerificationFailure::NO_VERIFICATION_KEYS`). The converter keeps each failure:
  `name_author` from the name, `author` from the passphrase (a folder's hash key first), the active revision's
  `content_author` from the content key, as `PotentialObject::Degraded` with a `SignatureVerificationError` naming the
  claimed author. New `AuthorshipClaim::to_verified_author`, `verification_claim`, `checked`;
  `LinkDecryptionResult::name_authorship_claim`.
- **Behaviour change:** nodes whose signatures do not verify now carry degraded authors; nothing is refused and the node
  decodes as before. Telemetry also reports content key verification failures, which it never saw.
- **PdMount:** reports the failed parts with the node (`RemoteNode::unverified`, with the claimed signers and a digest)
  and logs it at WARN once per node and process; since M6.0.6 a tree in `refuse` mode (My Files and Photos by default)
  refuses it until `pdmount signatures accept`, and every tree lists it (D132 amended, D134, DESIGN §6.3).
- **Test:** a folder whose name is signed by a stranger decodes with its name author unverified and its author
  verified; a folder whose named author has no keys has both unverified; the content key verifies with its node key's
  signature, not with a stranger's or none; a hash key signed by its node key verifies, a stranger's does not (all four
  failed before). PdMount: `an_unverified_node_is_logged_once_and_shown`.
- **Real account (checked 2026-11-19, `crates/pdmount/src/probe_parity.rs`):** none. Of 3 720 nodes decoded without the
  entity cache (My Files 780 folders and 2 801 files, the two folder shares 19 and 117 below their roots, Photos 3), no
  name author, node author or content author is unverified and none is degraded; this covers content key signatures and
  folders' hash keys written by the official clients (no integrity telemetry either). The account has no anonymous node,
  so those checks are exercised by the unit tests only.
- **0.5.1:** new. Branch `0.5/feat/node-verification` from `0.5/fix/undecryptable-node` (P26), whose converter it
  changes.

### P28 — Sign with the primary address key
- **Where:** `rs2/account.rs:132-142` (`get_address_primary_private_key`), `:258-318` (`convert_from_address_dto`),
  `sdk/node/draft.rs:58-74` (`signing_key_for`), new `sdk/account.rs` (`upload_signing_key`). Parity finding A4 (Medium),
  M6.0.4.8.
- **Problem:** drafts signed with the address's first key whose OpenPGP version is below 6, not its primary key, which
  the official SDKs sign with (`cs/client/cs/src/Proton.Drive.Sdk/Nodes/Upload/NewFileDraftProvider.cs:45`,
  `NewRevisionDraftProvider.cs:50`). `get_address_primary_private_key` (folders, renames, moves, devices) was not
  reliable either: `primary_key_index` counted every active key while the list it indexes holds only the keys that
  unlocked, so a key that failed to unlock shifted the "primary" onto another key, and a primary that did not unlock
  fell back to index 0. One address key token whose signature failed failed the whole address (`:280`). The key flag bits
  were swapped (`:268-269`): bit 1 is "not compromised" (may verify), bit 2 "not obsolete" (may encrypt)
  (`cs/incubating/account/cs/src/Proton.Drive.Sdk.Account/Api/Addresses/AddressKeyCapabilities.cs`).
- **Fix:** rs2 keeps an address's keys as the official account client does
  (`cs/incubating/account/cs/src/Proton.Drive.Sdk.Account/Addresses/AddressOperations.cs`, `ConvertFromDtoAsync`): only
  the active keys that unlock, `AddressKey` entries and private keys in the same order, and the primary's index among
  them (`address_keys`). A key that does not unlock, a bad token signature included, is left out with a warning. When the
  primary does not unlock, `primary_key_index` is -1 and `get_address_primary_private_key` fails instead of returning
  another key (the official client fails the whole address). The flag bits follow `AddressKeyCapabilities`. Drafts sign
  with the primary key (`upload_signing_key`); while the primary is a v6 key they keep upstream's guard and take the
  address's first v4 key, since Drive's commit endpoint still verifies v4 signatures (TODO.md Q5).
- **PdMount:** no code change; it uses none of these calls directly. An address whose primary key does not unlock now
  fails the changes that sign (`Other`, parked after five attempts; DESIGN Appendix A) instead of signing them with
  another key.
- **Test:** the primary is found among the unlocked keys when an earlier key does not unlock; a primary that does not
  unlock is no primary; the flag bits (the three failed with the old rules, checked by running them against the old logic
  in the same helper); uploads are signed with the primary key when it is not the address's first key.
- **Behaviour change:** `Address.keys` lists only the keys that unlocked, and `primary_key_index` indexes them (-1 when
  the primary did not unlock); `is_allowed_for_encryption` and `is_allowed_for_verification` change places. An address
  cached by an older version keeps its old index until it is fetched again.
- **Branch:** `0.5/fix/primary-signing-key` from `base/0.5.1`; it merges into `pdmount-0.5` after P11 and P21 without a
  conflict.
- **Real account (checked 2026-11-19):** the account has one address with one active key, v4 and primary, so uploads
  sign with the primary key and the v6 guard does not apply. Through the SDK (`crates/pdmount/src/probe_parity.rs`): a
  folder, an upload, a file rename, a move with a new name, a folder rename and a new revision pass, and read back with
  their name, node and content signatures verified. Through the mount: R1 (`VERIFY_MODE=live`, `R_FILES=30 R_BIG_MIB=1`)
  passes, its 31 uploads and 4 folders reach the server, and a folder rename, a file rename and a move into a new folder
  made on the mount afterwards drain with nothing queued and no WARN or ERROR in the daemon's log.

### P29 — Verify signatures at server time
- **Where:** `sdk/node/crypto.rs:462` (`decrypt_message`'s verifying decryptor) and P27's `decrypt_content_key`, P2's
  `verify_manifest_signature` (`node/download.rs`), `sdk/node/mod.rs:1495` (a membership's inviter signature) and P24's
  acceptance check, `rs2/account.rs:341` (`get_address_key_token_passphrase`); new `rs2/crypto_time.rs`. Security review
  finding V-8 (Low, `docs/security/2026-11-18-review/verification.md`), M6.0.7.
- **Problem:** keys and signatures are dated at server time (`server_time::now()`, `check_now()`: `sdk/crypto.rs:27`,
  `sdk/pgp.rs:50`, `:61`), but every verification used rPGP's default date, the device clock (`proton-rpgp` 0.3.1
  `src/verify.rs:55`), and rPGP refuses a signature created after that date with no tolerance (`src/signature.rs:77-85`).
  On a device whose clock is slow by N seconds, whatever was signed in the last N seconds, by this client or another,
  fails: a revision's manifest (its block map fails, EIO in PdMount, DESIGN §6.3), and names, passphrases, hash keys,
  content keys and memberships are reported unverified (P27; refused from M6.0.6 on).
- **Fix:** every verification takes `.at_date(crate::server_time::check_now())`. The sites, found with `git grep` for
  `Verifier`, `with_verification_key(s)`, `verify_` and `Decryptor` on `pdmount-0.5`:
  - `node/download.rs` `verify_manifest_signature` (P2), which block maps (P17) and full downloads use;
  - `node/crypto.rs` `decrypt_content_key`'s `Verifier` (`ContentKeySignature`, P27) and `decrypt_message`'s second
    decryptor, the one given verification keys: names, node passphrases, folder hash keys, extended attributes (through
    the telemetry wrapper), share passphrases and names (`sharing.rs`), URL passwords and `share_ops.rs`;
  - `node/mod.rs` a membership's inviter signature and its acceptance (P24);
  - `proton-sdk-rs2` verifies an address key token's signature with the user keys and has no server clock
    (`proton-drive-sdk` depends on it, not the reverse). New `proton_sdk_rs2::crypto_time`: `check_now()` reads a source
    installed once with `set_source`, else the device clock; `ProtonDriveClient` installs `server_time::check_now` when
    it is built.
  - Not changed: decryptors without verification keys (session keys, `pgp.rs:38`, `:277`; the content key's session
    key; a URL password's `sharing.rs:1773`); P20's upload check, which compares plaintext; `proton-srp`'s modulus
    signature check (`sharing.rs:848`, `RPGPVerifier::default()`), a third-party crate that takes no date, over a
    signature years old.
- **Official clients:** they verify at server time too. C# sets the PGP library's process-wide
  `PgpConfiguration.DefaultTimeProviderOverride` to `CryptographyTimeProvider`, the latest `Date` of any answer
  (`cs/client/cs/src/Proton.Sdk/Cryptography/CryptographyTimeProvisionHandler.cs:9-20`), which the library takes for its
  operations by default (the library is not in the checkout). The JS SDK passes no date to `verifyMessage`
  (`cs/client/js/src/crypto/openPGPCrypto.ts:220-255`), so the injected CryptoProxy's time applies, which Proton's web
  client keeps at server time (outside the SDK). The fork differs in which time: device time plus P19's bounded offset,
  not the latest `Date` seen.
- **Behaviour change:** a signature made up to server time verifies on a device whose clock is slow; one dated after
  server time still fails. Verification now moves with the offset, which P19's amendment (M6.0.7) bounds.
- **PdMount:** no code change. V-8's window (EIO for a revision signed in the last seconds of a slow clock) closes.
- **Tests:** `tests/verify_at_server_time.rs`, its own process since it moves the offset: with server time an hour ahead
  of the device clock, a manifest, a content key and a name signed at server time verify (all three failed before), and
  ones dated ten minutes after server time still fail. rs2: an address key token signed an hour ahead fails at the
  device clock and verifies once a source an hour ahead is installed; one signed after it fails.
- **Branch:** `0.5/fix/verify-at-server-time`, created from `base/0.5.1` and then based on `pdmount-0.5` at `e9e8c02`:
  three of its sites are other patches' own (P2's manifest check, P27's content key check, P24's acceptance check), and
  the test needs the first two. It merges after the amended P19 without a conflict.
- **0.5.1:** new.
- **Real account (later):** nothing new to see while the device clock is right; the usual reads, listings and shares
  still verify.

### P30 — Read a revision's block list by range
- **Where:** `node/file/block_map.rs` (P3, P17): new `ProtonDriveClient::get_block_page`, `verify_listed_block_hashes`,
  `listed_block_sizes`, pure `check_block_page` and `verify_block_hashes` (which `verify_block_listing` now calls).
  M6.0.11, D135.
- **Problem:** `get_block_map` lists every block of a revision, in pages of 10 one after the other
  (`DEFAULT_BLOCK_PAGE_SIZE`, `node/download.rs`), before any block can be read: 17 requests for a 650 MiB video, 126 for
  4.9 GiB, and the whole listing again for the next map. A reader that needs a few blocks of a large file (a player: the
  head, then the index at the end) waits for all of it. A caller cannot list by range and verify itself: the manifest's
  keys (`manifest_verification_keys`) and the decoding of a listed revision's block sizes
  (`decrypt_claimed_block_sizes`) are crate-private, and `get_revision` checks nothing of a page.
- **Fix:** `get_block_page(revision, from, page_size)` lists one page from any index and checks it with
  `check_block_page`: sorted by index (as C# sorts each page), the blocks from `from` on without a gap or a repeat,
  otherwise `DataIntegrityException` ("File contents are incomplete"); empty past the end. A page may hold fewer blocks
  than asked for without being the last. `verify_listed_block_hashes(revision, dto, hashes)` is P17's check over the
  hashes of pages read one by one, with the keys `get_block_map` uses. `listed_block_sizes(revision, dto)` reads the
  sizes as `get_block_map` does (the answer's XAttr, else the revision listing), now shared with it. `get_block_map`
  does not change.
- **Official clients:** the JS SDK lists in pages of 20 (`cs/client/js/src/internal/download/apiService.ts:7`,
  `iterateRevisionBlocks`, which pages until an empty page) and, for seeking, lists one block at its index
  (`getRevisionBlockToken`, `PageSize=1`) with offsets from the claimed block sizes (`fileDownloader.ts`
  `getSeekableStream`, `blockIndex.ts` `getBlockIndex`); C# lists pages of 10 (`RevisionReader.cs` `GetBlocksAsync`) and
  refreshes one block with `PageSize=1` (`GetBlockTransferTargetAsync`). Both verify the manifest after a whole download
  (`RevisionReader.cs` `VerifyManifestAsync`, `fileDownloader.ts` `internalDownloadToStream`); the JS seekable stream
  verifies neither the manifest nor the block hash.
- **Behaviour change:** none for existing callers.
- **PdMount:** `pdmount-backend/src/block_lists.rs` and the adapter (DESIGN §6.1): the layout from the node's claimed
  block sizes, a read's page in pages of 20, the other pages for the manifest beside the first download, the bytes once
  the manifest verified, a page listed again alone after 10 min; the thumbnail's reference is one page without URLs
  (`get_revision` with `NoBlockUrls`, DESIGN §14.5).
- **Tests:** a page is sorted and contiguous from its start; a gap, another start or a repeat is incomplete; indices
  start at 1; hashes from pages verify as the whole listing, and a tampered, missing or empty hash fails.
- **Branch:** `0.5/feat/block-list-range`, from `pdmount-0.5` at `10a4471` (on P3 and P17).
- **0.5.1:** new.
- **Real account (2026-11-19, M6.0.11):** a ~633 MiB MP4 with its index at the end, uploaded to `pdmount-test/` and read
  from My Files, its blocks wiped and the daemon fresh before each run: every page of 20 answered 20 blocks, 73–400 ms
  per page; first frame (mpv) 1.5–4.0 s, median 2.6 (7 runs), against 2.4–7.3 s, median 4.5 (9 runs) on `10a4471`; 10 s
  of playback 12.3 s against 14.1 s; a full copy 43–44 s against 36–51 s; no 429. The test account's Photos holds no
  video, so neither a Photos video nor a video thumbnail of 20 blocks or more was checked; the three photos' thumbnails,
  made again, were byte-identical.

## Upstream tracking

Branches on `base/0.5.1` (M6.0.1; pushed). The column "On 0.4" gives the commits of the old branches, which have
the same names without the `0.5/` prefix and stay in the fork.

| Patch | Fork branch | Commit | Based on | On 0.4 | Upstream | State |
|---|---|---|---|---|---|---|
| P1 | `0.5/fix/block-pagination` | `74ee66c` | `base/0.5.1` | `8048435` | after stable (D089) | merged into `pdmount-0.5` |
| P2 | `0.5/fix/block-verification` | `dc5b440` (M6.0.4.12; was `543b396`) | `base/0.5.1` | `09999b6` | after stable (D089) | merged into `pdmount-0.5` |
| P3 | `0.5/feat/block-map` | `60fc890` (M6.0.4.11; was `1ffe1c4`) | P1 + P2 | `5e169ab` | after stable (D089) | merged into `pdmount-0.5` |
| P4 | `0.5/fix/revision-xattr-decode` | `f3ff7d7` (M6.0.4.11; was `b974a6c`) | `base/0.5.1` | `9bc1ace` | after stable (D089) | merged into `pdmount-0.5` |
| P5 | `0.5/feat/folder-mtime` | `330b78a` | `base/0.5.1` | `03a4f1f` | after stable (D089) | merged into `pdmount-0.5` |
| P6 | `0.5/feat/upload-revision-uid` | `a71c08a` | `base/0.5.1` | `3cd9ead` | after stable (D089) | merged into `pdmount-0.5` |
| P7 | `0.5/feat/transfer-concurrency` | `18cf2c0` | `base/0.5.1` | `d46e7a2` | after stable (D089) | merged into `pdmount-0.5` |
| P8 | `0.5/feat/typed-api-error` | `a32d51e` | `base/0.5.1` | `af6ec76` | after stable (D089) | merged into `pdmount-0.5` |
| P9 | `0.5/fix/token-refresh-errors` | `1f84a7c` | `base/0.5.1` | `152a028` | after stable (D089) | merged into `pdmount-0.5` |
| P10 | `0.5/feat/move-with-rename` | `846d99c` | `base/0.5.1` | `8c525e6` | after stable (D089) | merged into `pdmount-0.5` |
| P11 | `0.5/feat/revision-conflict-error` | `d533612` | P8 | `7cacdae` | after stable (D089) | merged into `pdmount-0.5` |
| P12 | `0.5/feat/metadata-retry` | `316b827` | P8 | `ca42587` | after stable (D089) | merged into `pdmount-0.5` |
| P13 | `0.5/feat/fresh-node-batch` | `223ae6a` (M6.0.4.3; was `f97022a`) | `base/0.5.1` | `f245e14` | after stable (D089) | merged into `pdmount-0.5` |
| P14 | `0.5/feat/caller-name-hash` | `dcfc93e` | `pdmount-0.5` at `720f6e9` (needs P10, P13) | `0389db3` (on `pdmount` at `7c92076`) | after stable (D089) | merged into `pdmount-0.5` |
| P15 | `0.5/fix/link-trash-time` | `b7433ac` | `base/0.5.1` | `0459ebb` | after stable (D089) | merged into `pdmount-0.5` |
| P16 | `0.5/fix/no-secret-logs` | `1ea0985` (M6.0.4.2) on `a1fb48a` (squashed) | `pdmount-0.5` at `bb73ff8` (after P15) | `9290db0`, `6c6617f` (rustfmt; on `pdmount` at `c837e03`) | after stable (D089) | merged into `pdmount-0.5` |
| P17 | `0.5/feat/block-map-manifest` | `5ea7020`, `7777eea` (keys decoding) | `pdmount-0.5` at `4bca0d5` (after P16) | `6f6276d`, `5dd08b3` (on `pdmount` at `981bb1b`) | after stable (D089) | merged into `pdmount-0.5` |
| P18 | `0.5/feat/block-parallelism` | `a3065fe` | `pdmount-0.5` at `f0b77c4` (needs P7) | `faf4691` (on `pdmount` at `631b4aa`) | after stable (D089) | merged into `pdmount-0.5` |
| P19 | `0.5/fix/server-time-jitter` | `c368c40` (M6.0.7; was `3df2984`, `1f6dfac`) | `base/0.5.1`, with `0.5/feat/metadata-retry` (P12) merged in M6.0.7 | — (new in 0.5) | after stable (D089) | merged into `pdmount-0.5` |
| P20 | `0.5/fix/block-upload-verification` | `5b303af` (M6.0.4) | `base/0.5.1` | — (new in 0.5) | after stable (D089) | merged into `pdmount-0.5` |
| P21 | `0.5/feat/small-file-upload` | `a9e3dbe` (M6.0.4) | P23 with P20 merged in (`c532eca`) | — (new in 0.5) | after stable (D089) | merged into `pdmount-0.5` |
| P22 | `0.5/fix/commit-not-in-draft` | `aae3b9a` (M6.0.4) | P8 | — (new in 0.5) | after stable (D089) | merged into `pdmount-0.5` |
| P23 | `0.5/feat/intended-upload-size` | `5086c0a` (M6.0.4) | `pdmount-0.5` at `55adc5f` | — (new in 0.5) | after stable (D089) | merged into `pdmount-0.5` |
| P24 | `0.5/feat/membership-acceptance` | `64b9ccc` (M6.0.4) | `base/0.5.1` | — (new in 0.5) | after stable (D089) | merged into `pdmount-0.5` |
| P25 | `0.5/fix/share-single-get` | `86f783b` (M6.0.4) | `base/0.5.1` | — (new in 0.5) | after stable (D089) | merged into `pdmount-0.5` |
| P26 | `0.5/fix/undecryptable-node` | `fc88ea3` (M6.0.4) | `pdmount-0.5` at `55adc5f` (needs P13) | — (new in 0.5) | after stable (D089) | merged into `pdmount-0.5` |
| P27 | `0.5/feat/node-verification` | `d4bd6af` (M6.0.4) | P26 | — (new in 0.5) | after stable (D089) | merged into `pdmount-0.5` |
| P28 | `0.5/fix/primary-signing-key` | `d39cbdb` (M6.0.4) | `base/0.5.1` | — (new in 0.5) | after stable (D089) | merged into `pdmount-0.5` |
| P29 | `0.5/fix/verify-at-server-time` | `f18062e` (M6.0.7) | `pdmount-0.5` at `e9e8c02` (needs P2, P24, P27) | — (new in 0.5) | after stable (D089) | merged into `pdmount-0.5` |
| P30 | `0.5/feat/block-list-range` | `f33d65e` (M6.0.11) | `pdmount-0.5` at `10a4471` (needs P3, P17) | — (new in 0.5) | after stable (D089) | merged into `pdmount-0.5` |

Integration: `pdmount-0.5` = `base/0.5.1` + all branches above, merged with `--no-ff` in the old order (P8, P11, P12, P3
with P1 and P2, P4, P5, P6, P7, P9, P10, P13, P14, P15, P16, P17, P18, then P19; "Merge 0.5/<branch> into pdmount-0.5").
After P10 it also contains `9df873e` (was `8c7ff8b`), the test-only fix that sets P5's `FolderNode` field in a test helper
added by P10. Tip `70f10ff` (2026-11-18, P19 merged on `0f26291`), the planned pin `pin/70f10ff`. Checks: `cargo test -p
proton-drive-sdk -p proton-sdk-rs2` passes on each of the 19 branches and on `pdmount-0.5` (268 tests, 166 on
`base/0.5.1`); `cargo test --workspace --all-features` passes on `pdmount-0.5` (352); clippy warns less than on the base
(81 against 90; P19 adds none). PdMount builds against it without a code change: nothing it uses changed its signature.
All 18 patches carried over from 0.4 are still needed; none was merged upstream.
M6.0.4 (2026-11-18): the amended P16 and P13 merged again (`294c75e`, `55adc5f`), pin `pin/55adc5f`; `cargo test -p
proton-drive-sdk -p proton-sdk-rs2` passes on it (274 tests), and PdMount's full `scripts/ci.sh` passed against it.
M6.0.4.5–M6.0.4.16 (2026-11-19): after `55adc5f`, merged with `--no-ff` in this order: P20, P23, P21, P28, P26, P27,
P4, P3, P2 (each amended branch merged again), P24, P25, P22, P19 (amended). Tip `e9e8c02`, no conflicts. `cargo test -p
proton-drive-sdk -p proton-sdk-rs2` passes on each branch and on the tip (329 tests, 1 ignored), and PdMount's full
`scripts/ci.sh` passed against the tip through a local override; PdMount also builds against the pin `55adc5f`, as it uses
no new fork API. Pushed on 2026-11-19 with tag `pin/e9e8c02`.
M6.0.7 (2026-11-19): P19 amended (its branch now also carries P12, merged to resolve the retry middleware) and P29,
merged with `--no-ff` in that order on `e9e8c02`. Tip `10a4471`, no conflicts. `cargo test -p proton-drive-sdk -p
proton-sdk-rs2` passes on each branch and on the tip (341 tests, 1 ignored); PdMount's full `scripts/ci.sh` passed
against the same tree through a local override (M6.0.6–M6.0.9 together), and a real-account check (`doctor`, R1, 27
fresh blocks read) found no warning. PdMount's `doctor` uses the new `server_time::offset_seconds`, so it needs this
pin. Pushed on 2026-11-19 with tag `pin/10a4471`.
M6.0.11 (2026-11-19): P30 merged with `--no-ff` on `10a4471`. Tip `19cc34d`, no conflicts. `cargo test -p
proton-drive-sdk -p proton-sdk-rs2` passes on the branch and on the tip (344 tests, 1 ignored); PdMount's full `scripts/ci.sh` passed
against the same tree through a local override (M6.0.10 and M6.0.11 together). PdMount uses P30's new functions, so it
needs this pin. Pushed on 2026-11-19 with tag `pin/19cc34d`, which PdMount pins.

On 0.4 (history): `pdmount` = `base/0.4.0` + the branches without the prefix, merged with `--no-ff`. It also contains
`8c7ff8b`, the test-only fix above. Checks on 2026-10-19: 236 SDK tests pass (baseline 150), pdcli builds and its tests
pass, and `cargo fmt --check` passes. The fork's own tests are all offline. P13 was merged on 2026-10-21 (`7c92076`, pinned
by PdMount); the fork's workspace tests (278) pass on it. Through PdMount, the patches ran against the live API in the M2.8
real-account matrix (60 of 60 runs passed, 2026-10-22). P14 (`df38c89`) and P15 (`c837e03`, pinned by PdMount) were merged
on 2026-10-22; the fork's workspace tests (281) pass, and both ran against the live API in the M3.1 probe. P16 was merged
on 2026-10-27 (`946cf96`; its rustfmt follow-up `981bb1b`); the fork's workspace tests (281) pass. P17 was merged on
2026-10-27 (`9037d6b`, then `631b4aa` with the keys-decoding fix); the fork's workspace tests (288) pass. P18 was merged on
2026-10-28 (`caafaea`, pinned by PdMount); the fork's workspace tests (290) pass.

## Behaviour changes

These changes differ from upstream and can affect existing callers such as pdcli. Approved by the user against 0.4.0 on
2026-10-21 (P2, P8, P9, P11, P12; P13 only adds an API and changes no existing behaviour) and on 2026-10-22 (P15; P14 only
adds API); they still hold against 0.5.1, except where the P9 row says otherwise:

| Patch | Change |
|---|---|
| P2 | A full download (`download_to_stream`) of a revision whose manifest signature is missing or cannot be verified now fails with `CompletedDownloadManifestVerificationException` after the content has been written, as in the C# SDK. A block hash mismatch is fetched once more, as the JS SDK does, and then fails with `DataIntegrityException`; a block missing after its target refresh fails with it too (M6.0.4.12). The uploader now orders thumbnail digests by type in the signed manifest, as C# does. |
| P8 | Errors keep the `"API error {code}: {message}"` / `"HTTP {status}: …"` wording. `ApiResponse::to_result` changed from `"API error: code X, message: Some(..)"` to `"API error X: …"`. Endpoints that ignored the HTTP status now return `ApiError` instead of a JSON parse error. |
| P9 | A failed token refresh returns `Err(TokenRefreshError)` instead of `Ok(old tokens)`. `refresh_token_expired` fires on Proton code 10013. `tokens_refreshed` no longer fires after a failure. Upstream does the last two since 0.5.0 (by matching the error text), so against 0.5.1 only the `Err` differs. |
| P11 | New-file drafts send `ClientUID`. With `override_existing_draft_by_other_client = true`, another client's draft that never got a committed revision is deleted and the create retried. Failures are `NodeWithSameNameExistsError` / `RevisionDraftConflictError`. |
| P12 | Drive API requests are retried: 429 for all methods; 5xx and timeouts for idempotent methods only; connection failures. Circuit breakers fail fast for 60 s after 50 consecutive 429s or 10 consecutive 5xx. |
| P17 | `get_block_map` fails for a revision whose manifest signature is missing or does not verify over the listed hashes, or that lists a block without a hash (2026-10-27, accepted with the M4.0.1 fix plan). `get_address_public_keys` now returns the address's non-compromised keys instead of failing, so authorship and manifest checks of address-signed nodes run for the first time. |
| P15 | Trashed nodes report their trash time (`trash_time`) in `get_node`, `get_nodes_uncached`, folder listings and the trash listing; before, it was always `None`. |
| P19 | The server time offset changes only by 2 s or more; a one-second change is ignored (2026-11-18, requested by the user). Amended (M6.0.4.16): the crypto time does not go back by up to 60 s; a larger step back logs a WARN. Amended (M6.0.7, V-9, user): the offset is taken from the Drive API host's answers only (not block storage, public links or `create_client_with_timeout`; `client_with_server_time` takes the API base URL); the first must be within a day of the device clock, else it is refused with a WARN; a later change of more than 60 s waits for a second answer that agrees; `offset_seconds()` reads it. |
| P20 | Each block is decrypted with the verification session key and compared with its plaintext before upload; a block that fails twice fails the upload with `IntegrityException` (proposed, M6.0.4.5). `verify_block` takes the whole block, not prefixes. |
| P21 | None with the default feature flag provider. With `DriveSmallFileUpload` on, uploads under 128 KiB (with thumbnails, × 1.1) go up in one request, falling back to a draft only before the request or on a clean conflict; `RevisionDraftProvider` gains `get_small_upload`, `FilesApiClient` two methods (proposed, M6.0.4.7). |
| P22 | A commit answered 2511 succeeds when the revision is already active or obsolete (one more `GET` of the revision); a 2xx commit answer with an error `Code` now fails the commit. |
| P23 | Drafts send `IntendedUploadSize` (a new file's rounded down to 100 000 bytes, none at or below that; a revision's exact); `RevisionDraftProvider::get_draft` takes the size (proposed, M6.0.4.6). |
| P24 | A shared node's membership carries `accepted_by`; decoding a shared node also fetches the accepting address and its keys. A membership whose inviter or acceptance signature does not verify logs a WARN once per node and field. |
| P25 | A share is fetched once to decrypt it, and not at all for the My Files and Photos roots. The public `ShareCrypto::decrypt_share` fetches the share before decrypting, not after, and its result takes the membership address it was given. |
| P28 | Drafts sign with the address's primary key (the first v4 key while the primary is v6); `Address.keys` lists only the keys that unlock and `primary_key_index` indexes them, -1 when the primary did not unlock, which makes `get_address_primary_private_key` fail; the key flag bits are swapped back (proposed, M6.0.4.8). |
| P29 | Signatures are verified at server time (`server_time::check_now()`), as they are made, not at the device clock; `proton-sdk-rs2` verifies address key tokens at the time `crypto_time::set_source` installs, which `ProtonDriveClient` sets (M6.0.7, V-8, user). |

## Upstream 0.5.0 and 0.5.1

What the rebase onto 0.5.1 (M6.0.1) found in upstream's changes since 0.4.0, apart from the patches' 0.5.1 lines above.

**Findings (DESIGN.md §2.2):** F2 is fixed upstream (P1); F8 in part (P9). F1, F3–F7, F9–F11, F12 (events) and F13 (login)
are unchanged. F14: the new `get_node_hierarchy` reads through the entity cache too.

**Behaviour changes that reach PdMount** (no PdMount code change was needed):
1. A failed upload write deletes its draft: a new file's draft link through `delete_multiple`, a new revision through
   `delete_revision` (`sdk/node/file/upload.rs:196-213`, `sdk/node/draft.rs:167`, `:230`). A failed commit keeps it, since
   it may have been applied. This matches the official SDK; a retry gets a fresh draft instead of its own leftover.
   PdMount's code 2511 ("not in draft") at commit stays transient.
2. Signing, encryption and key generation are dated by server time: a process-wide offset taken from the `x-pm-date`
   (else `Date`) header of the SDK clients' responses (`sdk/server_time.rs`). PdMount's own HTTP clients
   (`TimelineClient`, the probe's) do not update it. Upstream adopts every response's offset, so one-second jitter could
   date a signature before its new key; P19 ignores changes under 2 s.
3. Storage upload retries also cover a connection dropped mid-request.
4. `delete_device` reports API errors.
5. Block uploads report progress as the bytes are sent.

**New API PdMount may use later** (none is used yet):
- `enumerate_shared_nodes`, `enumerate_shared_nodes_with_me` (`sdk/client.rs:1384`, `:1393`).
- `get_node_hierarchy` (`sdk/client.rs:716-738`): a node and its ancestors, one cached `get_node` each.
- `copy_nodes` (`sdk/client.rs:1153`).
- `get_event_scheduler` (`sdk/client.rs:1261`) and `EventScheduler` (`sdk/event_scheduler.rs`), the official clients'
  polling cadence (DESIGN.md §9.2).
- `FileDownloader::open_block_reader` (`sdk/node/file/download.rs:218`; see P3).
- `error::is_transient_error` (`sdk/error.rs:197`; P8 extends it).
- Photos `get_timeline_hash_page` (`sdk/photo.rs:515`).

**Official SDK baseline:** upstream synced with the official SDK at `8a44654a` (upstream `900107c`, "sync upstream SDK
v0.22.2"; its `docs/UPSTREAM_SYNC.md`). By changelog that is `js/v0.22.2`; the commit also carries the tag `js/v0.23.0`,
which had no changelog entry, and its changes were ported by commit, except the incubating search engine and what does
not apply to Rust (M6.0.2 starts from there). On the crypto migration upstream adds nothing: its `docs/RELEASE.md` keeps
the note of 0.4.0 (a compatibility boundary expected late 2026 or early 2027), with no plan (TODO.md Q5).

## Known gaps after M0.2

- **P3:** `download_range` builds a `BlockMap` on every call. PdMount keeps its own map per revision instead (DESIGN.md §6.1).
- **P4:** `Media` is not copied into `additional_claimed_metadata`. Anonymous revisions are not signature-checked. (Legacy block-size ordering is applied since M6.0.4.11.)
- **P9:** `DefaultAuthenticationApiClient` still hard-codes `https://drive-api.proton.me/` for the auth endpoints. The Drive API clients still do not refresh on a 401; the PdMount backend adapter does that (DESIGN.md §12.3).
- **P10:** no name validation on rename or move (the C# SDK has `ValidateNodeName`). PdMount validates names itself (DESIGN.md §8.8).
- The legacy, unused `node::download::BlockDownloader` has no hash check.
