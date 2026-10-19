# TODO

Last updated: 2026-10-19. M0 done (exit check passed, browser opening waived by the user). Next: M1.

## Next action

- M1 (read-only mount, DESIGN.md §16), starting with the `pdmount-fuse` crate.
- In parallel:
  - The user reviews proposals D011–D020 in docs/decisions.md.
  - The user reviews the SDK behaviour changes listed in docs/sdk-patches.md, "Behaviour changes".
  - Research Q3 from git source.

## Milestones

Scope and exit criteria: DESIGN.md §16 (the only source; scenario IDs are defined in §15.3). Tick a box here only
when every exit criterion in §16 passes.

- [x] M0 Foundations (2026-10-19; browser opening at login waived, see Handoff notes)
- [ ] M1 Read-only mount
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
  fallback and a 20 s keyring timeout (D024). The SDK caches are encrypted.
  Verified live by the user: sign-in through the printed link for profiles `default` and `second`.
- [x] M0.6 `doctor`: checks /dev/fuse, fusermount3, fuse.conf, config, keyring, login, disk space, clock and version. On this
  machine (sandbox user): no FAIL; warnings for fuse.conf (D020) and the keyring (no session bus, file fallback D024).
- [x] M0.7 `scripts/ci.sh`: fmt, clippy `-D warnings`, 50 tests, release build. Passes. Fake-backend mount tests are added in M1.

## Open questions

| ID | Question | How to resolve | Milestone |
|---|---|---|---|
| Q1 | Does the server enforce `CurrentRevisionID` (new revision) and `OriginalHash` (rename/move)? At draft creation or at commit? **Partly answered:** draft creation enforces `CurrentRevisionID`. The API schema documents a 409 ("submitted revision is no longer up to date or another draft is open") for `POST …/files/{linkID}/revisions` (`cs/client/js/src/internal/apiService/driveTypes.ts` ≈9655). Still open: commit-time enforcement and `OriginalHash`. | Test against the real API with two profiles | M3 |
| Q2 | ~~Is there an API to update a folder's XAttr (mtime) after creation?~~ **Resolved: no.** Folder XAttr can only be set at creation. Rename has no XAttr; XAttr updates exist only for photo revisions (iOS-only) and albums (`driveTypes.ts` 4950–4980, 1782–1794, 3726–3736). D013 stands. | — | M2 |
| Q3 | Does git's object finalization fall back from `link()` to `rename()` on EPERM? Verified only from memory, not against git source | Read git `object-file.c` (`finalize_object_file`); test G2/G3 | M2 |
| Q4 | ~~Where is the SDK fork hosted?~~ **Resolved:** local bare repo `<fork>` (D022) | — | M0 |
| Q5 | Crypto migration timing, and how the community SDK will follow | Watch `cs/README.md`/changelogs and the upstream repo | M5 |
| Q6 | Which server error codes signal a stale revision or name hash? **Partly answered:** draft conflicts are HTTP 409 / code 2500 with `Details {ConflictLinkID, ConflictRevisionID, ConflictDraftRevisionID, ConflictDraftClientUID}` (C# `RevisionConflict.cs`). P11 maps these to `RevisionDraftConflictError` / `NodeWithSameNameExistsError`. Still open: the codes for stale `OriginalHash`. | Real-API tests with Q1 | M3 |
| Q7 | Rate-limit thresholds for many small uploads (git, untar) | Measure with real-account runs of G2/G3; tune the upload concurrency and debounce | M2/M4 |
| Q8 | **Deferred (D023); don't raise it until GitHub use resumes.** How are the fork patches submitted upstream (GitHub fork, or `git format-patch` to the maintainer)? | User decision | M5 |

## Handoff notes

<!-- Rewrite at the end of every session. Keep it short. -->

- **In progress:** none.
- **Uncommitted state:** none. Everything is committed and pushed to `origin/master`.
- **`cs/` sources:** `<official SDK checkout>/` is a clone at tag `js/v0.21.0` (`e6a661f5`, cs 0.24), chosen because every existing
  `cs/` line citation matches there.
- **GitHub postponed (D023):** no PRs, forks, issues or hosted CI. A read-only `git fetch upstream` is allowed.
- **Deviations from DESIGN.md (already updated there):**
  - The SDK secret cache is in the data directory, the entity cache in the cache directory.
  - The browser is opened with `xdg-open`.
  - `nix` replaces `libc` (unsafe code is denied).
- **Tried and dropped:** the fuser `experimental::AsyncFilesystem`, because it covers only lookup, getattr, read and readdir (D014).
- **Unverified assumptions:**
  - Q1 (commit-time and `OriginalHash` enforcement) and Q3.
  - The SDK patches and the Proton adapter: only login, account/quota and key unlock have run live so far; the read path is
    first exercised in M1.
- **Review pending:** decisions D011–D020 are proposals awaiting the user's review.

## Done

- [x] 2026-10-19 Design documents: DESIGN.md, docs/pdcli-evaluation.md, docs/sdk-patches.md, README.md (`ea4b9b9`).
- [x] 2026-10-19 SDK fork bare repo created (M0.1, D022).
- [x] 2026-10-19 SDK patches P1–P12 (M0.2), fork `pdmount` `9176b0f`.
- [x] 2026-10-19 M0.3–M0.7: workspace, backend, auth, doctor, local CI.
- [x] 2026-10-19 Handoff docs: AGENTS.md, CLAUDE.md, docs/decisions.md, TODO.md (the commit after `ea4b9b9`; `git log -1 -- AGENTS.md`).
- [x] 2026-10-19 M0 exit check passed (browser opening waived).
