# Evaluation of the community SDK narrrl/proton-sdk-rs

Dated 2026-11-18 (M6.0.5, `docs/plans/m6.md`). This document evaluates the second community Rust SDK for Proton Drive,
[`narrrl/proton-sdk-rs`](https://github.com/narrrl/proton-sdk-rs) (crates `proton-sdk` and `proton-drive-rs`, here "narrrl's
SDK"), against what PdMount builds on: upstream `tirbofish/proton-sdk-rs2` 0.5.1 plus the fork's patches P1–P19
(`pdmount-0.5` at `70f10ff`, `docs/sdk-patches.md`). It is research only: nothing switches.

All findings come from reading the source. Nothing ran against a Proton account. A scratch crate outside the repository
built both crates and ran `cargo tree`, `cargo about` and `dist plan` (§5). No adapter spike was built (§8). This is a
dated record: its citations are pinned and are not updated.

Path prefixes used below:

| Prefix | Location | Version |
|---|---|---|
| `nsdk/` | `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/proton-sdk-0.12.1/src/` | crates.io 0.12.1 (2026-11-17), built from commit `3fd5b531` (`.cargo_vcs_info.json`) |
| `ndrive/` | `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/proton-drive-rs-0.12.1/src/` | the same |
| `narrrl/` | `<narrrl checkout>/`, a clone of the GitHub repository | `main` at `0522c3c` (0.12.0, 2026-11-15) |
| `pdl/` | a clone of `github.com/narrrl/proton-drive-linux` | `main` at `e9d6d4a` (2026-11-16) |
| `sdk/`, `rs2/` | as in AGENTS.md (upstream's published 0.5.1 crates) | 0.5.1 |
| `cs/` | `<official SDK checkout>/` | tag `cs/v0.33.0` = `05c91159` |

**About the pin.** Commit `3fd5b531` of the 0.12.1 crates is not on GitHub, whose `main` ends at the 0.12.0 release; the
CHANGELOG has no 0.12.1 entry. The two releases differ only in four files of `proton-sdk` (`crypto/time.rs` is new;
`crypto.rs`, `crypto/encrypt.rs` and `http.rs` changed): new keys are dated by the server's clock (P19 row). So the
repository-level files (`narrrl/README.md`, `CHANGELOG.md`, `UPSTREAM_SYNC.md`) are read at `0522c3c` and apply to 0.12.1.
To restore the sources: a scratch crate depending on `proton-drive-rs = "=0.12.1"` and `proton-sdk = "=0.12.1"`, then
`cargo fetch` (crate SHA-256 `d05ad4be…` and `80c381e9…`).

## Summary

narrrl's SDK is a pure-Rust reimplementation with rPGP, actively maintained by one author, and closer to the current official
SDK than rs2 is. It has much of what PdMount patches into rs2: paged block listing, a reusable range reader, typed errors,
401 refresh, retries, fresh batched node fetches and the trash time. But it lacks the guarantees PdMount's design rests on.
Rename, move and revision uploads take their preconditions from the SDK's own fresh read, never from the caller, and its
batch move retries a refused `OriginalHash` with the server's hash. Range reads verify no block hash and no manifest.
Uploads return no revision ID. Custom XAttr keys (the POSIX mode) are neither written nor read. There is no web sign-in.
**Recommendation: stay on rs2 and the fork; no switch, no borrowed code; re-evaluate at M7 planning or at the crypto
migration (§9).**

## 1. Overview

| Aspect | narrrl 0.12.1 | rs2 0.5.1 + fork (PdMount today) | Evidence |
|---|---|---|---|
| Crates | `proton-sdk` (session, HTTP, account, crypto; 8.5k lines) and `proton-drive-rs` (Drive, Photos, events, cache; 18.2k lines) | `proton-sdk-rs2` (4.4k) and `proton-drive-sdk` (29.0k) | `nsdk/lib.rs`, `ndrive/lib.rs` |
| Crypto | rPGP (`pgp` 0.21.1) only, `#![forbid(unsafe_code)]`. The README says rPGP 0.20 | `proton-crypto` 0.12.1 over `gopenpgp-sys` (Go through cgo) | `narrrl/Cargo.toml`, `narrrl/README.md:37` |
| HTTP | `reqwest` 0.13 with rustls, `webpki-roots` and HTTP/2; own retry loop | `reqwest` with `reqwest-middleware` and P12 | `narrrl/Cargo.toml`, `nsdk/http.rs:873` |
| Login | SRP with the password, TOTP, human verification | browser session fork (F13) | `nsdk/session.rs:117-212` |
| Keys after a resume | the mailbox password, cached key salts, or per-key passphrases held by the caller | per-key passphrases in the SDK's persistent secret cache (D085) | `ndrive/client.rs:473`, `:510`, `:529` |
| Secrets | in memory only: share keys, a 512-entry LRU of folder keys | encrypted SQLite secret cache, kept across restarts (§12.4) | `ndrive/client.rs:229-283` |
| Entity cache | pluggable; `get_node` reads through it; invalidated by events | read through by `get_node`; P13 bypasses it | `ndrive/client.rs:721`, `:1301` |
| Errors | `ProtonError` enum with `Api(ProtonApiError { code, http_status, message, details })` | `ApiError` from P8, found in an `anyhow` chain | `nsdk/error.rs:9-82` |
| Official baseline | `05c91159` = `cs/v0.33.0`, synced 2026-11-05 | `8a44654a` (`js/v0.22.2`) | `narrrl/UPSTREAM_SYNC.md:10`; `docs/sdk-patches.md` |
| Consumers | `proton-drive-linux` (FUSE mount and GTK app, 85 stars), pins 0.12.0 | pdcli, PdMount | `pdl/Cargo.toml:18-19` |

## 2. Patches and findings

**Has it** means narrrl provides what the patch or the fix provides. **Lacks it** means it does not. **Differs** means it
provides something related that PdMount cannot use as it is. The last column says whether a switch would still need a patch
in a fork of narrrl.

### 2.1 Patches P1–P19

| Patch | narrrl | Evidence (narrrl; PdMount) | Patch still needed? |
|---|---|---|---|
| P1 block-list pagination | **has it**: pages of 50, up to 6 requested at once, sorted by index, contiguous from 1 or an error | `ndrive/transport.rs:161-211`; P1 | no |
| P2 block digests, manifest signature | **differs**: `BlockDto` has no `Hash`, so listed block hashes are never read or compared. A full download hashes what it fetched and checks the manifest signature over that, but a failure is only logged (`VerificationStatus`); the download still returns `Ok` | `ndrive/dtos.rs:908-917`, `ndrive/client.rs:1356-1424`, `:8008-8037`, `narrrl/README.md:234`; P2 | yes: fatal per-block hash and manifest checks |
| P3 reusable block map | **has it**: `open_revision` / `open_revision_at` resolve keys and the block table once; `RevisionReader::read_at` fetches the covering blocks; an expired block URL (401/403/404) re-lists the revision once | `ndrive/client.rs:1449`, `:1664`; `ndrive/revision.rs:229-240`, `:330`, `:426`; P3 | no (integrity: P17) |
| P4 revision XAttr decode | **differs**: size, mtime (the ISO string verbatim), SHA-1 and block sizes are decoded. Only `Common` and `Location` are parsed, so custom keys (`pdmount.mode`, F10) are dropped | `ndrive/client.rs:7778`, `ndrive/node.rs:247-278`, `ndrive/dtos.rs:841-871`; P4 | yes: custom keys, read and write |
| P5 folder mtime | **differs**: the mtime is written on create. The folder XAttr is deserialized but never decoded; a folder's `modification_time` is the link's `ModifyTime` | `ndrive/client.rs:2572-2650`, `:7856`, `ndrive/dtos.rs:784-789`; P5 | yes: decode on read |
| P6 upload returns the revision | **lacks it**: a new file returns `NodeUid`, a new revision `()` | `ndrive/client.rs:2235`, `:2469`; P6 | yes |
| P7 per-client file semaphore | **has it**: no whole-file semaphore; only block limits (P18) | `ndrive/client.rs:137-157`; P7 | no |
| P8 typed API errors | **differs**: typed, with the code and `Details`, but without `Retry-After`, and a code missing from `ResponseCode` becomes `Unknown` (the raw number only reaches a debug log) | `nsdk/error.rs:72-82`, `nsdk/api.rs:117-169`, `nsdk/http.rs:768-773`; P8 | small: `retry_after` and the raw code |
| P9 refresh errors, base URL | **has it**: 401 refresh inside the client, one at a time; the refresh request is not resent after a timeout; a refused refresh is `Err(Api)` with code 10013; rotated tokens reach `set_on_tokens_refreshed`; configurable base URL, used for the refresh too | `nsdk/http.rs:210`, `:457-480`, `:583-640`, `:915-933`, `nsdk/config.rs:120`; P9 | no |
| P10 move with rename | **differs**: `move_node(uid, parent)` takes no name. A rename on move exists only in `move_nodes_streaming` (`NodeMoveItem::target_name`), over `move-multiple` batches with the code-2000 retry (§3.1) | `ndrive/client.rs:2784`, `:3004`, `ndrive/node.rs:17-21`; P10 | yes, with P14 |
| P11 typed revision conflict | **lacks it**: `CurrentRevisionID` is the active revision read just before the draft, never a caller's base. Another client's open draft is an untyped `InvalidOperation`. `ClientUID` is the session ID, not configurable | `ndrive/client.rs:4911-5010`, `:4949-4952`, `:4997`; P11, §10.2 | yes |
| P12 metadata retries | **differs**: 408, 429, 502–504, timeouts and refused connections are retried for every method, writes included; reads (GET, `post_query`) also after a connection drops mid-request. 500 is not retried. 3 retries, 0.5 s base with full jitter, `Retry-After` capped at 120 s. No circuit breaker | `nsdk/http.rs:873-906`, `:948-958`, `nsdk/config.rs:26-41`; P12 | optional: no write retries on 5xx or timeouts, circuit breakers |
| P13 batched fresh fetch | **differs**: `enumerate_nodes` always fetches and refreshes the entity cache, but with 150 links per request and 4 requests at once (official: 50 in C#, 100 in JS, D035) | `ndrive/client.rs:108`, `:115`, `:868-960`, `:7889`; `cs/client/cs/src/Proton.Drive.Sdk/Nodes/NodeProvider.cs:18`, `cs/client/js/src/internal/nodes/apiService.ts:32`; P13 | small: batch size and concurrency |
| P14 caller's name hash | **lacks it**: rename and move compute `OriginalHash` themselves (entity cache, else the link fetched just before, else recomputed). `Node` exposes no name hash, though the cache entry carries one | `ndrive/client.rs:2717-2770`, `:3242-3280`, `:7912-7940`, `ndrive/node.rs:61-118`, `ndrive/cache.rs:31`; P14 | yes |
| P15 trash time | **has it**: reads `TrashTime` and the legacy `Trashed`, and checks `State`; exposed as `Node::trashed` (no time) | `ndrive/dtos.rs:738-780`; P15 | no |
| P16 no secrets in logs | **has it**: no log call found that prints a body or a secret; a failed request logs its path and code; `KeyPassphrases` prints redacted | `nsdk/http.rs:768-780`, `nsdk/account/passphrases.rs:107`; P16 | no (PdMount's log filter needs the new crate names) |
| P17 manifest on block-map reads | **lacks it**: `read_at` and `download_range` check no hash and no manifest, only that each block decrypts | `ndrive/revision.rs:330-380`, `ndrive/client.rs:1530`; P17 | yes |
| P18 block parallelism | **differs**: `with_max_inflight_blocks` is one pool for blocks held in memory by both directions, buffered and encrypting blocks included (default 16); per file, 5 block uploads and 10 block downloads, fixed | `ndrive/client.rs:137`, `:144`, `:634`, `ndrive/revision.rs:31`; P18, D058 | optional: per-direction transfer limits |
| P19 server-time jitter | **differs** (new in 0.12.1): there is no offset. New keys and their self-signatures carry the latest server `Date` seen, which never moves back and does not advance between responses; every other signature carries the device clock. The jitter P19 handles cannot occur, but a device clock behind the server dates signatures before the key they use. Not tested here | `nsdk/crypto/time.rs:21-37`, `nsdk/crypto/encrypt.rs:134`, `:180`, `:382`, `:434`, `nsdk/http.rs:936`; P19 | to verify |

Counts: **has it** 6 (P1, P3, P7, P9, P15, P16), **lacks it** 4 (P6, P11, P14, P17), **differs** 9 (P2, P4, P5, P8, P10,
P12, P13, P18, P19).

### 2.2 Findings F1–F16 (DESIGN §2.2)

| Finding | narrrl | Evidence |
|---|---|---|
| F1 size and mtime from the XAttr | **has it** (P4 row) | `ndrive/client.rs:7778` |
| F2 first 10 blocks only | **has it** (P1 row) | `ndrive/transport.rs:161-211` |
| F3 no block or manifest check | **differs** (P2 row) | `ndrive/dtos.rs:908-917` |
| F4 range reads rebuild state | **has it**: `RevisionReader`; `download_range` still rebuilds, as documented | `ndrive/client.rs:1530` |
| F5 one transfer per client | **has it** (P7 row) | `ndrive/client.rs:137` |
| F6 upload returns no revision | **lacks it** (P6 row) | `ndrive/client.rs:2469` |
| F7 untyped errors | **has it** (P8 row) | `nsdk/error.rs:9` |
| F8 no 401 refresh, failed refresh reports success | **has it** (P9 row) | `nsdk/http.rs:457` |
| F9 folder mtime | **differs** (P5 row) | `ndrive/client.rs:7856` |
| F10 custom XAttr keys | **lacks it** (P4 row) | `ndrive/dtos.rs:1543-1577` |
| F11 `CurrentRevisionID`, `OriginalHash` | **differs**: both are sent, both taken from the SDK's own fresh read (P11, P14 rows) | `ndrive/client.rs:4951`, `:7912` |
| F12 events carry IDs only | **differs**: the same data; one call drains every page; Create, Update and UpdateMetadata become one `NodeUpdated` (§3.3) | `ndrive/client.rs:1235-1290`, `:8353-8379` |
| F13 browser fork only | **differs**: password login only (§4) | `nsdk/session.rs:117` |
| F14 stale entity cache | **differs**: `get_node` reads through the cache, which only events evict (and `restore_revision`); own uploads, renames and moves leave it stale; `enumerate_nodes` is always fresh | `ndrive/client.rs:721`, `:1301-1330`, `:1640` |
| F15 hash fetched just before rename/move | **lacks it**: the same defect, plus the code-2000 retry (§3.1) | `ndrive/client.rs:7912-7940` |
| F16 `TrashTime` | **has it** (P15 row) | `ndrive/dtos.rs:738` |

Counts: **has it** 7, **lacks it** 3 (F6, F10, F15), **differs** 6 (F3, F9, F11, F12, F13, F14).

## 3. Departures from what PdMount relies on

### 3.1 Conditional rename and move (D038, P14; §8.3, §10.4, §10.5)

PdMount sends the name hash it last saw as `OriginalHash`. A rename or move by another device since then is refused (code
2000), and the executor follows the remote placement with a `RenameRename` record. narrrl takes the hash from its own reads
(F15), so the server's check passes against the state narrrl just read, and the other device's change is overwritten.
`move_nodes_streaming`, the only call that moves with a rename, also retries a 2000 once with the server's `NameHash` when
the node still has the parent and name that narrrl read while preparing the move (`ndrive/client.rs:3171`, `:3199-3240`).
Since that read is the SDK's own, the retry succeeds exactly when another device renamed the node before the move was
prepared. The official C# SDK checks the retry against the caller's `CurrentParentUid` and `CurrentName`
(`cs/client/cs/src/Proton.Drive.Sdk/Nodes/Move/NodeMoveItem.cs:11-16`), and retries only when it computed the hash itself
(`cs/client/cs/src/Proton.Drive.Sdk/Nodes/Move/NodeMoveOperation.cs:252-253`, `:315-345`). narrrl dropped both fields on
purpose, to read them off the link rather than trust the caller (`narrrl/UPSTREAM_SYNC.md:40`).

The retry cannot be turned off. `rename_node` and `move_node` do not retry, but `move_node` cannot rename, and both still
choose the hash themselves. PdMount's fresh pre-check would narrow the window to one round trip, but the server-side check,
which §8.3 counts on for that race, would be lost. A switch needs a P14 equivalent in a fork of narrrl: a caller-supplied
hash on `rename_node` and on `NodeMoveItem`, and no retry when the caller supplied one.

### 3.2 Revision conflicts and draft clean-up (D043, P11; §10.4)

Layer 3 of §10.4 sends `CurrentRevisionID = base_rev` and maps the refusal to `RevisionConflict`. narrrl reads the active
revision just before it creates the draft and sends that (`ndrive/client.rs:4920-4952`). A revision committed by another
device between PdMount's pre-check and the draft is overwritten silently; it stays in the history, but no conflict copy is
made. Another client's open draft yields `InvalidOperation("revision draft is held by another client")` (`:4997`), which
PdMount would have to match by its text to treat it as transient. `ClientUID` is the session ID (`:4949`), so after a new
login PdMount's own stale draft counts as another client's and blocks the file until the server collects it. PdMount
sends a stable `pdmount-<device.id>` (§10.2).

Draft clean-up: a new file or photo whose block upload fails deletes its draft; one whose seal fails deletes it only
after a clear rejection (`ndrive/client.rs:5077-5087`, `:8508`). A new revision that fails keeps its draft. A conflicting
revision upload makes no draft at all, so there is nothing to clean. A switch needs a P11 equivalent: the caller's base
revision, a typed conflict, and a configurable `ClientUID`.

### 3.3 Events and refresh (§9, F12)

`enumerate_events(scope, cursor)` drains all pages in one call and returns events that each carry their ID
(`ndrive/client.rs:1235-1290`). A page with `Refresh` ends the list with `ContinuityLost { id }` (`:1254`); `more` is
handled inside. PdMount's `poll_events` contract (one page with `next`, `more`, `refresh`) can be met by treating one
drain as one page whose `next` is the last event's ID. The cost is that a large backlog arrives in one batch. Event kinds
collapse to `NodeUpdated` and `NodeDeleted` (`:8353-8379`); PdMount uses the kinds only to count them in its log and to
recognise a delete (`crates/pdmount-core/src/events.rs:329-332`), so that is enough.

`EventManager` cannot sit under PdMount's pollers (§9.2):
- It stores the cursor after broadcasting a page, not after the subscribers applied it (`ndrive/event_manager.rs:236-251`).
  §9.1 stores the cursor only after the rows the page changed.
- A subscriber that falls 256 events behind loses events (broadcast lag).
- After a failure it waits 1 s, doubling up to the interval, without jitter (`:39`, `:226`). §9.2 waits the interval
  times 1, 1, 2, 3, 5, 8, 13 plus up to 1 s of jitter, as the official event manager does.
- The first poll waits one interval (`:215`); PdMount polls at once.
- It has one scope per manager and no shared-volume fan-out.

PdMount would keep its own pollers and call `enumerate_events` directly. `invalidate_caches_for_event` matters only for
callers of `get_node`; PdMount would not call it.

### 3.4 Echo suppression (§7.5, P6)

PdMount records the revision an upload returns. An event whose fresh node matches it is an echo. narrrl returns no
revision ID. Reading it afterwards (`enumerate_nodes`, `NodeKind::File::active_revision_id`, `ndrive/node.rs:264`) costs a
request per upload and is unsafe: a revision another device commits just after ours would be recorded as ours, so its
event would be taken for an echo and never applied. A switch needs P6.

### 3.5 Trash time (P15, F16)

narrrl has it (P15 row). `Node::trashed` is a boolean, which is all `RemoteNode::trashed` needs.

### 3.6 Entity cache staleness (F14, D035, P13)

`enumerate_nodes` always fetches and writes the fresh nodes into the cache, as D035 requires, but sends 150 links per
request and 4 requests at once, where D035 follows the official 50 or 100 per request, one batch after the other (P13
row). `get_node` reads through the cache and goes stale after our own writes (F14 row). PdMount would call only
`enumerate_nodes`, and would need a patch, or a decision, on the batch size.

### 3.7 Size, mtime and mode (P4, F1, F10; P5, F9)

Size, SHA-1 and block sizes come from the revision XAttr. The mtime is the ISO string as written, which the adapter would
parse. The POSIX mode in `pdmount.mode` (F10) is lost both ways: uploads write only `Common` (and `Location` for photos,
`ndrive/dtos.rs:1543-1577`), and reads parse only those two (`:841-871`). After a switch, every new revision would drop
the mode. `rsync -p` and git's `core.filemode` would stop converging (§8.5, scenarios R and G). Folder mtimes are sent on
create but read back as the server's `ModifyTime` (P5 row).

### 3.8 Further roots (M5.9–M5.14)

Nothing blocks the further roots; the adapter work is part of §7.
- **Shares:** `NodeUid` carries the volume, so one client reads any volume. Shared items come from
  `enumerate_shared_with_me` (`ndrive/client.rs:3456`); nodes carry `membership` and `direct_role` (`ndrive/node.rs:89-102`).
- **Photos:** `ProtonPhotosClient` has the timeline (all pages in one call), thumbnails (`download_thumbnail`,
  `enumerate_thumbnails`) and albums (`ndrive/photos.rs:149-480`).
- **Core events:** narrrl has no core event stream, which PdMount polls for `DriveShareRefresh` (D112). The adapter can
  call it through `ApiHttpClient::get` (`nsdk/http.rs:338`), which is public and brings refresh and retries. The same
  holds for PdMount's own `TimelineClient`.

## 4. Auth and secrets (DESIGN §12, D029)

| Item | narrrl | What a switch needs |
|---|---|---|
| Web sign-in (§12.1, §12.2, RQ5) | none: `begin` / `begin_verified` take the password, then `apply_second_factor_code` (`nsdk/session.rs:117-220`). `proton-drive-linux` asks for the password and TOTP (`pdl/crates/pdfs-core/src/auth.rs:328-349`) | build the fork flow in PdMount on narrrl's public API. The calls: `http::get_unauthenticated` for `auth/v4/sessions/forks` and its selector (`nsdk/http.rs:666`), the payload's `keyPassword` decrypted as `rs2/session.rs:672-700` does, `ProtonApiSession::resume` (`nsdk/session.rs:86`), and `KeyPassphrases` filled with that passphrase for each user key ID (`nsdk/account/passphrases.rs:30`), as rs2 fills its secret cache (`rs2/session.rs:318-323`). About 200–300 lines ported from rs2 (MIT); no SDK change |
| What a session stores | `ResumeParameters`: session ID, username, user ID, tokens, scopes, 2FA flag, password mode (`nsdk/session.rs:39-48`), the same fields as rs2's `StoredCredentials`. Keys need the mailbox password, the key salts or the per-key passphrases, held by the caller | the keyring entry `<profile>:session` keeps its content. The passphrases go to the D085 store (`sdk-keys.db`, sealed with the cache key; keyring or files per D029 as now). The SDK secret cache `sdk-secrets.db` and its 5000-entry cap are no longer needed. The mailbox password is never stored |
| Resume and refresh (§12.3, P9) | transparent 401 refresh, rotations through `set_on_tokens_refreshed`, refusal as code 10013 | the `with_reauth` wrapper goes; the token saver hooks the callback; a cheap GET replaces `ensure_authenticated` for the proactive refresh |
| Secret and key caches (§12.4, D085) | node and share keys in memory only (folder-key LRU of 512); after a start they are derived again along the path, an S2K per folder (`ndrive/client.rs:295`, `:318`) | a new cache key deletes only the block cache (and an entity cache, if one is kept) |
| Secrets in logs (P16) | none found (P16 row) | the log filter's target `proton_sdk_rs2` becomes `proton_sdk` and `proton_drive_rs` |
| Server time (P19) | keys dated by the last server `Date` (P19 row) | verify on the real account |
| Existing profiles | the tokens map one to one; the passphrases sit in `sdk-keys.db` in rs2's sealed format | a one-time reader of that format, or one new login per profile (as D088 did once) |

## 5. Build and dependencies

Checked in a scratch crate outside the repository that depends on both crates at `=0.12.1`. It built with
`cargo build --release` on Rust 1.98.1 without Go, clang or protoc in use.

- **Crate tree:** 357 packages in the scratch crate's `Cargo.lock` (PdMount has 607). Notable: `reqwest` 0.13 (rustls,
  `webpki-roots`, HTTP/2, gzip), `rustls` 0.23 over `aws-lc-rs` (`aws-lc-sys`, already in PdMount's tree), `pgp` 0.21.1,
  `bcrypt`, `aes-gcm`, `hkdf`, `sha1`/`sha2` 0.11, `lru`, `chrono`. Absent: `gopenpgp-sys`, `bindgen`, `prost-build`.
- **What leaves the build:**
  - Go: `gopenpgp-sys` under `proton-crypto`.
  - clang/libclang: `bindgen` in `gopenpgp-sys`.
  - protoc and `libprotobuf-dev`: `prost-build` in both rs2 crates.
  - So `scripts/build-doctor.sh` loses its Go, protoc and clang checks; `.github/workflows/ci.yml` loses
    `protobuf-compiler libprotobuf-dev clang` and `setup-go`; `dist-workspace.toml` loses `clang`, `protobuf-compiler` and
    `github-build-setup` (Go).
  - A C compiler stays (`aws-lc-sys`, `libsqlite3-sys`), as do pkg-config and libdbus (keyring, zbus) and fuse3.
- **Licences:** `cargo about generate` with PdMount's `about.toml` fails on `webpki-roots` 1.0.12, licensed
  `CDLA-Permissive-2.0` (root certificate data). With that licence added to `accepted` it succeeds. narrrl's own crates
  are MIT; the LICENSE file adds a preamble that names Proton AG's copyright for the original SDK, which is why GitHub shows
  no licence (`NOASSERTION`).
- **MSRV:** `rust-version = "1.96"` (`narrrl/Cargo.toml`). PdMount's minimum (1.85 in `scripts/build-doctor.sh`, from
  edition 2024) would rise to 1.96; the local toolchain (1.98.1) and CI's stable are newer.
- **`dist plan`:** dist 0.32.0 with PdMount's `dist-workspace.toml` minus the Go, clang and protoc entries plans one app,
  one `.tar.gz` and its `.sha256`.

## 6. Maintenance and health (read 2026-11-18; nothing posted, D089)

- **People:** one maintainer, who wrote all 126 commits on `main`. The repository was created 2026-06-24 and has 6 stars and
  no forks. Issues and PRs: 3 in all. Two are the maintainer's own merged PRs; #2, from another user (a shared-root key
  reused within a volume), is closed.
- **Releases:** 45 of each crate since 0.1.0 (2026-06-28); 8 between 0.9.0 (2026-10-15) and 0.12.1 (2026-11-17). The
  CHANGELOG starts after 0.9.2, follows Keep a Changelog and lists a "Breaking" section in each minor release
  (`narrrl/CHANGELOG.md:10-48`). 0.x semver: breaking changes take a minor version. 0.12.1 was published from a commit not
  on GitHub and without a changelog entry.
- **Following the official SDK:** `UPSTREAM_SYNC.md` pins `05c91159`, which is `cs/v0.33.0`, the tag M6.0.2 moves the
  `cs/` row to (`narrrl/UPSTREAM_SYNC.md:10`). It logs each sync with every upstream commit triaged as ported, no-op or
  deliberately diverged (`:24-49`); `scripts/upstream-sync.sh` lists the commits to triage. rs2's baseline is older
  (`js/v0.22.2`). Deliberate divergences that matter here: the entity cache is kept (`:31`), and `NodeMoveItem` has no
  current parent or name (`:40`).
- **Operational rules (`cs/README.md:40-47`, DESIGN §2.4):**
  - `x-pm-appversion` is a required configuration field, sent on every API request (`nsdk/config.rs:93`,
    `nsdk/http.rs:501`).
  - Event-based sync: `enumerate_events`, `EventManager`.
  - Back-off: HTTP retries with jitter and `Retry-After`.
  - Rate limits: no circuit breaker, and node fetches of 150 links run 4 at once (§3.6).
  - The README asks consumers to follow these rules (`narrrl/README.md:17`).
- **Crypto migration (Q5, `cs/README.md:23-28`):** no plan in the README, CHANGELOG or `UPSTREAM_SYNC.md`. The code already
  writes v6 node keys with SEIPDv2 (AEAD) content when an upload asks for it (`nsdk/crypto/encrypt.rs:117-120`). Following
  the new model would be narrrl's own port on rPGP. rs2 depends on Proton's `proton-crypto`.
- **Tests and CI:** 209 offline unit tests (96 in `proton-sdk`, 113 in `proton-drive-rs`; the fork has 268). There are 49
  integration tests in `tests/`, almost all live tests against a real account, ignored by default. GitHub Actions runs fmt,
  clippy with `-D warnings`, a build and the offline tests on every push (`narrrl/.github/workflows/ci.yml`).
- **Use:** `proton-drive-linux` (85 stars, 8 forks, PRs merged daily) reads through `open_revision` / `read_at`
  (`pdl/crates/pdfs-fuse/src/reads.rs:379`, `:672`). Many of the SDK's features were built for it.

## 7. Cost of switching (rough)

| Area | Work |
|---|---|
| Proton adapter (`crates/pdmount-backend/src/proton.rs`, 2451 lines; §3.2) | Most of it rewritten. `account`/`root`: `get_my_files_folder`. `list_children`: `enumerate_folder_children_node_uids` plus `enumerate_nodes`. `get_nodes`: `enumerate_nodes`, name hashes from P14'. `blocks`/`fetch_block_into`: a `RevisionReader` memoized per revision in place of the `BlockMap`, a block read as a byte range, integrity from P2'/P17'. `upload_new`/`upload_revision`: the streaming uploads with P6', P11' and the mode patch (the expected-SHA-1 check moves to PdMount). `create_folder`: as is. `rename`: P14'. `trash`, `revisions`, `latest_event_id`, `poll_events` (§3.3), `storage_info` (`quota`): direct. `map_error` becomes a match on `ProtonError`. Shares and Photos move to narrrl's calls (§3.8). Its unit tests are rewritten |
| Auth and credentials (`crates/pdmount/src/session.rs`, `auth.rs`, `credentials.rs`, `secret_cache.rs`) | The web sign-in ported from rs2 (§4); `StoredCredentials` replaced; D085's store holds `KeyPassphrases`; the token saver, proactive refresh and log filter adapted. One new login per profile, or a one-time migration |
| Fork | P1, P3, P7, P9, P15 and P16 dropped. A new fork of narrrl with at least 7 patches: P2'/P17' (fatal per-block and manifest checks on every read), P4' (custom XAttr keys, both ways), P5' (folder XAttr decode), P6', P11', P14'. Optional: P8' (`Retry-After`, raw code), P12' (no write retries on 5xx), P13' (batch of 50, one at a time), P18', P19' (signature time). Upstreaming is uncertain: P14' and P11' run against a choice narrrl made on purpose (§3.1) |
| Documents | DESIGN §2.2 (F1–F16 rewritten), §3.2, §6.3, §7.5, §8.3, §9.2, §10.4, §12, Appendix A; `docs/sdk-patches.md` replaced; AGENTS.md citation table (`sdk/`, `rs2/`), build and environment notes; decisions D002 and D004 superseded, D029, D035, D038 and D085 amended |
| Build and CI | §5: Go, clang and protoc go; the `CDLA-Permissive-2.0` licence comes in; MSRV 1.96 |
| Tests | Fake-backend tests and scenarios are unaffected. Again on the real account: the Q1/Q6 probe (`crates/pdmount/src/probe.rs`, both preconditions and drafts), the roots probe (`probe_roots.rs`), and a real-account matrix as in M2.8, since every SDK path changes |

Overall this is a milestone of its own: about as large as M3's sync work on the adapter side, with a fork that is no smaller
than today's in the areas that matter (conflict safety and integrity).

## 8. Spike

Not built. The question the spike would answer, whether `list_children`, `blocks`/`fetch_block_into` and `rename` with
`expect` map onto narrrl's API, is answered by reading. The first two map with adapter work. The third cannot be expressed
without a patch (§3.1), and an adapter would not show more than that.

## 9. Recommendation

**Stay on rs2 and the fork through M6. Do not switch, and borrow no code from narrrl now.**

- **What narrrl lacks is what PdMount's correctness rests on:** the conditional rename and move (D038), the base-revision
  precondition (D043, §10.4), integrity on every block read (P2, P17), the revision ID for echo suppression (§7.5), the mode
  in the XAttr (F10), and the web sign-in (RQ5).
  - Its code-2000 retry and its precondition design are deliberate, so a switch means a new fork with at least 7 patches
    and an uncertain path upstream. PdMount would carry about as large a fork as today, on top of a rewrite of the adapter
    and the auth code and a new real-account validation.
- **What narrrl does better** does not need a switch now: the build without Go, clang or protoc, a baseline at
  `cs/v0.33.0`, typed errors, HTTP/2, replay of interrupted reads and an active maintainer. PdMount's fork already covers
  the functional gaps those would close.
- **No part is worth borrowing as a fork patch.** Read replay is already covered: PdMount treats a broken connection as
  transient and retries. Server-time key dating is P19. Paged block listing is P1.
- **For M6.0.2:** `narrrl/UPSTREAM_SYNC.md:31-49` triages the official SDK's changes from mid-July to `05c91159` commit by
  commit. It is a useful cross-check for the parity findings between rs2's `js/v0.22.2` baseline and `cs/v0.33.0`.

**Re-evaluate** at M7 planning, or earlier when one of these happens:
1. rs2 upstream does not follow the crypto migration (Q5) when Proton announces its dates, or stops releasing.
2. narrrl accepts caller-supplied preconditions (a base revision, a name hash, or C#'s `CurrentParentUid`/`CurrentName`),
   fatal integrity checks on range reads, custom XAttr keys and a returned revision ID. Asking the maintainer waits until
   PdMount contacts other projects (D089).
3. The Go, clang and protoc build needs become a real cost, for packaging or a second architecture.

This recommendation changes nothing, so no decision entry is proposed (D002 and D004 stand).
