# TODO

Last updated: 2026-10-22. M2 done. M3 started: plan in `docs/plans/m3.md` (D039), M3.0 done. Next: M3.1 (Opus), after the user logs in profile `second`.

## Next action

- **User:** log in profile `second` (a real session; the user opens the link):
  `cargo run -p pdmount -- --profile second --force-file-secrets login --no-browser`.
- **Then M3.1, in an Opus session:** fork patch P14 and the Q1/Q6 probe on the real API (`docs/plans/m3.md` M3.1).
  Its outcomes pick the branches of M3.9, M3.11, M3.12 and M3.13. M3.2–M3.11 do not depend on it and may start in
  parallel sessions only if the user says so; the plan's order is sequential.
- Every M3 task: one Sonnet session, the task text in `docs/plans/m3.md`, `scripts/ci.sh`, docs updated, one commit
  `M3.n: <title>` (D039).
- Rerun the real-account matrix after changes to the write or sync path (command in Handoff notes). The trash may be emptied when less than 1 GiB is free (D036).

## Milestones

Scope and exit criteria: DESIGN.md §16 (the only source; scenario IDs are defined in §15.3). Tick a box here only
when every exit criterion in §16 passes.

- [x] M0 Foundations (2026-10-19; browser opening at login waived, see Handoff notes)
- [x] M1 Read-only mount (2026-10-19; results under Done)
- [x] M2 Writes, single device, access control (2026-10-22)
- [ ] M3 Multi-device and sync robustness
- [ ] M4 Performance and operations
- [ ] M5 Upstream patches and crypto migration (upstream submission deferred, D023)

## M0 tasks (in order)

- [x] M0.1 Decide where the SDK fork is hosted (Q4): local bare repo `<fork>` (D022). Created, with branch `pdmount` and tag `base/0.4.0`.
- [x] M0.2 All patches P1–P12 applied: one branch per patch, each with its own commit and tests. Merged into fork `pdmount` (`9176b0f`).
  The combined branch passes 236 SDK tests (baseline 150) and pdcli's tests, and passes `cargo fmt --check`.
- [x] M0.3 Workspace with `pdmount-core`, `pdmount-backend` and `pdmount`, `[patch.crates-io]` pinned to fork `9176b0f`, and clippy config.
  `pdmount-fuse` is created in M1 together with its first code.
- [x] M0.4 `Backend` trait, `FakeBackend` and Proton adapter:
  - The fake backend models name uniqueness, base-revision and name-hash preconditions, trash, quota, paged events with
    `refresh`, fault injection, and clones sharing one drive. It has 10 tests.
  - The Proton adapter implements the read side. Writes return `Unsupported` until M2.
- [x] M0.5 `login` (browser and/or printed link), `logout`, `status`. The session is stored in the keyring, with a 0600 file
  fallback and a 20 s keyring timeout (D024, superseded by D029). The SDK caches are encrypted.
  Verified live by the user: sign-in through the printed link for profiles `default` and `second`.
- [x] M0.6 `doctor`: checks /dev/fuse, fusermount3, fuse.conf, config, keyring, login, disk space, clock and version. On this
  machine (sandbox user): no FAIL; warnings for fuse.conf (D020) and the keyring (no session bus, file fallback D024).
  Since D029 the sandbox user runs it with `--force-file-secrets`; without a flag the keyring check fails there.
- [x] M0.7 `scripts/ci.sh`: fmt, clippy `-D warnings`, 50 tests, release build. Passes. Fake-backend mount tests are added in M1.

## M1 tasks

- [x] M1.1 Store: rusqlite (same version and features as the SDK), WAL, writer thread with group commit and per-write
  savepoints, full §13.2 schema as migration 1, downgrade refused. Paths `state.db`, `blocks/`, `staging/`.
- [x] M1.2 Inode tree and engine: listings applied as diffs (moves, renames, removals, invalidations), stable inode numbers
  from a durable reservation, readdir snapshots, nlookup/forget, hot-path guard and counters, event cursor on the first
  mount, startup revalidation (D025).
- [x] M1.3 Block cache: layout from declared sizes or 4 MiB blocks, RAM LRU, encrypted disk cache with eviction,
  single-flight downloads with per-class permits, readahead, listing and small-file prefetch. The Proton block-map memo is
  bounded and fetches once per revision.
- [x] M1.4 `pdmount-fuse`: read-only `Filesystem`, slow path with deadlines and a 64-request bound, `catch_unwind`, kernel
  capabilities and TTLs, `FOPEN_KEEP_CACHE`/`FOPEN_CACHE_DIR`, notify thread.
- [x] M1.5 CLI: `mount [--foreground]` (D026), `unmount [--wait|--force]`, `stats`, `reset --keep-login`, control socket,
  `status` with mount state. Smoke-tested on the real account (empty drive), including kill -9 → `unmount --force` → mount.
- [x] M1.6 Tests: 96 unit tests, plus a mounted fake-backend test (rsync twice, git clone and fsck, EROFS, remount
  with the same inode numbers) that runs in `scripts/ci.sh`.
- [x] M1.7 Dataset generator and scenario scripts `tests/scenarios/{r6,g6_read,sha,kill9}.sh` (`VERIFY_MODE=live|fresh`).
- [x] M1.8 Fixes from the real-account run:
  - Speculative API work is paced and yields to user requests (D027). Before, prefetch caused HTTP 429, and four
    `opendir` calls passed their deadline.
  - The daemon exits after an unmount even when another mount namespace holds a copy of the mount (a Flatpak app did;
    DESIGN.md §14.4).
  - `unmount` waits for the daemon process, and `mount` refuses to start while it runs.

## M2 tasks (in order)

Scope and exit criteria: DESIGN.md §16. Real-account writes only under `My Files/pdmount-test/` (D028).

- [x] M2.0 Reproduce the D024 keyring hang in this environment before D024 is accepted (user request, 2026-10-20).
  `scripts/repro/keyring-d024.sh`: hang reproduced, fallback after 20 s, migration back works. Results in D024 Refs.
- [x] M2.0b D029: `--allow-file-secrets` / `--force-file-secrets`, three-state keyring answers, the file-wins rule,
  no migration while another daemon runs, cache reset on a new key, `login` ends an unstored session, `logout` reports
  leftover keyring entries, the daemon saves only changed tokens, `mount` reads the session before starting the daemon,
  `doctor` reads the lock state without prompting. 11 unit tests; the harness cases s0–f3 pass.

- [x] M2.1 Persistable fake: the state is saved to a directory, and `pdmount mount --backend fake:<dir>` exists in test
  builds only (§15.1). Scenario scripts gain `BACKEND=fake` and the `remount` and `crash` verify modes (§15.3).
  `scripts/ci.sh` runs r6, sha and g6_read in all four modes plus kill9 on the fake.
- [x] M2.2 Staging (§7.1, §7.2): staging files with a sidecar (base revision, materialized blocks), snapshots, reads of
  dirty inodes (§6.5), `FUSE_ATOMIC_O_TRUNC` without fetching old content (`crates/pdmount-core/src/staging.rs`).
- [x] M2.3 Local-first namespace operations (§8.1–§8.6): create, mkdir, rename (NOREPLACE; EXCHANGE/WHITEOUT EINVAL),
  unlink and rmdir with orphans and tombstones, setattr (mode, mtime, size), listing merge that keeps local changes.
  Per-inode staging locks; the table stays one `RwLock` (§4.3). Daemon-side permission checks (D031).
- [x] M2.4 Journal (`crates/pdmount-core/src/journal.rs`): reconciliation, dependencies, name reservation and swaps,
  debounce, upload job with snapshot, SHA-1 adoption after a crash, atomic-save adoption, `own_revisions`, `fsync=local`,
  quota pre-check and deferred errors, backoff and parking, `unmount` drain, `reset` refusal while pending.
- [x] M2.5 Proton adapter writes: `upload_new`, `upload_revision` (with the expected SHA-1), `create_folder` (P5),
  `rename_node` / `move_node` with a new name (P10), `trash_nodes`; P6's revision uid. Run against the real API in the M2.8 matrix (2026-10-22).
- [x] M2.6 `.pdignore` (§7.7): profile file plus `/.pdignore`, reloaded when it changes; local-only content in staging.
- [x] M2.7 Access control (§11): rules bound to inodes, EROFS (D019), masked write bits, runtime changes with kernel
  invalidation, orphaned rules, config import once per path, `pdmount access set|unset|list|effective`.
- [x] M2.8 Tests:
  - [x] Journal property tests at 10k cases (§15.1): `PROPTEST_CASES=10000 cargo test -p pdmount-core proptests`, about
    140 s; 50 000 cases passed. Restarts and crashes run each engine in its own runtime.
  - [x] Scenarios R1–R7, G1–G7, N1 and A1–A6 in `live`, `remount`, `crash` and `fresh` on the fake
    (`BACKEND=fake tests/scenarios/run-all.sh`, in tmpfs, 13 min). 2026-10-20: 92 of 92 pass (after D032).
  - [x] The same in `live`, `remount` and `fresh` on the real account, under `pdmount-test/`: one run at a time, stop at the
    first HTTP 429, modest G2/G3 datasets (D030).
    - [x] Calibration slice (2026-10-20, user's OK): R1 (`R_FILES=200 R_BIG_MIB=20`) and G3 (`G3_COMMITS=50`) in
      `live` pass; no 429. R1: 201 uploads (21.5 MiB) drained in 318 s; G3: 265 uploads in 531 s (about 0.5 uploads/s,
      bound by per-file round trips). Zero hot-path calls, panics and expired deadlines.
    - [x] The full matrix (2026-10-21 21:25 – 2026-10-22 00:30 CEST, user's OK): R1–R7, G1–G7, N1, A1–A4, A6 in
      `live`, `remount`, `fresh`, 60 of 60 pass, 3.1 h of runs; no HTTP 429, no panics, expired deadlines or hot-path
      calls; the trash never needed emptying. One transient failure, rerun and passed: G7
      `remount` got EIO on reads for a few minutes after its fresh remount (`git fsck` passed, `git status` failed),
      next to the only malformed API reply in the log (an event page without `EventID`); minutes later everything read
      back intact. The daemon did not log why it answered EIO; slow-path EIO answers are now logged at warn level.
      Logs: `target/m2.8/ra{,2}.out`, `target/m2.8/ra{,2}/`.
- [x] M2.9 pjdfstest and fsx, over the fake backend only, never the real account (D030), on tmpfs to spare the disks
  (user request 2026-10-20):
  - [x] Test data in tmpfs: one root variable in `tests/scenarios/lib.sh` (`SCENARIO_ROOT`, default `/dev/shm/pdmount-tests`
    when `/dev/shm` exists, else `~/.cache/pdmount-scenarios`) for the fake drive, datasets and mountpoint; the test
    profile's state (state database, staging, block cache, log) follows through the XDG variables; a small
    `cache.max_bytes` in that profile's config. `/dev/shm` is a 16 GB tmpfs (31 GB RAM).
  - [x] Fake persistence without full rewrites: changes go to an append-only log next to a snapshot that is compacted
    now and then, instead of rewriting `state.json` (15 MB after one scenario matrix) on every change. Every change is
    still on disk before the call returns, because the fake lives in the daemon process (crash mode).
  - [x] Fake growth bounded: keep the last N revisions per file (configurable) and delete their blobs; drop the content of
    trashed nodes; trim the change and event logs with sequence numbers, so `scenario_area`'s mark keeps working.
    Before, the fake kept every revision (1.8 GB, 23 400 blobs after one matrix).
  - [x] Build pjdfstest and fsx from source as the sandbox user in `<tools dir>/` (Handoff notes).
  - [x] The pjdfstest subset (§15.1): `tests/pjdfstest.sh`, 149 test files, 771 assertions pass; every failure is listed
    with its reason in `tests/pjdfstest.list` (other users, unsupported types, directory nlink, quota ENOSPC). It found
    three bugs, fixed: atime was not kept (§8.5), an open unlinked file reported nlink 1, ftruncate through a writable
    handle checked the mode again (EACCES for a file created with mode 0).
  - [x] fsx with mmap for 6 h: `tests/fsx.sh` (two instances, 256 KiB and 8 MiB files). The first run (2026-10-20
    23:44 CEST) found a race: the 256 KiB instance passed (7 356 225 operations), the 8 MiB one died of SIGBUS after
    about 5 h 44 min. A write or truncation released the staging lock before it raised `content_gen`, so an upload
    commit in between marked the file clean and dropped the staging: the write was lost and the size outgrew the
    revision (short reads, so the kernel cut the size and mmap faulted). Fixed (DESIGN.md §4.3, §7.4) with the test
    `writes_racing_upload_commits_are_kept`, which lost writes on every run before the fix. The second run (06:53) filled
    the 16 GB tmpfs after 13 min: the SQLite WAL had grown to 14 GB (database 4.8 MB), because under steady writes a
    passive checkpoint never catches up with the writer, so the WAL never starts over. Fixed with a `TRUNCATE`
    checkpoint once 64 MiB is left after a passive one (DESIGN.md §4.3), test `steady_writes_keep_the_wal_bounded`;
    the WAL now stays under about 130 MB under fsx. Run on the sync thread's own connection, that checkpoint lost row
    writes in turn ("store write failed: database is locked", 1 581 times in the third fsx run, which was stopped; the
    matrix's one failure, G2 `fresh`, a row left pending): the writer's transactions got `SQLITE_BUSY` while it held
    the database. `BEGIN IMMEDIATE` fixed the unit test but the matrix still logged 35 such errors, so the `TRUNCATE`
    checkpoint now runs on the writer between two batches, asked for by the sync thread (§4.3); the writer's
    transactions are deferred again, as before. The test now also reads before writing, runs a concurrent reader,
    idles now and then and counts the rows (it lost 43–66 of 10 000 with the checkpoint on the other connection).
    The fifth run with the same seeds (1792532643/4, 2026-10-21 08:24–14:24 CEST) passes: 293 720 176 and 77 959 489
    operations, no panics or expired deadlines, no hot-path network calls, no store errors; the WAL stayed at 91 MB.
  - The first three items also took the scenario matrix (M2.8) off the disk (DESIGN.md §15.1, §15.3).
- [x] M2.10 Event-based sync, brought forward from M3 so that PdMount follows the official SDK's operational
  requirements before the real-account matrix (D033). The backend side exists: `latest_event_id`, `poll_events` with
  `more`/`refresh` in the Proton adapter and the fake; the first mount stores the cursor (`event_cursor`) and uploads
  record `own_revisions`.
  - [x] Block downloads capped at 5 over all priorities, one below C#'s 6 (`DOWNLOADS` in `cache.rs`; test
    `at_most_five_blocks_download_at_once`).
  - [x] `sync.poll_interval` (default and minimum 30 s) replaces the unused `poll_active`/`poll_normal`/`poll_idle`.
  - [x] Event loop (§9.2, `crates/pdmount-core/src/events.rs`): resumes from the stored cursor at mount (startup
    revalidation is gone); polls every `poll_interval`, drains `more` at once; after errors waits the interval times
    1, 1, 2, 3, 5, 8, 13 with jitter, or a longer `Retry-After`; the cursor is queued after the page's rows.
  - [x] Applying events (§9.3, D034, accepted 2026-10-20): batched `get_nodes` for known links or children of listed
    directories; echoes skipped; the listed directories a change touches are listed again, a directory a node left
    after the one it entered, so remote changes go through `Tree::apply_listing_at` (moves keep inodes and listed subtrees).
  - [x] Echo suppression (§7.5): a node equal to the tree's remote state (parent, name, active revision) costs no
    listing; uploads in flight are protected by `remote_stamp` and pending content.
  - [x] Dirty or pending inodes: the listing rules keep local changes (content conflicts park at upload, remote deletes
    resurrect); M3's conflict engine refines this.
  - [x] Refresh (§9.4): in the background, events keep flowing from the fresh cursor, the cursor is stored only after a
    successful pass, a refresh asked for meanwhile repeats it. D025 superseded by D034.
  - [x] Tests on the fake: 10 event tests (every kind, moves between listed directories, echoes, local changes kept,
    pages and cursor across a restart, refresh, events during a refresh, back-off steps, listing order); the property
    tests with another device's write/create/rename/trash at clean points (10 000 cases pass; they found two bugs, see
    D034); scenario E1 (`pdmount fake rename|trash|write` while mounted; visible after 28–29 s) in all four modes.
  - [x] Real-account check of the event stream (2026-10-20, no 429): one log line per applied page (kinds, resolved,
    echoes, listings). It found that `get_nodes` answered from the SDK's entity cache, stale after revisions and other
    devices' changes (F14); fixed by fetching each node fresh (D035, accepted; batched by P13). After the fix every own change of the
    check and of the calibration (436 creates, a rename, an overwrite) was an echo; a trash still costs one listing.
    Event kinds: Create, UpdateMetadata for a new revision, UpdateContent for a rename and a trash (§9.3).

## M3 tasks (in order)

Specifications: `docs/plans/m3.md` (one task per session, one commit per task, D039). Exit criteria: DESIGN.md §16.

- [x] M3.0 Plan into the repo, decisions D037–D039, SLO amendment (§16), this checklist; tag `m3-start`
- [ ] M3.1 Fork patch P14 and real-API verification of Q1/Q6 (Opus; needs profile `second` logged in)
- [ ] M3.2 Error classification fixes in the Proton adapter (connection errors, body 401, block downloads)
- [ ] M3.3 Fake groundwork: clients, pause points, check toggles, revision metadata, fault file, CLI
- [ ] M3.4 Conflict naming (pure function)
- [ ] M3.5 Device identity, clock and namer in the engine and tree
- [ ] M3.6 Conflict registry, migration 0003, persisted `remote_newer`
- [ ] M3.7 Tree resolution primitives
- [ ] M3.8 Listing rules: layer 1, remote deletes, rename/rename, tombstone revival, name clashes (D040)
- [ ] M3.9 Upload path: layers 2 and 3, identical content, metadata-only races, edit/edit split
- [ ] M3.10 Create/create (files and folders)
- [ ] M3.11 Namespace executor: rename/rename, move cycles, local delete vs remote edit, folder delete
- [ ] M3.12 P14 wiring in the adapter and the M3.1 error codes
- [ ] M3.13 Revision post-check (only if M3.1 finds `CurrentRevisionID` not enforced at commit; D043)
- [ ] M3.14 Surfacing: `conflicts` CLI, status, xattrs, notification hook
- [ ] M3.15 Health: connectivity state, offline mode, probe (D041)
- [ ] M3.16 Auth: reactive 401, rotation, proactive refresh, `needs_login`, daemon login, `mount` login (D042)
- [ ] M3.17 Property tests: two devices plus an app; "no acknowledged content unreachable", "two cores converge"
- [ ] M3.18 Scenarios: `second` mode, `m1`…`m7`, `e2`, CI
- [ ] M3.19 Soak script and the 24 h run on the fake
- [ ] M3.20 Real-account runs: `second` mode and selected M scenarios (user-gated, D030)
- [ ] M3.21 Exit check and documentation consistency pass (Opus)

## Open questions

| ID | Question | How to resolve | Milestone |
|---|---|---|---|
| Q1 | Does the server enforce `CurrentRevisionID` (new revision) and `OriginalHash` (rename/move)? At draft creation or at commit? **Partly answered:** draft creation enforces `CurrentRevisionID`. The API schema documents a 409 ("submitted revision is no longer up to date or another draft is open") for `POST …/files/{linkID}/revisions` (`cs/client/js/src/internal/apiService/driveTypes.ts` ≈9655). Still open: commit-time enforcement and `OriginalHash`. | Test against the real API with two profiles | M3 |
| Q2 | ~~Is there an API to update a folder's XAttr (mtime) after creation?~~ **Resolved: no.** Folder XAttr can only be set at creation. Rename has no XAttr; XAttr updates exist only for photo revisions (iOS-only) and albums (`driveTypes.ts` 4950–4980, 1782–1794, 3726–3736). D013 stands. | — | M2 |
| Q3 | ~~Does git's object finalization fall back from `link()` to `rename()` on EPERM?~~ **Answered by test:** G2, G3, G4, G6 and G7 pass on the fake mount, where `link()` returns EPERM (git 2.55). | — | M2 |
| Q4 | ~~Where is the SDK fork hosted?~~ **Resolved:** local bare repo `<fork>` (D022) | — | M0 |
| Q5 | Crypto migration timing, and how the community SDK will follow | Watch `cs/README.md`/changelogs and the upstream repo | M5 |
| Q6 | Which server error codes signal a stale revision or name hash? **Partly answered:** draft conflicts are HTTP 409 / code 2500 with `Details {ConflictLinkID, ConflictRevisionID, ConflictDraftRevisionID, ConflictDraftClientUID}` (C# `RevisionConflict.cs`). P11 maps these to `RevisionDraftConflictError` / `NodeWithSameNameExistsError`. Still open: the codes for stale `OriginalHash`. | Real-API tests with Q1 | M3 |
| Q7 | Rate-limit thresholds for many small uploads (git, untar). **M1 data point:** prefetch at concurrency 4 got HTTP 429 within seconds; a cold rsync of 1000 small files (about 2 API calls each, unpaced) took 290–373 s without errors | Measure with real-account runs of G2/G3; tune the upload concurrency and debounce | M2/M4 |
| Q8 | **Deferred (D023); don't raise it until GitHub use resumes.** How are the fork patches submitted upstream (GitHub fork, or `git format-patch` to the maintainer)? | User decision | M5 |
| Q9 | ~~Should a daemon crash keep acknowledged changes that were not fsync'd (G5 `crash`)?~~ **Resolved (D032):** option (b), replies wait for the store commit (no fsync); measured within the §5.7 SLOs. | — | M2 |

## Handoff notes

<!-- Rewrite at the end of every session. Keep it short. -->

- **M3 planning (2026-10-22, Opus):** the plan was made from a code exploration of the conflict, auth/offline and test
  code; its findings (bugs and gaps with file:line) are in the plan's Context and in each task. Notable: connection
  errors probably map to `Other`, not `Offline` (M3.2); `timeouts.offline_read` is never read; the `conflicts` table
  and `inodes.remote_newer` exist but are unused; the fake has no pause points or faults for a running daemon.
- **Deviations from docs/plans/m3.md:** none yet. Record each one here and in the task's commit body.
- **Review:** the user reviews M3 with Opus at the end: `/code-review` over `m3-start..HEAD`.

- **Done this session (2026-10-20/21):**
  - D034 accepted by the user (`dae0dec`).
  - Committed after `dae0dec` in one commit: M2.9 (pjdfstest, fsx and their fixes) and the M2.10 real-account check
    (D035 proposed); full `scripts/ci.sh` and the property tests at 10 000 cases pass on it.
  - D035 checked against `cs/README.md`'s operational requirements: no rule broken, but one request per node where the
    official SDKs batch. Fork patch P13 (`feat/fresh-node-batch` `7b4dcf9`, merged into `pdmount` `4548aa7`, pinned):
    `get_nodes_uncached`, 50 links per `get_details`; the adapter's `get_nodes` uses it; D035 amended, then accepted by the user.
    Run against the real account in the M2.8 matrix. The fork's files mix CRLF and LF: edit them with an editor that keeps line
    endings (a Python rewrite turned `node/mod.rs` into a whole-file diff).
  - D036 (trash may be emptied when storage runs low; `pdmount trash empty --yes`, `TRASH_MIN_FREE`), the `STOP_ON`
    guard, then the full real-account matrix: 60/60 (G7 `remount` passed on its rerun after a transient EIO, M2.8).
    Slow-path EIO answers are logged at warn level since (`pdmount-fuse` `slow`). M2 ticked.
  - **M2.9:** pjdfstest (`<tools dir>/pjdfstest`, upstream `85a8aea`, `autoreconf -ifs && ./configure && make pjdfstest`)
    and fsx (`<tools dir>/fsx/fsx`: xfstests' `ltp/fsx.c` compiled alone with `<tools dir>/fsx/config.h`, a hand-written
    config with `HAVE_*` defines plus `assert.h`, `getopt.h`, `sys/param.h`). `tests/pjdfstest.sh` copies the suite to
    `SCENARIO_ROOT`, cuts the file-type loops to regular/dir, symlinks the binary (`/dev/shm` is `noexec`) and compares
    each test file's failures with `tests/pjdfstest.list`; regenerate the list only after reviewing new failures.
    Fixed: local atime (`Node::atime`, `inodes.atime_ns`, 0 = follows mtime, no migration), nlink 0 for orphans,
    `SetAttr::handle` for ftruncate. fsx: `tests/fsx.sh`; `-c` needs root (`drop_caches`).
  - **fsx 6 h run:** `SCENARIO_ROOT=/dev/shm/pdmount-fsx PROFILE=fake-fsx PDMOUNT=<tools dir>/bin/pdmount-fsx tests/fsx.sh`
    (a copy of the debug binary, so rebuilds do not touch it); the passing run's output is in `target/m2.9/fsx-6h.out`
    and `target/m2.9/{small,large}.out`, the failed first run in `target/m2.9/fsx-6h-sigbus.{out,log}`. fsx dies of an
    mmap SIGBUS without writing its `.fsxlog`; the daemon's warning "block sizes do not add up to the file size" was
    the clue (a revision paired with a newer size). `tests/fsx.sh` leaves the mount up at the end: unmount it (with
    the run's `XDG_*_HOME`) before deleting its `SCENARIO_ROOT`, or `rm -rf` deletes through the mount. Watch
    `state.db-wal` and `df /dev/shm` during long runs.
  - **M2.10 real-account check:** found and fixed F14/D035 (stale SDK entity cache in `get_nodes`); per-page log line
    "event page applied" and a debug line "remote change" (known vs remote rev/parent/name) in `events.rs`; see
    `RUST_LOG="info,pdmount_core::events=debug"`. Logs in `target/m2.10/`.
  - **Calibration slice:** results under M2.8. The fake matrix (96/96, 16.9 min), CI and the property tests at 10 000
    cases pass on the final code.
- **Real-account load:** about 0.5 uploads/s; echo checks of created nodes cost one `get_details` per 50 nodes (P13;
  before, one per node: 437 calls during calibration). The 60-run matrix met no 429.
- **Pitfalls found this session:** `grep 429` over the daemon log matches revision ids and sizes; grep
  `status: 429|RateLimited` instead. `pkill -f` with a pattern from the agent's own command line kills the agent's shell
  (exit 144). `date -d HH:MM:SSZ` takes today's date in the local zone, which is a day ahead of UTC after midnight.
  Run `pdmount` for a test profile through `pd` in `tests/scenarios/lib.sh` (it sets the `XDG_*_HOME` variables):
  a bare `pdmount --profile fake-scenarios unmount` looks for the socket in `~/.local/share`, takes the live daemon
  for stale and unmounts it lazily, and a scenario started at that moment fails ("not mounted").
- **Bench pitfalls:** keep the fake drive on tmpfs (`FAKE_DIR`) when measuring the disk, since a real backend writes
  nothing locally. A btrfs profile directory grows with the block cache across runs, so compare runs from one session.
  perf and strace are not available to the sandbox user. Sampling `/proc/<pid>/task/*/{stat,wchan}` found the
  descriptor-table stall (`expand_files`, `__wait_rcu_gp`).
- **Tmpfs layout:** fake runs keep everything under `SCENARIO_ROOT` (`/dev/shm/pdmount-tests`), including the test
  profile through `XDG_*_HOME`, so its daemon log is `$SCENARIO_ROOT/xdg/state/pdmount/fake-scenarios.log` and is gone
  after a reboot. To inspect the profile by hand, source `tests/scenarios/lib.sh` with `BACKEND=fake` and use `pd`. The
  old on-disk fake data (`~/.cache/pdmount-scenarios/fake-*`, `m1-nobig`, the `fake-scenarios` profile dirs) was deleted;
  only the real dataset copy `m1` remains there. The fake's old `state.json` is still migrated on open.
- **Real-account matrix (D030; passed 2026-10-22):** from the repository root, in the background, after
  `scripts/ci.sh` (it builds `target/release/pdmount`) and with nothing mounted at `~/ProtonDrive`:
  ```sh
  SCENARIOS="r1 r2 r3 r4 r5 r6 r7 g1 g2 g3 g4 g5 g6 g7 n1 a1 a2 a3 a4 a6" MODES="live remount fresh" \
    R_FILES=200 R_BIG_MIB=20 G3_COMMITS=50 LOG_DIR=target/m2.8/ra tests/scenarios/run-all.sh \
    > target/m2.8/ra.out 2>&1
  ```
  R4 (48 MiB) and G4 (about 45 MiB) must exceed 40 MiB. `STOP_ON=429,fail` (default) stops after a run with a 429 in
  its own daemon log lines (the log has 25 old ones from the M1 prefetch on 2026-10-19) or after a failed run; restart
  from there with `SCENARIOS`/`MODES`. `TRASH_MIN_FREE` (1 GiB) empties the trash before a run (D036). The runs take
  about 3 h; G3 and G5 `fresh` are the longest (15–17 min).
- **Property tests:** keep their state in `/dev/shm` (disk fsyncs made a case 1.4 s); restarts and crashes run each
  engine in its own tokio runtime and wait until the old store writer is gone (two writers on one SQLite file lose
  commits). They found four real bugs (plan race → entry generations; stale listings → remote stamps; every new folder
  listed; listings overwriting pending files' mode/mtime).
- **Scenario pitfalls:** the fake's change log is shared by all scenarios, so temp-name checks look only at changes
  numbered after `scenario_area`'s mark (`fake changes --after`; numbers survive trimming); git's detached auto-maintenance is off in `git_env`; `status` exits 1 without a session (always for
  the fake), so never pipe it under `pipefail`; `pkill -f <pattern>` kills the agent's own shell, and `until ! pgrep -f
  <pattern>` never ends, when the pattern is in the command line.
- **Secrets mode (D029):** the sandbox user has no session bus, so every agent command that reads secrets needs
  `--force-file-secrets` (the scenario scripts pass it by default, `SECRETS=force`). The profile `default` session is
  already in files there.
- **Keyring harness:** it uses `target/keyring-d024/` because gnome-keyring's control socket must fit the 108-byte Unix
  socket path limit; the scratchpad path is too long and fails with a misleading "Address already in use". Set `HOME`
  and the XDG variables before `dbus-run-session`, or the D-Bus-activated daemon uses the real home. `secret-tool` cannot
  show the hang: libsecret refuses to write to a locked collection instead of prompting.
- **Real account:** `<test account>`. The dataset `pdmount-m1/` (from `make-m1-dataset.sh`) is in My Files; keep it
  for later milestones. Tests write only under `pdmount-test/` (D028). The local copy is in `~/.cache/pdmount-scenarios/m1`.
- **Flatpak sandboxes copy the mount.** A Flatpak app did. The copies show ENOTCONN after an unmount until
  the app exits, and an unmount then takes the 10 s grace period.
- **`cs/` sources:** `<official SDK checkout>/` is a clone at tag `js/v0.21.0` (`e6a661f5`, cs 0.24), chosen because every existing
  `cs/` line citation matches there.
- **GitHub postponed (D023):** no PRs, forks, issues or hosted CI. A read-only `git fetch upstream` is allowed.
- **Deviations from DESIGN.md (already updated there):**
  - The SDK secret cache is in the data directory, the entity cache in the cache directory.
  - The browser is opened with `xdg-open`.
  - `nix` replaces `libc` (unsafe code is denied).
  - M1: one `RwLock` over the whole inode table, loaded completely at mount (§4.3); the `device-id` file stays a file (§13.1);
    the control socket falls back to the data directory without `$XDG_RUNTIME_DIR` (§13.1).
- **Tried and dropped:** the fuser `experimental::AsyncFilesystem`, because it covers only lookup, getattr, read and readdir (D014).
- **Known gaps, by milestone:**
  - M3: no 401 retry in the adapter; tokens are saved every 60 s, so a crash right after a refresh can force a new login.
  - M3: events for dirty inodes follow the listing rules (local wins, resurrection, park); no conflict copies yet.
  - M4: a directory with many children that changes remotely at every poll is listed in full each time (D034); measure.
  - M4: a trash of our own still lists the parent once (a trashed node is never an echo).
  - M3: SDK calls other than `get_nodes` still read the SDK's entity cache (F14), which our own revisions leave stale;
    watch for stale name hashes or revisions in renames and uploads (with Q1/Q6).
  - `pdmount status` does not show the event stream's state (cursor age, refresh running).
  - M4 (with P7): the SDK uploads up to 6 blocks at once, equal to C#'s limit rather than below it (D033);
    `ProtonDriveClient::new` fixes it at 6, so lowering it to 5 needs the lower-level constructor. Keep files below 5
    when P7's file limit is used.
  - M4: every staged file holds an open descriptor, so pending files are bounded by the open-file limit (the daemon
    raises the soft limit to the hard one, 4096 in the sandbox); beyond it, creates fail with EMFILE. Open staging files
    on demand or keep an LRU of them.
  - M4: the first write to a remote file creates its staging under the staged-map lock, so other creates wait for that
    file creation.
  - M4: cold reads of many small files are API-bound (about 2 calls per file); cold `sha` of the dataset took 553 s, against
    104 s with a partly warm cache. No log rotation. fuser's `ForgetOne` is private, so `batch_forget` uses the default loop.
  - SDK (P4): without a `Size` XAttr, `claimed_size` falls back to the encrypted size (`sdk/node/revision.rs:321`). Uploads
    by the official web client carry it; all sizes in the M1 run were exact.
- **Known gaps of M2 (in DESIGN.md):** journal in RAM (rebuilt from rows at mount; parked items retried at the next mount);
  no `queue` commands, `mount -o ro`, `fsync.remote_paths`; no conflict pre-check (M3); A5 needs a second client (fake
  only); access rules are local, so fresh mode checks only uploaded data in A1–A6.
- **Unverified assumptions:** Q1 (commit-time and `OriginalHash` enforcement).
- **Review pending:** none (D035 and the SDK behaviour changes were approved on 2026-10-21).
- **Real-account load (D030):** fsx, pjdfstest and soak run only on the fake. Ask the user before any long-running test
  against the real account.

## Done

- [x] 2026-10-19 Design documents: DESIGN.md, docs/pdcli-evaluation.md, docs/sdk-patches.md, README.md (`ea4b9b9`).
- [x] 2026-10-19 SDK fork bare repo created (M0.1, D022).
- [x] 2026-10-19 SDK patches P1–P12 (M0.2), fork `pdmount` `9176b0f`.
- [x] 2026-10-19 M0.3–M0.7: workspace, backend, auth, doctor, local CI.
- [x] 2026-10-19 Handoff docs: AGENTS.md, CLAUDE.md, docs/decisions.md, TODO.md (the commit after `ea4b9b9`; `git log -1 -- AGENTS.md`).
- [x] 2026-10-19 M0 exit check passed (browser opening waived).
- [x] 2026-10-20 D011–D020 and D025–D027 accepted by the user.
- [x] 2026-10-20 M2.0: D024 keyring hang reproduced with `scripts/repro/keyring-d024.sh`.
- [x] 2026-10-20 M2.0b: D029 secret storage modes, replacing D024.
- [x] 2026-10-20 D030: stress and conformance tests only on the fake.
- [x] 2026-10-20 D031 accepted by the user: permission checks in the daemon.
- [x] 2026-10-19 M1 read-only mount; exit check passed on the real account with the dataset uploaded by the web client.
  The sha256 of all 1004 files matched (the 2 GiB file included), with exact sizes. R6 and the G6 read side passed in
  `live` and `fresh` mode, and kill -9 → `unmount --force` → mount recovered. Throughout: `hot_path_network_calls = 0`,
  `deadline_expired = 0`, `handler_panics = 0`. Timings: sha live 104 s, sha fresh 553 s, R6 fresh 290 s, G6 fresh 18 s.
