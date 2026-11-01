## Area 3 (backend crate, test switches in shipped builds, SDK fork P16–P18, scripts): findings

Reviewed: worktree `686ded8` (`m4-start..HEAD` for `crates/pdmount-backend`, `Cargo.toml`, `scripts`, `tests`); SDK fork
`pdmount` `c837e03..caafaea` (P16–P18). Paths relative to the worktree unless prefixed `sdk/` (= fork
`crates/proton-drive-sdk/src/`) or `rs2/` (= fork `crates/proton-sdk-rs2/src/`), both at `caafaea`. Threat model
`docs/security/threat-model.md`. Open Lows of the 2026-10-27 review are not repeated; none of them is made worse by the M4 changes
(see "Checked and fine").

### B3-1: `queue.sh` runs `queue drop --yes --all` and a mount-wide `sync` on whichever profile the scenario runs against
- **Rating**: Low (accident; D028 guard gap)
- **Location**: `tests/scenarios/queue.sh:44-46` (`pd queue drop --yes --all`, `pd sync`); `crates/pdmount-core/src/engine/queue.rs:188`
  (`QueueTarget::All` = every journal inode); `tests/scenarios/run-all.sh:26` (`queue` in the default list); `tests/scenarios/lib.sh:78`
  (`BACKEND` defaults to `proton`, profile `default`, mount `~/ProtonDrive`).
- **Actor**: T5 (the developer or agent running the scenarios).
- **Attack/accident**: `tests/scenarios/run-all.sh` without `BACKEND=fake`, or `tests/scenarios/queue.sh` alone, runs on the real
  account. Every other step of the scenario stays under `$MNT/pdmount-test/queue` (D028), but `--all` discards every queued entry
  of the mount: never-uploaded files anywhere in the mount are deleted, changed files revert to the remote content. `ops.sh:6`
  exits early for `BACKEND != fake`; `queue.sh` has no such guard, and `scripts/ci.sh:68` only happens to export `BACKEND=fake`.
- **Impact**: loss of pending local changes outside `pdmount-test` on the profile the scenario was pointed at (A4, the user's own
  unsynced writes).
- **Fix**: drop with `--all` only on the fake (`[[ $BACKEND == fake ]] || skip`, as `ops.sh` does), or replace `--all` with
  `pd queue drop --yes "$area"` after creating `h` inside `$area`; same for the bare `pd sync`.
- **Confidence**: high (behaviour), medium (likelihood: the real account is a test account, but D028 promises read-only outside
  `pdmount-test`).

### B3-2: `needs_login` decided by a substring of free error text
- **Rating**: Low (hardening)
- **Location**: `crates/pdmount-backend/src/proton.rs:396-402`, `:409` (`USER_KEYS_LOCKED = "none could be unlocked"`, matched with
  `cause.to_string().contains` over the whole chain, after the typed checks); origin `rs2/account.rs:238-246` (an untyped
  `anyhow::bail!`). Consumers: `crates/pdmount-core/src/health.rs:136` (`Unauthorized` → `NeedsLogin`), `:167-171` (every backend
  call refused until a probe succeeds; `engine.rs:716-728`: the probe runs only on `kick`, i.e. a login or `pdmount sync`),
  `crates/pdmount/src/session.rs:418-426` (`needs_login` at start).
- **Actor**: T2/T3 in principle; no in-scope path found (see below).
- **Attack**: any cause whose `Display` carries the phrase classifies the call as `Unauthorized`, so the mount stops talking to the
  server until the user runs `pdmount sync` or `pdmount login`, and the daemon's start treats a resumed session as needing a
  login. Text that reaches a cause's `Display` without a typed error ahead of it in `map_error`: HTTP bodies of malformed replies
  (`sdk/api/mod.rs:298,307`, `sdk/api/storage.rs:139`, `rs2/auth.rs:508-512`: server-controlled, i.e. out of scope with TLS
  intact) and `reqwest` status errors, whose text includes the request URL (`rs2/keys.rs:84-90` `error_for_status`, URL with
  `Email=<signature_email_address>` of a remote revision, `sdk/node/download.rs:182-195`). The email is uploader-chosen (T2) but
  `Url::query_pairs_mut` percent-encodes it (`rs2/keys.rs:58-60`), so the phrase with spaces cannot match. Decrypted names,
  xattrs and mode strings never enter an error `Display` on the paths PdMount calls (`NodeWithSameNameExistsError` is typed and
  checked first; degraded nodes are skipped, `proton.rs:316-327`).
- **Impact**: none shown; a future SDK or fork message containing the phrase, or a reply body on an unverified connection, would
  take the mount into `needs_login`.
- **Fix**: give the fork a typed error for the locked-keys case (`rs2/account.rs:238`, e.g. `UserKeysLockedError`) and downcast it
  in `map_error`, as the other classifications do; keep the string match only as a fallback for the pinned fork revision, or
  drop it.
- **Confidence**: high (no in-scope path), high (fix).

### B3-3: A revision whose block listing fails verification is refetched on every read attempt
- **Rating**: Low
- **Location**: `crates/pdmount-backend/src/proton.rs:186-211` (`block_map`: a failed `get_block_map` leaves the slot empty, nothing
  is remembered); `sdk/node/file/block_map.rs:356-375` (`get_block_map`: `get_secrets`, the paged block listing (`list_all_blocks`,
  one request per 10 blocks), the address-key fetch, then `verify_block_listing`).
- **Actor**: T2 (a device of the account uploading a revision with a missing or wrong manifest signature; a storage-side change
  of a listed hash is caught the same way).
- **Attack**: the user (or `rsync`, `git`, a file manager's previewer) opens the file; each attempt costs the whole listing
  (about 100 requests for a 4 GiB file) before P17 refuses it with `Integrity` (EIO). Readahead and retries by the reader
  multiply this; the SDK's 429 breaker then slows every other call of the mount for 60 s (P12).
- **Impact**: self-inflicted rate limiting of the whole mount while such a file is being read; the read itself fails correctly.
  Bounded by what the reader retries, hence Low.
- **Fix**: remember an `Integrity` failure in the block-map slot for `BLOCK_MAP_TTL` (negative cache) and answer from it; or
  verify before paging the whole listing (the manifest needs all hashes, so a cheaper variant is a per-revision failure
  counter with backoff).
- **Confidence**: medium.

### B3-4: The benchmark builds a `fake-backend` binary at the release target path
- **Rating**: Info
- **Location**: `scripts/bench/m4.sh:55` (`cargo build --release --locked -p pdmount --features fake-backend` →
  `target/release/pdmount`); `scripts/ci.sh:64` (the plain release build writes the same file); `tests/scenarios/lib.sh:135-139`
  (prefers `target/release/pdmount` when present); `crates/pdmount/src/autostart.rs:82` (the autostart entry points at
  `current_exe()`).
- **Actor**: T5.
- **Attack/accident**: after a bench run, `target/release/pdmount` is the test build; a user who installs or autostarts it gets
  `--backend fake:<dir>` and `pdmount fake ...` in the daemon they run. Nothing crosses a uid boundary (the same user could build
  the feature anyway), so this is hygiene, not a hole.
- **Fix**: build the bench binary into its own target directory (`--target-dir target/bench` or `CARGO_TARGET_DIR`), and have
  `lib.sh` prefer the debug build when `BACKEND=fake`.
- **Confidence**: high.

## Checked and fine (area 3)

Backend crate (`git diff m4-start..HEAD -- crates/pdmount-backend Cargo.toml`):
- E14 block buffer: `download_block_into` (`proton.rs:559-623`) refuses a claimed `plain_size` above the caller's buffer before
  any transfer, writes the plaintext into a `Cursor<&mut [u8]>` so a longer block ends in `WriteZero` → `Integrity` (test
  `a_full_block_buffer_is_an_overflow`), and checks `len == claimed` afterwards. The `tokio::sync::Mutex<&mut [u8]>` is only
  there because `authed` may run the closure twice (token refresh), sequentially; the cursor restarts at 0. A partial buffer is
  only ever returned with `Err`, which core discards (`cache.rs`, never caches a failed block).
- `fetch_block` sizes its own buffer at `min(claimed, MAX_BLOCK_BYTES)` (`proton.rs:728-741`); `MAX_BLOCK_BYTES` = 4 MiB.
- Claimed sizes: `file_size` (`proton.rs:217-233`) prefers the sum of block sizes (`saturating_add`, negatives → 0) over the bare
  claimed size (S-5); `blocks()` passes `plain_size` through and core bounds it (`cache.rs:120-135` `Layout::new`, `:1209`) before
  it sizes anything (S-6). `index.max(0)`; `i32::try_from(index)` → `NotFound`.
- `map_error` ordering (`proton.rs:341-404`): conflict types, the four integrity types (including
  `CompletedDownloadManifestVerificationException`, test `integrity_and_unknown_errors_are_classified`), token refresh, `ApiError`,
  429, transfer errors, transport errors, then the D085 string match (B3-2), then `Other`. `local_io` (`lib.rs:204-215`) maps
  only EMFILE/ENFILE to `Transient`; its message holds a staging path, not remote text.
- New logs in the diff: `claimed`/`blocks` numbers at debug, a fixed warning at the D085 match; `?degraded` as before (no key
  material). `log`/`tracing-log` are workspace entries only; `pdmount-backend` does not depend on them.
- `Cargo.toml`: fork pinned to `caafaea` by revision for both crates, `--locked` in CI.

Test code and shipped builds:
- The only feature is `pdmount/fake-backend` (`crates/pdmount/Cargo.toml:14`); no other crate names it, so the workspace release
  build (`scripts/ci.sh:64`, resolver 3, no `--features`) cannot unify it in. `--backend fake:` is refused by
  `BackendChoice::parse` (`daemon.rs:67-80`, `cfg!` is compile-time), which is the only constructor of `BackendChoice::Fake`;
  the child daemon is re-parsed by the same binary (`arg()`); the config file and the control protocol (`control.rs:27-107`) carry
  no backend. `Command::Fake` and its handlers are `#[cfg(feature)]` (`main.rs:109-114,553,662,684,803`). `profile.json`,
  `faults.json` and `PDMOUNT_FAKE_KEEP_REVISIONS` are read only inside `FakeBackend` (`fake.rs:195,262,270,1288`), which a shipped
  daemon never constructs. Core's `fake_fs` is `#[cfg(test)]` (`pdmount-core/src/lib.rs:15-16`); `probe` is `#[cfg(test)]`
  (`main.rs:15-16`); `PDMOUNT_REPRO_*` live in a test module. S-23 (the fake module compiled into every build) stands unchanged.
- Env switches in shipped code: `RUST_LOG` (intended), `PDMOUNT_KEYRING_TIMEOUT_SECS` (`credentials.rs:128-134`, documented in
  `DESIGN.md:1349`; same uid only, shortens a timeout), `DBUS_SESSION_BUS_ADDRESS`/`DISPLAY`/`WAYLAND_DISPLAY` (presence checks).
  None selects a backend or changes what is verified.

SDK fork `c837e03..caafaea`:
- P16: the listed debug logs are gone (`rs2/account.rs` passphrase, `rs2/api/cache.rs` raw keys, `rs2/users.rs` body,
  `rs2/secret.rs` serialized values, `rs2/session.rs` probe body; `sdk/api/{file,links,share,trash}.rs` bodies and request JSON).
  Grep of the fork at `caafaea` for log calls mentioning passphrase/token/key/body/`{:?}`: only key ids, address ids, cache key
  names, session id, counts and URLs with volume/link ids remain (`rs2/account.rs:117,229,299`, `rs2/secret.rs:57`,
  `rs2/session.rs:394,472`, `sdk/api/links.rs:192,213`). `println!` only in a `#[cfg(test)]` keyring test and doc comments. The
  decode helpers that still put a reply body into the error text (`sdk/api/mod.rs:298,307`, `sdk/api/storage.rs:139`) are
  documented as left as is (`docs/sdk-patches.md:198-200`); such bodies are server JSON, not secrets.
- P17: `get_block_map` (`sdk/node/file/block_map.rs:356-375`) selects keys exactly as the full download does (shared
  `manifest_verification_keys`, `sdk/node/download.rs:182-195`: node key without `SignatureEmail`, else the address's
  non-compromised keys) and calls `verify_block_listing` (`:245-283`) before any map exists; failures return. `verify_manifest_signature`
  fails on a missing or empty signature and on an empty key list (`download.rs:148-181`), so an unknown email or a key list
  filtered to nothing cannot pass. `verify_block_listing` rejects an empty `hash_digest` first (S-11) and builds the manifest
  over thumbnail hashes by type then block hashes in listing order (a reordered, dropped or altered entry fails: tests
  `a_tampered_block_hash_fails_verification`, `a_dropped_thumbnail_or_block_fails_verification`,
  `an_empty_block_hash_fails_verification`). The block-size fallbacks (`decrypt_claimed_block_sizes`, `listed_claimed_block_sizes`)
  run only after verification and affect sizes, not hashes. `download_verified` uses the hash the caller passes (the verified
  map's) and refreshes only URL/token on expiry (`sdk/block/download.rs`). PdMount reads blocks through `block_map()` only
  (`proton.rs:559,728`; no use of `download_range`, `download_block_at`, `refresh_urls` or `BlockMap::from_parts`), maps both
  exceptions to `Integrity`, and caches a map only on success (`proton.rs:202-209`). S-4 and S-11 are fixed in the final tree.
- P17's keys decoding (`rs2/api/response.rs:219-272`, `rs2/account.rs:160-186`): `Address.Keys` only, `Unverified` and `CatchAll`
  ignored, `Flags & 1` required (absent flags accepted, matching the previous behaviour).
- P18: `with_block_transfer_parallelism` replaces the block semaphore only; the `assert!(limit > 0)` is reached with the constant
  `TransferLimits::BLOCKS = 5` (`crates/pdmount/src/session.rs:264,276-280`), not with config input.

Scripts (`git diff m4-start..HEAD -- scripts tests`):
- S-7 intact: `ensure_dataset` removes only `$DATASET/pdmount-m1` and refuses a non-empty `DATASET` without it (`lib.sh:178-190`);
  `remove_fake_dir` removes only an empty directory or one with `snapshot.json`/`.seeded*`, never a symlink or a file
  (`lib.sh:64-76`); used by `lib.sh:201`, `latency.sh:27`, `read.sh:38`, `m4.sh:121`. Self-tested by `selftest-lib.sh` in CI.
- S-8 intact: `private_root` does `mkdir -p -m 0700`, refuses a symlink or another uid's directory, then `chmod 0700`
  (`lib.sh:50-58`); applied to `SCENARIO_ROOT` for both backends (`lib.sh:88`, so real-account `$work` is private too),
  `FAKE_DIR` in the bench scripts, `BENCH_ROOT` and the bench `FAKE_DIR` in `m4.sh:58,66,118,122`; `/dev/shm` is sticky, so a
  checked directory cannot be swapped afterwards. `soak.sh` goes through `lib.sh`.
- `m4.sh`: `BACKEND=fake` forced (`:59`); writes under `$MNT/pdmount-test/bench`; every `rm -rf` targets `$area/...`,
  `$local_root/...`, `$BENCH_ROOT/...` or an `mktemp -d` under `BENCH_ROOT`; the git.git mirror is cloned over HTTPS into
  `BENCH_ROOT` (`:155-159`, hooks and config are not transferred by `clone`), checked out by tag into the mount and the local
  root; Python runs with `-I` and arguments, never from inside downloaded data; `report_dir` under `target/`.
- `make-untar-dataset.sh`: writes only the named tar; `python3 -I`; deterministic content.
- `ops.sh`: fake only; edits only the test profile's config and restores it. `queue.sh`: see B3-1.
- `read.py`/`ops.py`: new JSON output paths are the caller's; no shell interpolation.
- `ci.sh:82-83` `rm -rf "$XDG_RUNTIME_DIR"` still relies on `lib.sh` having set it under `SCENARIO_ROOT` (`BACKEND=fake` is
  exported at `:68`); as noted in the previous review.
- Open Lows of 2026-10-27 touching this area (S-10 `Retry-After`, S-12 mode bits, S-23 fake module, S-24 empty name hash,
  S-25 5xx replay): the M4 diff does not change the relevant code paths (`proton.rs:244-248,311-313,436-438,840-842`).
