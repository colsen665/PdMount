# TODO

Last updated: 2026-10-20. M1 done. D011–D020 and D025–D027 accepted; D029 (secrets in files only with a flag) replaces D024 and is implemented (M2.0b). Next: M2.1.

## Next action

- M2 (writes, single device, access control; DESIGN.md §16): the M2 task list below, starting with M2.1.
- In parallel:
  - The user reviews the SDK behaviour changes listed in docs/sdk-patches.md, "Behaviour changes".
  - Research Q3 from git source.

## Milestones

Scope and exit criteria: DESIGN.md §16 (the only source; scenario IDs are defined in §15.3). Tick a box here only
when every exit criterion in §16 passes.

- [x] M0 Foundations (2026-10-19; browser opening at login waived, see Handoff notes)
- [x] M1 Read-only mount (2026-10-19; results under Done)
- [ ] M2 Writes, single device, access control
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

- [ ] M2.1 Persistable fake: the state is saved to a directory, and `pdmount mount --backend fake:<dir>` exists in test
  builds only (§15.1). Scenario scripts gain `BACKEND=fake` and the `remount` and `crash` verify modes (§15.3).
- [ ] M2.2 Staging (§7.1, §7.2): per-inode staging files (0700), a materialization bitmap, snapshots, and reads of dirty
  inodes (§6.5). `FUSE_ATOMIC_O_TRUNC` without fetching old content.
- [ ] M2.3 Local-first namespace operations (§8.1–§8.6): create, mkdir, rename with flags, unlink and rmdir with
  orphans, setattr (mode, mtime, size). Unsupported operations return the errors in §8.7 (scenario N1). The inode table
  needs per-inode state; today it is one `RwLock` (`crates/pdmount-core/src/vfs.rs`, `engine.rs`).
- [ ] M2.4 Journal (§8.2): desired-state reconciliation, executor, dependencies, backoff and parking. Upload trigger and
  debounce (§7.3), upload job (§7.4), atomic-save rename (§7.6), `own_revisions` (§7.5), `fsync=local` (§5.6), quota and
  deferred errors (§7.8, Appendix A).
- [ ] M2.5 Proton adapter writes: `upload_new`, `upload_revision`, `create_folder`, `rename`, `trash`, metadata revisions
  (`upload.meta_revision`). Use patches P5 (folder mtime), P6 (revision uid) and P10 (move with rename), which are already
  in the fork.
- [ ] M2.6 `.pdignore` local-only files (§7.7, D018).
- [ ] M2.7 Access control (§11): rules bound to nodes (D011), EROFS enforcement (D019), runtime changes with kernel
  invalidation (§11.4), and `pdmount access set|unset|list|effective` (§11.5).
- [ ] M2.8 Tests:
  - Journal property tests at 10k cases (§15.1).
  - Scenarios R1–R7, G1–G7, N1 and A1–A6 in `live`, `remount`, `crash` and `fresh` on the fake.
  - The same in `live`, `remount` and `fresh` on the real account, under `pdmount-test/`: one run at a time, stop at the
    first HTTP 429, modest G2/G3 datasets (D030).
- [ ] M2.9 Build pjdfstest and fsx from source as the sandbox user (outside the repo, e.g. `<tools dir>/`). Run the pjdfstest subset
  (§15.1) and record which cases need root and are skipped (root cannot use the mount without `user_allow_other`,
  D020). Run fsx for 6 h. Both over the fake backend only, never the real account (D030).

## Open questions

| ID | Question | How to resolve | Milestone |
|---|---|---|---|
| Q1 | Does the server enforce `CurrentRevisionID` (new revision) and `OriginalHash` (rename/move)? At draft creation or at commit? **Partly answered:** draft creation enforces `CurrentRevisionID`. The API schema documents a 409 ("submitted revision is no longer up to date or another draft is open") for `POST …/files/{linkID}/revisions` (`cs/client/js/src/internal/apiService/driveTypes.ts` ≈9655). Still open: commit-time enforcement and `OriginalHash`. | Test against the real API with two profiles | M3 |
| Q2 | ~~Is there an API to update a folder's XAttr (mtime) after creation?~~ **Resolved: no.** Folder XAttr can only be set at creation. Rename has no XAttr; XAttr updates exist only for photo revisions (iOS-only) and albums (`driveTypes.ts` 4950–4980, 1782–1794, 3726–3736). D013 stands. | — | M2 |
| Q3 | Does git's object finalization fall back from `link()` to `rename()` on EPERM? Verified only from memory, not against git source | Read git `object-file.c` (`finalize_object_file`); test G2/G3 | M2 |
| Q4 | ~~Where is the SDK fork hosted?~~ **Resolved:** local bare repo `<fork>` (D022) | — | M0 |
| Q5 | Crypto migration timing, and how the community SDK will follow | Watch `cs/README.md`/changelogs and the upstream repo | M5 |
| Q6 | Which server error codes signal a stale revision or name hash? **Partly answered:** draft conflicts are HTTP 409 / code 2500 with `Details {ConflictLinkID, ConflictRevisionID, ConflictDraftRevisionID, ConflictDraftClientUID}` (C# `RevisionConflict.cs`). P11 maps these to `RevisionDraftConflictError` / `NodeWithSameNameExistsError`. Still open: the codes for stale `OriginalHash`. | Real-API tests with Q1 | M3 |
| Q7 | Rate-limit thresholds for many small uploads (git, untar). **M1 data point:** prefetch at concurrency 4 got HTTP 429 within seconds; a cold rsync of 1000 small files (about 2 API calls each, unpaced) took 290–373 s without errors | Measure with real-account runs of G2/G3; tune the upload concurrency and debounce | M2/M4 |
| Q8 | **Deferred (D023); don't raise it until GitHub use resumes.** How are the fork patches submitted upstream (GitHub fork, or `git format-patch` to the maintainer)? | User decision | M5 |

## Handoff notes

<!-- Rewrite at the end of every session. Keep it short. -->

- **In progress:** none.
- **Uncommitted state:** none. Decisions, the M2.0 harness and D029 (M2.0b) are committed (`5cdd531`) and pushed.
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
  - M3: remote changes while mounted appear only at the next mount (D025).
  - M4: cold reads of many small files are API-bound (about 2 calls per file); cold `sha` of the dataset took 553 s, against
    104 s with a partly warm cache. No log rotation. fuser's `ForgetOne` is private, so `batch_forget` uses the default loop.
  - SDK (P4): without a `Size` XAttr, `claimed_size` falls back to the encrypted size (`sdk/node/revision.rs:321`). Uploads
    by the official web client carry it; all sizes in the M1 run were exact.
- **Unverified assumptions:** Q1 (commit-time and `OriginalHash` enforcement) and Q3.
- **Review pending:** the SDK behaviour changes in docs/sdk-patches.md. No decision is open.
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
- [x] 2026-10-19 M1 read-only mount; exit check passed on the real account with the dataset uploaded by the web client.
  The sha256 of all 1004 files matched (the 2 GiB file included), with exact sizes. R6 and the G6 read side passed in
  `live` and `fresh` mode, and kill -9 → `unmount --force` → mount recovered. Throughout: `hot_path_network_calls = 0`,
  `deadline_expired = 0`, `handler_panics = 0`. Timings: sha live 104 s, sha fresh 553 s, R6 fresh 290 s, G6 fresh 18 s.
