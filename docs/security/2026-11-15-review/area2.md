## Area C2 (the Photos tree and its stored thumbnails, M5.13a/M5.13b): findings

Reviewed read-only at commit 976ea61, the M5 changes `git diff 8b01e65..976ea61` for: `crates/pdmount-core/src/photos.rs` and
`photos/{model,rows,sync,thumbnails}.rs`, `crates/pdmount-backend/src/photos.rs`, the Photos parts of
`crates/pdmount-backend/src/proton.rs` (`PhotosClients`, `TimelineClient`, `thumbnail_ref`, `fetch_thumbnails`,
`download_thumbnail`), `crates/pdmount-core/src/readonly.rs`, `crates/pdmount-fuse/src/readonly.rs`, and where the
Photos root is wired (`crates/pdmount/src/roots.rs` `open_photos`, `crates/pdmount-core/src/paths.rs`). Decisions D081,
D107, D113, D115, D119; DESIGN §14.5. Dependencies read at the versions in `Cargo.lock`: `image-webp` 0.2.4, `image`
0.25.10 (`default-features = false`), `png` 0.18.1, `md-5` 0.10.6, `chrono` 0.4.45, `proton-rpgp` 0.3.1, `dirs` 6.0.0; SDK
fork at caafaea (`sdk/…`). Threat model: `docs/security/threat-model.md` (D070), which does not cover Photos yet (see
"Threat model additions"). Known gaps in `TODO.md` (memory per photo unbounded, one listing per photo, the shared gate,
`PhotoTag` on the real account) are not repeated.

Actor for remote input here: T2 as the threat model defines it. For Photos that is whatever uploads into the account's
own Photos volume: the user's other devices, official or third-party apps holding a session of the account, or a client
that is buggy or compromised. Photos of other users (shared albums) do not reach the tree: D113 shows only what the
timeline lists, which is the Photos root. A malicious server stays out of scope.

### C2-1: A WebP whose inner VP8 frame is larger than its canvas is decoded at full size before any size check
- **Rating**: Medium
- **Location**: `crates/pdmount-core/src/photos/thumbnails.rs:195-225` (`decode`: checks `decoder.dimensions()` and
  `output_buffer_size()`, sets `set_memory_limit`, then `read_image`); `image-webp` 0.2.4 `src/decoder.rs:427-431`
  (VP8X: `dimensions()` is the canvas from the VP8X header), `src/decoder.rs:694-702` (still image:
  `Vp8Decoder::decode_frame` first, the frame-vs-canvas comparison after it), `src/decoder.rs:800-808` (the same order
  for the first frame of an animation), `src/vp8.rs:1187-1205` (`read_frame_header` allocates the Y, U and V planes from
  the frame's own 14-bit width and height), `src/decoder.rs:551-556` (`set_memory_limit`: "Some allocations currently
  ignore this limit"; the VP8 planes are among them).
- **Actor**: T2 (an uploader into the account's Photos volume chooses the thumbnail bytes and signs them in the revision
  manifest, so the D115 checks all pass).
- **Attack**: The uploader stores as the type 1 thumbnail an extended WebP (`VP8X`) whose canvas says 512 × 384 and
  whose `VP8 ` chunk carries a key frame header of 16383 × 16383. All of a key frame's macroblocks can be skipped
  (no coefficients, about 2–3 bits of mode data per macroblock in the first partition), so the roughly one million
  macroblocks of such a frame fit in a few hundred KB, under `MAX_THUMBNAIL_BYTES` (1 MiB) and under the 512 KB first
  partition limit. `decode` accepts the header (512 × 384 ≤ 1024) and the output buffer (≤ 4 MiB). `read_image` then
  runs the VP8 decoder on the inner frame: it allocates about 268 MB (Y) + 2 × 67 MB (U, V) at once, grows a `Vec` of about
  a million macroblock records, intra-predicts every macroblock into the planes (so the pages are touched), and only then
  compares 16383 with 512 and fails with `InconsistentImageSizes`. The same holds for the first frame of an animated
  WebP (`ANMF` checks the frame rectangle against the canvas, but not the inner VP8 header). The photo is refused
  for the session, and the next one of the same kind costs the same: one month of such photos repeats it per photo
  (decoding is serial in one blocking task), and every new session or month open repeats it again.
- **Impact**: Per crafted thumbnail, a transient allocation of roughly 0.4–0.5 GB written in full and seconds of CPU
  (estimated from the code, not measured), in the daemon that also serves My Files and every other mount of the
  profile. On a machine with little free memory, a container or a service memory limit, the kernel's OOM killer ends
  the daemon (all mounts of the profile go stale); with strict overcommit or an address-space limit the allocation
  fails, and an allocation failure aborts the process: `catch_unwind` (`thumbnails.rs:401`) does not catch it. D115 and
  DESIGN §14.5 state that the WebP's dimensions are bounded before decoding and that the decoder has a memory limit;
  neither holds for this input.
- **Fix**: Before `WebPDecoder::new`, walk the RIFF chunks of the (≤ 1 MiB) buffer and, for every `VP8 ` chunk at the
  top level or as the image sub-chunk of an `ANMF`, read its frame header (3-byte tag, start code `9d 01 2a`, then
  little-endian width and height & 0x3FFF) and refuse the thumbnail unless both are ≤ `MAX_SIDE` and, for a still
  `VP8X`, equal to the canvas. Simpler and enough for Proton's thumbnails: refuse animated WebPs outright
  (`decoder.is_animated()`), and for a `VP8X` still check its `VP8 ` chunk's frame header as above (`VP8L` is already
  checked before decoding, `lossless.rs:105-109`; a plain `VP8 ` file is read from the same header the decoder uses).
  Add a unit test with a VP8X canvas of 512 × 384 around a patched 16383 × 16383 key frame that must be refused by the
  pre-check. Correct the wording in D115 and DESIGN §14.5 ("memory limit" applies only to metadata chunks). Check
  whether a later `image-webp` validates the frame header against the canvas before allocating; keep the pre-check
  either way.
- **Confidence**: high on the order of checks and the allocation (read in the crate source); medium on the exact
  bytes needed per macroblock and the CPU time (not built or measured).

### C2-2: Every insert or name change rebuilds its whole month's names, so large months cost quadratic time under the model's write lock
- **Rating**: Low
- **Location**: `crates/pdmount-core/src/photos/model.rs:273-298` (`place` calls `rename(month)` on every call),
  `:387-412` (`rename`: walks every photo of the month, sorts, allocates two `String`s per named photo, runs
  `unique_name`), `:198-231` (`put` places the photo and each of its related photos), `:344-356` (`set_meta`
  renames the month); callers `crates/pdmount-core/src/photos.rs:304-313` (`PhotosTree::open` puts every stored row,
  one at a time, inside an `async fn` on a runtime worker), `crates/pdmount-core/src/photos/sync.rs:266-285`
  (`apply_items`: a page of 500 under one `model.write()`), `:398-437` (`set_metas` under one `model.write()`);
  `model.rs:531-549` (`unique_name` tries `(2)`, `(3)`, … in order).
- **Actor**: T2 (capture times, names and related links are the uploader's), and ordinary large libraries.
- **Attack / failure**: A month with m photos costs O(m log m) per insert or name, so O(m² log m) to load it. That is
  hit (a) at every remount, when `open` reloads the named rows (before the mount appears), (b) at a timeline refresh of
  a month whose names are known, (c) when names arrive in batches of 50. Months of tens of thousands of photos are
  normal after a bulk import where every file got the import date as capture time; an uploader can also set the same
  capture time on any number of photos, or attach them all as related photos of one main photo (they follow its
  month). With duplicate names (a client that sends a wrong name hash, which the server cannot check), each
  `unique_name` is O(k) for k equal names, so O(k²) per rebuild and O(k³) to load. While `apply_items` or
  `set_metas` holds the write lock, every `lookup` (`photos.rs:747`, write lock) and `getattr` (read lock) of the
  Photos mount waits in a FUSE thread.
- **Impact**: Slow starts of the Photos root (estimated: minutes for a month of about 20 000 named photos at a few
  hundred ns per element and rebuild, not measured) and stalls of the Photos mount while pages or names are applied; no
  effect on data. Below Medium because it needs very large months or many uploads, and ends.
- **Fix**: Let `put`/`set_meta`/`remove` mark months dirty instead of renaming, and rename each dirty month once per
  batch (`apply_items`, `set_metas`, `remove_links`, the load in `open`, which should also run in `spawn_blocking`);
  or keep `Names` incremental (insert or remove one name, reassigning suffixes only among the equal names). Give
  `unique_name` a per-stem counter so duplicates cost O(1) each. Add a test with one month of 50 000 photos that loads
  in bounded time.
- **Confidence**: high on the complexity (read from the code); the times are estimates.

### C2-3: The thumbnail cache has no size bound, outlives logout and reset, and keeps an existing folder's looser mode
- **Rating**: Low
- **Location**: `crates/pdmount-core/src/photos/thumbnails.rs:265-285` (`write_atomic`: `DirBuilder` mode 0700 applies
  only to folders it creates), `:398-424` (three PNGs per photo); `crates/pdmount-core/src/paths.rs:103-107`, `:140`
  (`thumbnails_dir` outside the profile's directories); nothing in `crates/pdmount/src/` removes thumbnails
  (`logout`, `reset`, profile removal); `docs/user-guide.md` §Photos ("They stay when a photo is deleted").
- **Actor**: T1 (other local users, for the folder mode); A3 at rest (decrypted pictures); T2 drives the volume.
- **Failure**: (1) Every photo of every month the user opens gets up to three PNGs: up to 512 px RGB(A), re-encoded
  losslessly, so a few hundred KB per photo (estimated); a library browsed month by month leaves gigabytes in
  `$XDG_CACHE_HOME/thumbnails`, counted against nothing (`cache.max_bytes` covers only blocks), with no free-space check,
  on the same filesystem as staging and the store, where `ENOSPC` fails the user's writes. (2) `logout`, `reset` and
  removing a profile leave the decrypted thumbnails of the whole account behind; D081 accepts that thumbnails sit there
  while the mount is used, but the user guide does not say they survive signing out. (3) When `thumbnails/` or a size
  folder already exists with mode 0755 (made by some other program), it stays so; the files are 0600, but their names
  are the MD5 of a guessable URI (`file://<mountpoint>/<YYYY>/<MM>/IMG_1234.jpg`), so another local user who can
  traverse the cache directory can confirm that a given photo name exists in a given month.
- **Impact**: Disk use outside PdMount's accounting; decrypted image data of an account that was signed out; a small
  metadata leak to other local users under a non-default `~/.cache` mode.
- **Fix**: Remember the files PdMount wrote (a table in the Photos tree's store: URI hash and size), count them against
  a cap (say a `photos.thumbnail_max_bytes`, default a few hundred MB, oldest first), stop writing when the
  filesystem's free space is below the D036 reserve, and delete the remembered files at `logout` and `reset` (and say
  so in the user guide). `chmod` the size folders to 0700 when they are owned by the user and looser, as the
  freedesktop specification asks.
- **Confidence**: high (sizes estimated).

### C2-4: Thumbnail verification relies on the content key, not on the manifest, for binding to the photo (hardening)
- **Rating**: Info
- **Location**: `crates/pdmount-backend/src/proton.rs:1586-1631` (`thumbnail_ref`), `:1681-1735` (`download_thumbnail`);
  `sdk/crates/proton-drive-sdk/src/node/file/block_map.rs:245-279` (`verify_block_listing`),
  `sdk/crates/proton-drive-sdk/src/node/download.rs:124-139` (`build_revision_manifest`), `:182-196`
  (`manifest_verification_keys`).
- **Observation**: The signed manifest is the concatenation of the digests, without lengths, link or revision id;
  `signature_email_address` comes unsigned from the listing, and its keys from the key server for that address, not
  checked against the account's own addresses; the listing's revision id is not compared with the one requested. So
  the manifest alone proves "a listing someone with these keys signed", and it is the decryption with the photo's own
  content key (`download_thumbnail`, an integrity-protected SEIPD packet) that stops another photo's thumbnail from
  passing as this one's. Against T2 nothing is lost (the uploader controls its own photo's thumbnail anyway, and can
  make it show anything, as with the official clients); against a misbehaving server, which is out of scope, an older
  revision's thumbnail of the same photo would still pass, and digest boundaries could be shifted between thumbnails
  and blocks (the SDK checks only that block digests are non-empty).
- **Fix** (cheap): require `hash_digest.len() == 32` for the thumbnail (and, in the fork's `verify_block_listing`, for
  every listed digest), and compare `listing.revision.id` with the revision asked for. Document in D115 that the
  binding to the photo comes from the content key.

### C2-5: Minor hardening (Info)
- `thumbnails.rs:160-186` (`is_valid`): `File::open` follows a symlink and blocks on a FIFO planted at a thumbnail's
  path, which would stall the thumbnail task for good (same uid only, T4; or a non-0700 cache folder, C2-3). Open
  with `O_NOFOLLOW | O_NONBLOCK` and require a regular file.
- `proton.rs:1725` (`content_key.decrypt`): runs on a tokio worker; a compressed OpenPGP packet may inflate to
  proton-rpgp's default `max_reading_size` (50 MB) before the 1 MiB check after it, eight downloads at a time.
  Bounded, but pass a profile with `max_reading_size` of `MAX_THUMBNAIL_BYTES`, or decrypt in `spawn_blocking`.
- `model.rs:145-154`: inode numbers are given for every node ever used and never freed (`inos`, `nodes`), so a
  volume with churn grows them for the life of the tree; small per entry.
- `photos.rs:794-806` (`opendir`): for a complete month the hook builds a `PhotoView` with a formatted path for every
  photo of the month in the FUSE thread, at each open; fine at hundreds, noticeable at tens of thousands (with C2-2).
- Thumbnails go to the `XDG_CACHE_HOME` of the daemon's environment; under a user service that can differ from the
  desktop's (functional, not a security issue).

## Checked and fine (area C2)
- **Panic containment.** `panic = "unwind"` in `[profile.release]` (and `dist` inherits it); decoding, scaling and
  PNG encoding run in `spawn_blocking` under `catch_unwind` (`thumbnails.rs:400-415`), a panic refuses that photo;
  the `spawn_blocking` join errors map to `CoreError::Internal` and the batch is retried with back-off. Only
  allocation failure escapes (C2-1).
- **Byte bounds before decoding.** The listed size is checked before the download, the stream is capped at 1 MiB
  (`proton.rs:1693-1715`), the capacity reserved is bounded by that check, the plaintext is checked again, `decode`
  checks the length, `dimensions()` (0 < side ≤ 1024) and an output buffer ≤ 4 MiB. Plain `VP8 ` and `VP8L` take their
  dimensions from the header the decoder uses (VP8L compares again before decoding, `image-webp` `lossless.rs:105-109`);
  an animation's frame rectangle is checked against the canvas; `read_image` decodes only the first frame. The
  `VP8X` gap is C2-1.
- **Scaling and encoding.** `image` is built without formats and used only for `from_raw` and `thumbnail`
  (`resize_dimensions` keeps both sides ≥ 1; `scaled` never scales up), so no format sniffing or other decoder is
  reachable. The PNG is re-encoded from pixels, so file managers never parse Proton's bytes: this removes the usual
  thumbnailer attack surface for the photos it covers.
- **Verification (D115).** Every thumbnail written has passed: the revision listing's manifest signature over the
  thumbnail digests (sorted by type, so type 1 and type 2 cannot be swapped while both exist) and the block digests,
  with the keys `get_block_map` uses; a missing signature or an empty key list fails
  (`sdk/…/node/download.rs:148-179`); then the download's SHA-256 against the signed digest before decryption, then
  decryption with the photo's own content key. Failures are `Integrity`, permanent, and refuse the photo for the
  session for that revision; transient errors retry with back-off, no tight loop. Only type 1 is used; the 1920 px
  preview would fail `MAX_SIDE` even if relabelled. The address-key lookup escapes the e-mail as a query value
  (`sdk/crates/proton-sdk-rs2/src/keys.rs:57-59`).
- **One thumbnail cannot land on another photo.** The `Wanted` list and the fetched results are zipped in order
  (`buffered` keeps order), each result decrypted with the content key of its own link, and written under the
  `Original` built from that same photo.
- **Writing the cache.** Temporary file: random name, `create_new` (`O_EXCL`, so a planted symlink is not followed),
  mode 0600; `rename` replaces whatever is at the target without following it; new folders 0700. File names are 32 hex
  digits of MD5; reaching another file's thumbnail would need an MD5 second preimage. `Thumb::URI` is built from the
  absolute mountpoint and `YYYY/MM/<shown name>`, where the name passed `validate_name` and has no NUL
  (`model.rs:395`): no `/`, `.` or `..`, at most 255 bytes, valid UTF-8; every other byte, including control
  characters and non-UTF-8 bytes of the mountpoint, is `%XX`-escaped, so the tEXt values are plain ASCII.
  `Thumb::MTime` is the capture time in seconds (negative and far-future values formatted without overflow,
  `thumbnails.rs:150-155`); the revision key is written only when it is printable ASCII of at most 128 bytes.
  `XDG_CACHE_HOME` that is relative or empty is ignored by `dirs` 6 (falls back to `~/.cache`).
- **Names and metadata from the timeline.** Tags: any JSON value, numbers over 255 dropped, bits over 31 dropped
  (`proton.rs:211-235`, `model.rs:54-58`); capture times: integers, saturated beyond `i64`, fractions cut, anything else
  0 (`proton.rs:238-248`); `chrono` 0.4.45 returns `None` for out-of-range timestamps and the local-offset arithmetic
  does not panic; months outside years 0–9999 go to 1970-01 (`model.rs:282-286`); `time_of` uses checked arithmetic.
  Equal names get ` (n)` within 255 bytes, cutting the extension too (D119). Related photos: one level only, a link
  the timeline lists as a main photo stays one, a self-reference is skipped, no recursion or cycles in `put`, `place`
  or `remove`. Names are never logged by the Photos code (only link ids, which the server assigns).
- **Read path.** `PhotosTree::read` goes through the tree's `BlockCache::layout`, i.e. `ProtonBackend::block_map`
  → the SDK's `get_block_map` with the P17 manifest check and per-block hashes, on the Photos clients' drive client and
  credential (`proton.rs:496-502`, `:536-560`), keyed by link and revision.
- **Read-only in every operation.** The kernel mount is `ro`, `nodev`, `nosuid`; `RoFs` answers EROFS to `setattr`,
  `mknod`, `mkdir`, `unlink`, `rmdir`, `symlink`, `rename`, `link`, `create`, `write`, `setxattr`, `removexattr`,
  `fallocate`, to `open` with a write mode or `O_TRUNC`, and to `access` with `W_OK`; the remaining fuser defaults
  (`copy_file_range`, `ioctl`, `lseek`, locks) are `ENOSYS`; folders 0555, photos 0444; no `allow_other`.
- **Store.** `trees/<key>/state.db`: the key is a BLAKE3 hash of volume and root, so no server id becomes a path; the
  store of another volume or root is emptied at open; SQL binds parameters. The table holds plaintext names like the
  engine's store (A3).
- **Test switches.** `pdmount fake photo …` is under `#[cfg(feature = "fake-backend")]`; the fake's WebP encoder is not
  on the shipped decode path.

## Threat model additions
Proposed text for `docs/security/threat-model.md`:

- **A3** gains the decrypted thumbnails PdMount writes to `$XDG_CACHE_HOME/thumbnails/{normal,large,x-large}/`
  (PNG, 0600) for every photo of an opened month, outside PdMount's own directories and not removed by `logout` or
  `reset`; and the Photos tree's store `trees/<key>/state.db` (names, capture times, tags, related links in plaintext).
- **A5** gains a designed exception: the Photos root writes into the freedesktop thumbnail cache, a directory shared
  with every program of the user. Remote input chooses neither the file name (MD5 of a URI under the mountpoint) nor
  its bytes beyond pixels (re-encoded PNG); writes must never follow a link or leave the size folders.
- **T2 for Photos** (the user's own Photos volume, written by the account's devices and apps, or by any client holding
  a session of the account, D113): every timeline field (link ids, capture times of any magnitude or type, tags of any
  value, related links pointing anywhere, including at main photos, at themselves or at nothing), node names (also
  equal names, since the name hash is computed by the uploader), sizes, revisions, the Photos volume's events, and
  thumbnails: the encrypted size, the WebP container and its inner frame headers, the signer address. Concerns:
  decoder resource use and panics (C2-1), paths and URIs built from names, local-time conversion of capture times,
  month-sized work (C2-2), and that a valid signature binds a thumbnail to its uploader, not to the photo's content
  (an uploader can attach any picture as a thumbnail, as with the official clients).
- **T1 for Photos**: the modes of the thumbnail cache's folders (the specification asks for 0700) and the guessability
  of its file names (C2-3).
- **Dependencies**: `image-webp` 0.2.4 (WebP decoder, the exposed parser), `image` 0.25.10 without formats (scaling
  only), `png` 0.18.1 (encoder; decoder for reading text chunks of local files), `md-5` 0.10.6 (cache file names only,
  not a security use). Reviewed in this area: no finding above Medium (C2-1).
- **Out of scope stays**: shared albums (their photos are not in the timeline the tree reads) and a server that lies
  about listings, signer addresses or volumes.
