## Area C3 (upstream SDK changes 0.4.0 → 0.5.1 that PdMount now runs): findings

Reviewed read-only: PdMount at `9818aa1` (`Cargo.toml`, `Cargo.lock`, `crates/pdmount-backend/src/proton.rs`,
`crates/pdmount/src/main.rs` log filter, `crates/pdmount-core/src/cache/block_cache.rs`); the SDK fork
`git log base/0.4.0..base/0.5.1` and `git diff base/0.4.0 base/0.5.1 -- crates/proton-drive-sdk crates/proton-sdk-rs2`
(with `-w --ignore-cr-at-eol` for `photo.rs`, whose 2777 changed lines are mostly line endings), and the same files at the
merged `pdmount-0.5` (`70f10ff`) where a patch touches them: `server_time.rs` (P19), `api/storage.rs` (no patch),
`node/file/upload.rs` (P6), `node/draft.rs` (P11), `node/revision.rs`, `block/upload.rs`, `error.rs`, `client.rs`,
`http/retry.rs` (P12), `utils/thumbnail.rs`, `rs2/auth.rs` (P9). Dependencies read: `proton-rpgp` 0.3.1 (`types.rs`,
`signature.rs`, `key/certifications.rs`, `key/selection.rs`, `decrypt/message.rs`, `sign.rs`, `verify.rs`), `reqwest`
0.13.5 (`error.rs`, `async_impl/multipart.rs`). Official SDK at `cs/v0.33.0`: `cs/cli/src/api/serverTimeHook.ts`,
`cs/cli/src/api/index.ts`, `cs/cli/src/api/httpClient.ts`, `cs/client/js/src/internal/upload/{manager,fileUploader,apiService}.ts`.

Not covered: `crates/pdcli*`; the patches themselves (area 2); the new `docs.rs`, `search.rs`, `event_scheduler.rs`,
`easy_switch.rs` beyond confirming that PdMount does not call them; `@protontech/crypto` (not in the official checkout,
so how the official clients date their verification was not read); nothing was built or run. Not repeated: V-21
(`Retry-After` in PdMount's waits), `docs/sdk-patches.md` P16 "Left as is" (response bodies in decode errors) and TODO
"Known gaps".

Nothing Critical, High or Medium. Two Low, two Info.

### C3-1: Signatures and keys are dated by the server clock but verified against the device clock
- **Rating**: Low
- **Location**: signing and key generation at server time: `fork:crates/proton-drive-sdk/src/server_time.rs:64-76`,
  `crypto.rs:27` (`generate_private_key`), `pgp.rs:50`, `:61` (`sign_detached_armored`, used for the manifest and the
  content key signature), `block/upload.rs:49-63`, `node/crypto.rs:65-187`, `node/draft.rs:230`, `node/revision.rs:785`,
  `:942`. Verification at device time, unchanged from 0.4: `fork:crates/proton-drive-sdk/src/node/download.rs:164`
  (`verify_manifest_signature`: `Verifier::default()`), called from `node/download.rs:765` and
  `node/file/block_map.rs:272` (P17); `node/mod.rs:1579`; `node/crypto.rs:297`, `:453`, `:462`; `pgp.rs:38`, `:277`.
  `proton-rpgp` 0.3.1: `verify.rs:55` and `sign.rs:65` (default date `CheckUnixTime::enable_now()`, the system clock,
  `types.rs:179-181`), `signature.rs:77-85` (`date < creation` → `FutureSignature`, no tolerance),
  `key/certifications.rs:59-63` (the same for a key's self-certification). PdMount:
  `crates/pdmount-backend/src/proton.rs:717-723` (`CompletedDownloadManifestVerificationException` → `Integrity`),
  `crates/pdmount-core/src/cache/block_cache.rs:551-556` (error logged, block not cached, the read fails).
- **Actor**: none (T5 at most: the user's own clock); asset A4, availability only.
- **Scenario**: The machine's clock is 90 s slow (no NTP, a dead RTC battery, a VM resumed from pause). The first Drive
  response sets the offset to +90. PdMount uploads a file at device time D: the node key, the content key signature, the
  block signatures and the manifest signature are dated D+90. Until the device clock reaches D+90, every check of that
  revision on this machine uses device time and fails: the manifest signature ("Cannot verify the author uploaded this
  file: … Signature has a creation time in the future") and anything verified with the new node key. A block-map read
  or block fetch of the file in that window (a cache miss, a wiped cache, a second profile or a remount with
  `VERIFY_MODE=fresh`-like state) returns `BackendError::Integrity`, which the FUSE layer answers with EIO. With 0.4 the
  same machine dated its signatures D and could verify them at once. The size of the window equals the clock difference,
  so a machine that is an hour slow cannot read back its own uploads from the drive for an hour.
- **Impact**: Transient read failures of the user's own fresh uploads on a machine with a slow clock, reported as an
  integrity failure ("block failed verification" at ERROR), which misleads. No data is lost or accepted wrongly: the check
  fails closed and passes once the clock has caught up. Authorship checks of names and passphrases degrade in the same
  window but are not fatal. A machine with a fast clock is better off than with 0.4 (that is what upstream fixed).
- **Fix**: Verify at the same clock as signing: give the fork's verification sites the server time
  (`Verifier::default().at_date(server_time::check_now())`, and `Decryptor::…at_date(…)` where a verification key is
  set), as one patch next to P19. Until then, make `doctor`'s clock line (`crates/pdmount/src/doctor.rs:284-340`) say
  that a slow clock makes fresh uploads unreadable for as long as the difference.
- **Confidence**: high for the code path (rPGP's comparison and both defaults were read); the read-back window was not
  run against the account. Whether the official clients verify at server time was not checked (see the header).

### C3-2: Any single response of any SDK client sets the process-wide crypto clock, without a bound
- **Rating**: Low
- **Location**: `fork:crates/proton-drive-sdk/src/server_time.rs:27-33` (`update`: the observed offset replaces the
  stored one whenever it differs by 2 s or more, P19), `:44-54` (`x-pm-date`, else `Date`), `:83-93` (the middleware runs
  on every response, whatever its status or host); attached to the Drive API client and the storage client
  (`client.rs:198-201`, `:298-301`), the Photos client (`photo.rs:131`), the public-link clients (`sharing.rs:917`,
  `:921`) and `http.rs:70`. Block URLs are absolute URLs from the block listing, requested with the storage client
  (`api/storage.rs:142-146`, `:219-222`). Official SDK: `cs/cli/src/api/serverTimeHook.ts:12-23`, on the one `ApiClient`
  that also carries blob requests (`cs/cli/src/api/httpClient.ts:20-28`), so upstream matches the official behaviour.
- **Actor**: T3 for the delay case; otherwise a Proton host with a wrong clock (a compromised server is out of scope).
- **Scenario**:
  1. *Source.* A storage node answers with `Date` only (no `x-pm-date`). If its clock differs from the API's by 2 s or
     more, each block response moves the offset one way and each API response moves it back. Nothing restricts the
     clock to the API host, and a redirect's final response counts too.
  2. *Latency read as skew.* The observed offset is the server's stamp minus the device time at receipt, so a response
     whose headers arrive 2 s or more after they were stamped lowers the offset by its delay. A network attacker
     can hold a response without breaking TLS, up to the request timeout (30 s on the API, 300 s on storage), and so
     backdate PdMount's signatures and keys by up to five minutes; the next prompt response moves the clock forward
     again. A downward step that lands between `generate_private_key` and the content key signature made with that key
     (`node/draft.rs:196-234`, a window of milliseconds) fails the draft with rPGP's "creation time in the future", the
     case P19 removed for one-second jitter.
  3. *No bound.* A header dated years away is adopted as is (`now()` clamps only below zero, and rPGP truncates to
     32 bits). A far-past date makes every signature fail (the address key is "in the future"); a far-future date
     produces keys and signatures the server or other clients reject, and on this machine C3-1 for everything written
     meanwhile.
- **Impact**: Upload and folder-creation failures that PdMount retries; with 0.5 each failed write also deletes its
  draft. Dates in signatures can be moved back by a few minutes by a network attacker, which has no security meaning
  I can show (nothing in PdMount or the server compares them against a policy). No path to a wrong signature being
  accepted. Rated as hardening.
- **Fix**: In the fork, next to P19: take the time only from the Drive API client's responses (drop the middleware from
  the storage and public-link clients, or pass the host); treat the header as a lower bound on the offset and lower the
  stored offset only after several consecutive responses agree (latency can only make the observation too low);
  refuse an offset beyond a sanity limit such as 24 h with a WARN, and show the current offset in `doctor`.
- **Confidence**: high for the code; medium for case 1 (what the storage hosts send was not observed).

### C3-3: PdMount builds the SDK with its default features, so the thumbnail generator and `image`'s decoders stay in the binary
- **Rating**: Info
- **Location**: `Cargo.toml:47` (`proton-drive-sdk = "0.5"`, default features on), `crates/pdmount/Cargo.toml:32`
  (adds `cache-encrypted`); `fork:crates/proton-drive-sdk/Cargo.toml` `[features]` (`default = ["cache-sqlite",
  "thumbnail-generation"]`); `fork:crates/proton-drive-sdk/src/utils/thumbnail.rs:9-12` (`image::load_from_memory`),
  `:44-45` (the 0.5 change: `to_rgb8()` before the JPEG encoding); `Cargo.lock` `image` 0.25.10 with `exr`, `gif`,
  `tiff`, `qoi`, `ravif`, `zune-jpeg`.
- **Actor**: none today.
- **Scenario**: The 0.5 thumbnail change is reachable only through `ProtonDriveClient::upload_file`
  (`fork:crates/proton-drive-sdk/src/client.rs:1095-1120`), which PdMount never calls: it uploads through
  `get_file_uploader`/`get_file_revision_uploader` with an empty thumbnail list (`crates/pdmount-backend/src/proton.rs:889-896`).
  Feature unification still compiles `image` with the fork workspace's `png`, `jpeg`, `webp` and `image`'s default
  formats for PdMount's own `image` dependency, although `Cargo.toml:34` asks for `default-features = false` and the
  threat model says "without formats". PdMount's code uses only `image_webp` and the scaling types, so no generic
  decoder is reached. Unchanged from 0.4 (the lock file gained no `image` dependency in `9818aa1`).
- **Impact**: None now; a later `image::load_from_memory` on a remote thumbnail would accept every format without a
  build change showing it.
- **Fix**: `proton-drive-sdk = { version = "0.5", default-features = false, features = ["cache-sqlite"] }` plus
  `cache-encrypted` where it is set, if PdMount needs the SQLite repositories; regenerate `THIRD-PARTY-LICENSES.html`.
  Or correct the threat model's WebP/PNG paragraph.
- **Confidence**: high for the feature sets and the call sites; that `cache-sqlite` is the only default PdMount needs
  was not built.

### C3-4: Info bundle on the new upload paths
- **Rating**: Info
- **Location and content**:
  - *A draft made but not returned is not cleaned up.* `fork:crates/proton-drive-sdk/src/node/draft.rs:277-290`, `:374-381`:
    when the block verifier cannot be created after `create_file`/`create_revision` succeeded, `get_draft` fails and
    the 0.5 clean-up (`node/file/upload.rs:233-247`) never learns the draft's uid. The official SDK deletes the draft in
    that case (`cs/client/js/src/internal/upload/fileUploader.ts:197-214`, `:268-285`). P11 removes the leftover on the
    retry (same client uid), so only the name stays taken until then. The same holds when PdMount drops the upload
    future while blocks are being written.
  - *The clean-up result is only logged.* `node/file/upload.rs:242-244` logs the upload error and the clean-up error at
    WARN with `%`. Both are SDK error texts, which can carry a storage reply's body (`api/storage.rs:186`, `:193`, `:245`;
    P16 "Left as is"), unescaped and unbounded. No token or key: the storage token and the session travel in headers,
    and the block URL in a reqwest error is the bare URL.
  - *`Retry-After` on a storage 429 is slept in full.* `api/storage.rs:155-157` (`sleep(wait)` with any `u64` of seconds,
    then the backoff; unchanged since 0.4). It does not panic (tokio saturates), but the upload holds its permits for as
    long as the storage host says; V-21 covers PdMount's own waits.
  - *Storage retries.* `api/storage.rs:198` now also retries `is_request()`/`is_body()`: at most 4 sends of the same block
    to the same URL and token, the body rebuilt from the shared `Bytes` each time; a block that arrived although its
    reply was lost is sent again, and a refusal of the repeat fails the upload, after which the draft is deleted and
    PdMount starts over. No other request is replayed there.
- **Actor**: T3 (a cut connection), the storage host.
- **Impact**: Leftover drafts and log size; no integrity effect (the commit carries the manifest and the server checks
  the block hashes).
- **Fix**: Delete the draft inside `get_draft` when the verifier fails (fork, P11's area); cap the slept `Retry-After`
  (for example at the request timeout) and return `TooManyRequestsException` with the full value instead; truncate
  reply bodies in the transfer errors as `rs2/auth.rs` does (2048 bytes).
- **Confidence**: high.

## Checked and fine
- **Draft clean-up target.** The deleted uid is always the one returned by this call's own successful `create_file`
  (`node/draft.rs:277-281`, also after P11's delete-and-retry loop) or `create_revision` (`:374-375`); a new file is
  removed with `delete_multiple` on that link, a new revision with `delete_revision` on that revision id
  (`:303-316`, `:394-396`, `client.rs:1022-1037`), the endpoints the official SDK uses
  (`cs/client/js/src/internal/upload/apiService.ts:280-299`). The clean-up runs only when `write_draft` failed, never
  after `commit` was attempted (`node/file/upload.rs:248-249`), so a commit whose reply was lost keeps its node. I found
  no path where the uid names an existing committed node or another client's draft.
- **PdMount's retries and 2511.** `upload_new`/`upload_revision` rerun the whole closure under `authed`, so each attempt
  gets a draft of its own; 2511 at commit stays `Transient` (`crates/pdmount-backend/src/proton.rs:803`, `:818-819`) and
  cannot be caused by the new clean-up of the same attempt, which does not run after a commit attempt. A second
  clean-up of a draft P11 already removed answers "not found" and is logged only.
- **Clean-up when offline.** One extra POST with the 30 s API timeout; P12 does not retry a timed-out POST, and connection
  failures are bounded (`http/retry.rs:127-128`).
- **Upload progress.** `counted_body` slices the shared `Bytes` in 64 KiB pieces without copying, with a known length
  (`api/storage.rs:89-103`, `:133-136`); 0.4 copied each block into a `Vec`. `UploadProgress` holds one map entry per
  block in flight and releases its lock before calling the callback (`node/revision.rs:510-522`), so a panicking
  callback cannot poison it; PdMount passes a no-op callback.
- **Storage retry bounds.** `MAX_RETRIES = 3`, backoff 0.5–2 s plus jitter (`error.rs:417-426`), 300 s per attempt.
- **Auth refresh.** Upstream 0.5's text match for `"Code":10013` and its `log::warn!("session refresh failed: {error:#}")`
  are not in `pdmount-0.5`: P9's typed `RefreshSessionError`/`TokenRefreshError` replaces them
  (`fork:crates/proton-sdk-rs2/src/auth.rs:117-151`, `:370-412`, `:672-702`). What is logged at WARN is the status, the
  Proton code and the server's `Error` text, or a reqwest error with the refresh URL; never the request (which holds
  the refresh token) or a successful reply. A failed refresh no longer publishes the old token pair.
- **New log calls in 0.5.x.** Three in the diff: the `instrument` on `upload_encrypted_block` (skips the block and the
  callback), the draft clean-up WARN (C3-4) and the refresh WARN (replaced by P9). PdMount's filter still drops
  `proton_sdk_rs2` records above WARN (`crates/pdmount/src/main.rs:726-756`).
- **Server time arithmetic.** `saturating_sub` for the observation, `fetch_update` for concurrent responses, `max(0)`
  before the cast; header values that do not parse are ignored; the rs2 account and auth clients carry no middleware,
  and neither do PdMount's own clients (`TimelineClient`, `doctor`).
- **Encryption at server time.** rPGP's default profile allows encryption to keys dated in the future
  (`profile/settings.rs:204`), and decryption-key selection ignores dates (`decrypt/message.rs:104-105`), so only
  signature and key verification are date-sensitive (C3-1).
- **Not called by PdMount** (grep over `crates/`): `get_available_name`, `delete_album`/`save_to_timeline`,
  `is_transient_error`, `get_node_hierarchy`, `copy_nodes`, `get_event_scheduler`, `init_search`/`raw_api_request`,
  `open_block_reader`, `prepare_import_folder`, `delete_device`, `get_timeline_hash_page`, `ThumbnailGenerator`,
  the public-link clients. The Photos root reads through `ProtonPhotosClient` and PdMount's own `TimelineClient`.
- **Dependencies.** `Cargo.lock` at `9818aa1` against its parent: the two SDK crates move to 0.5.1 at `70f10ff`;
  `reqwest` gains `tokio-util` (already in the tree) and `wasm-streams` 0.5.0 (the `stream` feature; wasm targets only,
  not built on Linux); 607 packages against 606. `yrs` is not in the lock file and `flate2` was there before (`pgp`,
  `png`, `tiff`), so the `docs` feature is off.
- **Block listing.** Upstream's new paging and `ensure_contiguous_blocks` are superseded by P1 at `pdmount-0.5` (area 2).
