## Area C2 (the SDK fork's patches P1–P19 as rebased onto upstream v0.5.1; M6.0.3): findings

Reviewed read-only. PdMount at `9818aa1` (it pins the fork at `70f10ff`, `Cargo.toml:81-82`). Fork: `git diff base/0.5.1 pdmount-0.5 --
crates/proton-drive-sdk crates/proton-sdk-rs2` (`base/0.5.1` = `e2e45bb`, `pdmount-0.5` = `70f10ff`; 39 files, about 5 000 added
lines), read in full except the test modules. Rebase fidelity: `git range-diff base/0.4.0..pdmount base/0.5.1..pdmount-0.5` (all 21
commit pairs) and `git show --cc` of the 18 merges on `pdmount-0.5`. PdMount's use of the patched API:
`crates/pdmount-backend/src/proton.rs` (`with_reauth`, `block_map`, `download_block_into`, `classify`/`map_api_error`,
`get_nodes_checked`, `thumbnail_ref`, `download_thumbnail`, the uploads), `crates/pdmount/src/auth.rs` and `session.rs` (login,
refresh), `crates/pdmount-core/src/events.rs` and `poller.rs` (where a failed `get_nodes` ends up). Upstream code was read where
a patch relies on it: `sdk/api/storage.rs`, `sdk/server_time.rs`, `sdk/node/file/upload.rs`, `sdk/node/folder.rs`
(`get_secrets`), `rs2/session.rs` (`begin_via_web`), `rs2/keys.rs`, `rs2/api/cache.rs`. Official SDK at `cs/v0.33.0`:
`cs/client/js/src/internal/upload/manager.ts` and `apiService.ts` (draft conflicts). Dependency behaviour read: reqwest 0.13.5
(`src/error.rs`, the URL in an error's text).

Not covered: upstream code no patch touches (sharing, Docs, search, Easy Switch, telemetry), `proton-crypto`/rPGP internals, pdcli,
the fork's tests as tests (nothing was built or run), and the live server's behaviour (every statement about what the API accepts
or returns is from reading the clients). Not repeated: S-10/V-21 (`Retry-After` unbounded), S-24, S-25, R-15, V-9, the Known-gaps
entries "left by P16" (bodies in the generic decode helpers' errors) and "block map that fails is not cached".

### C2-1: One node whose parent folder cannot be decrypted fails the whole P13 batch, and the event poller then replays that page for ever
- **Rating**: Medium
- **Location**: `fork:crates/proton-drive-sdk/src/node/mod.rs:976-1013` (`get_fresh_nodes_metadata`; `:1001`
  `Self::resolve_parent_key(..).await?` inside the loop over all links of all batches), `:1059-1070` (`resolve_parent_key` →
  `FolderOperations::get_secrets(parent)`), `fork:crates/proton-drive-sdk/src/node/folder.rs:289-310` (`get_secrets`: a degraded
  parent is `Err`, from the cache at `:299` or from `try_get_folder_secrets_else_error`, `node/mod.rs:618-629`);
  `crates/pdmount-backend/src/proton.rs:1061-1076` (`get_nodes_checked` passes the `Err` on; `classify` makes it `Other`, `:759`);
  `crates/pdmount-core/src/events.rs:276-279` (`apply_events`: `get_nodes(batch).await?`); `crates/pdmount-core/src/poller.rs:290-303`
  ("Every tree failed: nothing advances, the page comes again") and `:205-208` (retry with back-off, no limit);
  also `crates/pdmount-core/src/engine/listing.rs:222-224`.
- **Actor**: T2s (an editor or the owner of a share); T2 for My Files (another client with the account's session).
- **Scenario**: P13's doc says a link the API omits is missing from the result, and PdMount adds `NodeLookup::Degraded` for a node
  that does not decrypt. A third case has no representation: the node decrypts only with its parent's key, and the parent is
  degraded. Steps: (1) with a client of their own, the member creates a folder `F` in the share whose `NodePassphrase` is not
  encrypted to the parent's key (the server cannot check that); PdMount skips `F` as degraded. (2) The member moves a file `C`
  that the victim's tree knows into `F`. (3) The volume's event page names `C`, a known link, so `apply_events` asks
  `get_nodes([.., C, ..])`. `resolve_parent_key` for `C` fails ("Degraded node has no folder secrets"), and the `?` fails the call
  for every link of the request, not only `C`. (4) `apply_events` fails for the tree; when it is the volume's only tree (My Files
  always, a share that is the only one mounted from its owner's volume), the pass returns the error without moving the cursor, and
  every later poll reads and fails the same page. The same state arises without an attacker whenever a known node's parent
  becomes undecryptable.
- **Impact**: The tree stops following remote changes for good, across restarts (the stored cursor stays before the page): other
  members' new revisions, renames and deletions are not seen, local edits are made on stale content and become conflicts, and the
  log gets a warning per poll. Local uploads and reads of known revisions go on. No data is lost. A listing that resolves moved
  chains (`listing.rs:224`) fails the same way.
- **Fix**: In `get_fresh_nodes_metadata`, treat a failed `resolve_parent_key` per link: return that link as degraded (the
  converter already yields `PotentialObject::Degraded` when the parent key is missing: pass `None` and let
  `convert_dto_to_node_metadata_inner` report "Parent folder key is degraded"), and keep `?` only for transport and API errors.
  PdMount then gets `NodeLookup::Degraded` for `C` and keeps what it knows (D119). Independently, the poller should not replay a
  page without limit on a non-transient error: after a few failures of the same page, fall back to `begin_refresh()` from a fresh
  cursor, as it does when only some trees fail (`poller.rs:304-310`). Add a fork test with a link whose parent secrets are cached
  as degraded, and a fake-backend scenario.
- **Confidence**: high for the code path (traced on both sides); medium for step 1 (that the API stores a folder whose passphrase
  does not decrypt was not tested; degraded nodes are an accepted case in D119 and Known gaps).

### C2-2: The session-fork selector is part of the error text of a failed sign-in poll, which the daemon logs at WARN
- **Rating**: Medium (the Ratings section's "secrets in logs" is High; rated lower because it needs a failed poll and a sign-in
  completed afterwards, and the log is 0600)
- **Location**: `fork:crates/proton-sdk-rs2/src/auth.rs:736-749` (`poll_session_fork`: the selector is a path segment of the
  request URL; `send().await?` at `:741`), `:492-514` (`error_for_status_with_body`: `for url ({url})`);
  `fork:crates/proton-sdk-rs2/src/session.rs:276` (`poll_session_fork(&fork.selector).await?`: any error ends the login);
  reqwest 0.13.5 `src/error.rs:301` (an error's `Display` ends with ` for url (<url>)`);
  `crates/pdmount/src/session.rs:153-155`, `crates/pdmount/src/auth.rs:333-336` (`"sign-in did not complete: {error:#}"`),
  `:293-295` (`tracing::warn!(%error, "no new session")`, and the text kept in `login_error`).
- **Actor**: T4 (the design promises that no secret is logged, D071), and whoever receives the log (a bug report, a backup).
- **Scenario**: The earlier reviews rely on "the selector stays in memory" (2026-10-31 verification, R-2). The daemon starts a web
  login and shows the link. The SDK polls `GET auth/v4/sessions/forks/<selector>` every 5 s for up to 10 minutes, with a client
  that carries no credentials. One poll fails in transport (a Wi-Fi drop, a resume from suspend, a DNS error) or with a status
  other than 422 (a 429 or 5xx): the error text is `error sending request for url (https://…/auth/v4/sessions/forks/<selector>)`
  or `HTTP status server error (503 …) for url (…/<selector>): <body>`, and the daemon writes it to its log. The login attempt is
  over for the daemon, but not for the server: the page is still open in the browser, and when the user completes the sign-in
  there, the fork is filled and nobody collects it. Whoever reads the selector from the log within the fork's lifetime polls the
  same URL and receives `UID`, `AccessToken` and `RefreshToken` (`rs2/api/response.rs:152-167`).
- **Impact**: A session of the account (A1): listing, trashing and deleting need no keys. Decrypting content also needs the key in
  the link's fragment (the `Payload`), which is in `xdg-open`'s argv (V-15) and in `status` for the same uid. `pdmount login` prints
  the same text to the terminal. Not new in 0.5: the 0.4 code built the URL the same way; P16 and the D071 review did not cover
  URLs in error texts.
- **Fix**: In the fork (P16), keep the selector out of errors: in `poll_session_fork` map a transport error with
  `reqwest::Error::without_url()`, and give `error_for_status_with_body` a variant that omits the URL (or prints only its path
  template) for this call. In PdMount, as a second layer, redact `/sessions/forks/<segment>` in the login error before it is
  logged, stored in `login_error` or printed. A poll that fails in transport should also be retried until `FORK_MAX_WAIT` instead of
  ending the login, which removes the most likely trigger. Add a test that the error of a failed poll does not contain the selector.
- **Confidence**: high for the text reaching the log (each step read); medium for the server side (that the selector alone
  authorises the poll follows from the client sending nothing else, and that a filled fork stays collectable was not tested).

### C2-3: A cancelled caller leaves a token refresh half done; the next caller sends the same refresh token again
- **Rating**: Low
- **Location**: `fork:crates/proton-sdk-rs2/src/auth.rs:335-366` (`get_refreshed_access_token`: the request runs inside the
  caller's own future, `attempt.outcome.get_or_init(|| self.run_refresh(&attempt))`), `:370-412` (`in_flight` is cleared only when
  `run_refresh` completes); `crates/pdmount-backend/src/proton.rs:338-351` (`with_reauth` awaits the refresh inside the backend
  call); `crates/pdmount-fuse/src/readonly.rs:139` (a FUSE request's work is dropped at its deadline), and the `abort()`/`select!`
  sites of the poller, pins and marker tasks.
- **Actor**: none (robustness of A1's availability); T3 can provoke it by delaying the refresh answer.
- **Scenario**: The access token expires. The first backend call that gets a 401 starts the refresh and is the future that drives
  it. That call is a FUSE request which reaches its deadline, or a task that is aborted, while `POST auth/v4/refresh` is on its
  way: the future is dropped, tokio's `OnceCell` stays empty, and `in_flight` still names the attempt with the old pair. The
  next call rejected with the same token joins that attempt and runs `run_refresh` again with the same refresh token. If the server
  had applied the first request, the old refresh token is spent: the answer is code 10013, `refresh_token_expired` fires, and
  PdMount goes to `NeedsLogin` although a valid new pair was issued and dropped with the first answer.
- **Impact**: A forced new sign-in (and with it the exposure of C2-2 and V-15), triggered by slowness at the wrong moment. The old
  tokens are not usable after a failure in any other sense: a failed refresh leaves the stored pair unchanged and broadcasts
  nothing, so nothing stale is saved over a newer pair. Upstream 0.5.1 has the same cancellation hazard in a worse form (its
  replaced `OnceCell` stays empty, so `get_tokens` then fails with "Tokens not initialized"); P9 did not introduce it.
- **Fix**: Run the refresh in a task of its own (`tokio::spawn` from the first caller, the others await the shared outcome), so
  it completes and stores the pair whoever is cancelled. Add a test that drops the first caller mid-request and expects one
  request and a stored new pair.
- **Confidence**: high for the client side; medium for the server's answer to a reused refresh token (not tested).

### C2-4: A block's encrypted body is read into memory without a size limit before any check
- **Rating**: Low
- **Location**: `fork:crates/proton-drive-sdk/src/block/download.rs:75-78` (P2: `response.bytes().await?.to_vec()`, which also
  copies the body once more), `:92` (the whole plaintext is produced before it is written to the caller's bounded buffer);
  `crates/pdmount-backend/src/proton.rs:915-979` (`download_block_into`: the capacity check on the claimed size at `:932-936`
  and the `WriteZero` mapping act only after download and decryption). For contrast, PdMount's own thumbnail download reads in
  chunks under a cap (`proton.rs:1705-1714`).
- **Actor**: T2, T2s (whoever uploads the revision), within what the storage accepts as one block; T3 only behind TLS.
- **Scenario**: A member uploads a revision whose block is much larger than 4 MiB, with a matching listed hash and a valid
  manifest signature (their own). A read of it downloads the whole body, hashes it, decrypts it, and only then fails with
  "more than the … expected". Up to the block parallelism (6) such reads run at once per client, each holding the body, briefly
  its copy, and the plaintext.
- **Impact**: Memory use set by remote input: three times the block size per read in flight. Whether this is more than a few
  dozen MiB depends on the largest block the storage accepts, which was not tested; hence Low.
- **Fix**: In `download_verified`, refuse a response whose `Content-Length` exceeds a block limit (a parameter, default the
  SDK's block size plus the OpenPGP overhead), read the body in chunks under the same limit, hash while reading, and drop the
  `to_vec()` copy. PdMount passes `MAX_BLOCK_BYTES` plus overhead.
- **Confidence**: high for the code; low for the reachable size.

### C2-5: Minor and hardening (Info)
- **P17, key flags fail open.** `fork:crates/proton-sdk-rs2/src/api/response.rs:262-265`: `may_verify` accepts a key whose `Flags`
  field is missing (`is_none_or`). The schema always sends it; treat a missing value as not usable for verification.
- **P17, key cache grows per email.** `fork:crates/proton-sdk-rs2/src/api/cache.rs:279-297` (upstream) drops an entry only when the
  same email is asked again after its 30 minutes; `get_address_public_keys` (`account.rs:160-192`) now succeeds and caches for
  every `SignatureEmail` a share member puts on a revision, an empty key list included. Bounded per entry, unbounded in count.
- **P17/P3, shape of the listing.** `verify_block_listing` (`fork:…/node/file/block_map.rs:245-278`) refuses an empty block hash
  but does not require 32 bytes, and the manifest is a plain concatenation, so a listing that presents a thumbnail's digest as
  block 1 has the same manifest (thumbnails are encrypted with the same content key). Only the server can build such a listing
  (out of scope), and every downloaded block must still match a 32-byte SHA-256. Requiring `hash_digest.len() == 32` for blocks
  and thumbnails, and comparing the returned revision id with the requested one in `get_block_map`, would close it cheaply.
- **P11, "own draft" is by client UID only.** `fork:…/node/draft.rs:62-95`, `:262-266`, `:351-363`: a conflicting draft is deleted
  when its `ClientUID` equals this client's, with no age test. PdMount's UID is `pdmount-<device id>`
  (`crates/pdmount-core/src/device.rs:18-20`), so two installations with one device id (a copied data directory, a restored
  backup running beside the original) delete each other's uploads in flight; each side then retries (code 2511 is transient), so
  nothing is lost. PdMount passes `override_existing_draft_by_other_client = false` (`proton.rs:1163`), so another device's
  draft is never deleted: for a new file the fork is stricter than the JS SDK (it also requires `ConflictRevisionID` to be
  absent, `draft.rs:85-92` against `cs/client/js/src/internal/upload/manager.ts:276-287`). Left: between the conflict answer and
  `delete_multiple` the draft could be committed by its owner; that the endpoint refuses a committed link is the server's rule,
  as for the official SDK (`cs/…/upload/apiService.ts:280-294`).
- **P12, what counts as idempotent.** `fork:…/http/retry.rs:93-103`, `:123-128`: `PUT` and `DELETE` are retried once after a 5xx
  and up to three times after a timeout. A rename, move or commit that was applied before the timeout is sent again with its now
  stale precondition; this is S-25's case, which names only 5xx. A 429 is retried for every method with no bound per request
  other than the 50-in-a-row breaker (S-10). Connection failures are retried for every method, but only `is_connect()` ones,
  which never reached the server. Upstream's new storage retries on `is_request`/`is_body` (`sdk/api/storage.rs:196-204`) are
  not under this middleware, and a block upload is safe to repeat (same URL and token).
- **P8, endpoints that skip the HTTP status.** `fork:…/api/links.rs:194-200`, `:215-221`, `:294-298` (`move_link`,
  `move_multiple`, `copy_link`) parse the body without looking at the status, so a 429 or 5xx answer with a JSON body becomes an
  `ApiError` without `http_status` and `retry_after`, which PdMount maps to `Other`, and one with another body becomes a bare
  serde error. Use `parse_json_response` there as the other endpoints do.
- **P19, no bound on the offset.** `fork:…/server_time.rs:27-41`: any response of the API, Photos or storage clients may set the
  process-wide crypto clock to any value (upstream's behaviour; the storage hosts are whatever the block listing names). P19
  makes this neither better nor worse: a jump of 2 s or more is adopted at once, as before, and the next honest response
  corrects it, as before. No overflow: `saturating_sub`, `abs_diff`, and header dates are limited by chrono to four-digit years,
  so `device_now() + offset` (`:65`, unchecked) cannot wrap. A clamp (ignore an offset beyond a day or so, with a warning)
  would stop a broken `Date` header from dating signatures years off.
- **P4/P5, signature failures of extended attributes.** `fork:…/node/revision.rs:232-250`, `…/node/folder.rs:365-396`: a failed
  signature check of a revision's or folder's XAttr is logged and the claimed values are used, as the official SDKs do. PdMount
  bounds sizes itself (S-5, S-6).

## Checked and fine (area C2)
- **Rebase fidelity.** The range-diff shows, per patch, only what the commit messages state: P1 reduced to `list_all_blocks` and
  `get_revision_with_all_blocks` in place of upstream's loop (the sort, the from-1 and no-gap checks, and the check of the last
  block are kept; upstream's `ensure_contiguous_blocks` is covered by them); P5's XAttr moved into `prepare_creation` with
  `at_date(server time)`; P6's result built from the kept `draft_uid`; P8 with `is_transient_error` extended and
  `parse_delete_album_response` typed; P9 taking the patch's `auth.rs`; P12's middleware outside `ServerTimeMiddleware` in both
  client constructors and the Photos client; P16 squashed. P2, P3, P7, P10, P11, P13–P15, P17 and P18 differ in their messages
  only. The 18 merges have no hand-made hunk outside a test module (`git show --cc`: only `5d78af9`, tests of `node/revision.rs`).
- **Integrity of reads (P1, P2, P3, P17).** `get_block_map` lists every block (sorted, contiguous from 1), refuses an empty
  block hash, and verifies `ManifestSignature` over thumbnail digests by type and block digests by index, with the node key
  when `SignatureEmail` is empty and the address's non-compromised keys otherwise; a missing signature or an empty key list
  fails. PdMount stores a map only after that (`proton.rs:536-560`) and reads blocks only through `download_verified` with the
  map's hash (`:946-958`), so every block is compared with a signed hash before it is decrypted. A hash mismatch is not retried.
  The 404 refresh inside the downloader (`block/download.rs:236-259`) replaces only URL and token; the expected hash stays the
  map's. `BlockMap::refresh_urls` refuses a listing whose count, indices or hashes differ (PdMount does not call it; it refetches
  and re-verifies the map after 10 minutes). The uploader signs the manifest that the reader rebuilds
  (`build_revision_manifest` on both sides). Thumbnails: PdMount verifies the listing with `verify_block_listing` and compares
  the body's SHA-256 with the listed digest, under a size cap. `download_range` slices by `plain_size`, which `download_block`
  has checked against the decrypted length. No PdMount read path uses `BlockDownloader::download` (no hash) or
  `open_block_reader`.
- **Secrets in logs (P16).** Every removal of the 0.4 patch is present on `pdmount-0.5` (passphrase, cached address keys,
  serialized secret, `/users` body, probe body, share, thumbnail-block, move, copy, link-details and trash bodies). All log
  calls of both crates at `pdmount-0.5` were listed, multi-line ones included: none prints a token, key, passphrase or a success
  body. Left, not secrets: an error body at WARN for a failed thumbnail-block listing (`api/file.rs:395`), the plaintext name at
  debug in `move_single` (`node/operations.rs:433`), URLs with volume and link ids at info in the retry middleware. Error texts:
  `ApiError` keeps `Error`, `Details` and at most 512 bytes of a non-JSON body; the refresh error keeps status, code and message;
  auth errors keep at most 2 048 bytes of an error body; `decode_json` and the share, trash and thumbnail decoders keep the
  serde error only. The bodies still in errors are the Known-gaps list. Block URLs in transport errors carry no token (it is a
  header). The exception is C2-2.
- **P9.** A failed refresh returns `Err(TokenRefreshError)`, leaves the stored pair as it was, fires `tokens_refreshed` only on
  success and `refresh_token_expired` only for code 10013; concurrent callers share one attempt; PdMount maps expired to
  `Unauthorized` and the rest to transient (`proton.rs:723-729`). The probe URL follows the configured base URL. See C2-3.
- **P14.** The caller's hash replaces only `OriginalHash`; the new name and its hash are computed from the freshly fetched node
  and parent keys, so a node moved meanwhile is refused (its stored hash is under another folder's hash key). `rename_node`,
  `move_node` and `move_nodes` are unchanged.
- **P11, P6 with upstream's draft clean-up.** A failed write deletes only the draft this upload created (`upload.rs:233-251`); a
  failed commit deletes nothing; the result's revision is that draft's. Revision drafts are deleted only when the server names
  this client as their owner.
- **P7, P18.** Zero limits panic by design; PdMount's config allows 1–4 uploads (`config.rs:445`).
- **Error mapping in PdMount.** Typed causes are looked up through the whole chain and through `reqwest_middleware`'s wrapper;
  integrity errors come before API errors; the breaker's synthetic 429/503 are `ApiError`s and map to rate-limited/transient.
