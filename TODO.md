# TODO

Last updated: 2026-11-01 (**On GitHub, D089, D090**: public MIT repo and real SDK fork, one commit per milestone. **M4 done**: M4.16 exit check passed at `afb99df`, not pushed; M4.8 done; D064 decided by the user: writeback cache stays off; M5–M7 planned, D073–D083, merged with M4.8; `kernel.writeback_cache` removed, D083). M3 done (M3.21: every §16 criterion passes; results in its entry in `docs/handoff-log.md`). Pre-M4
review done; fix groups G1–G5, G7 and G8 applied, G6 in part (the rest is optional, user's call). **M4 started:**
plan in `docs/plans/m4.md` (D056), M4.0 done, tag `m4-start`. M4.0.1 security review done (1 High, 5 Medium).
Fixes M4.0.2.1–M4.0.2.4 done (the High and every Medium). M4.1 done (latency histograms, `stats --json/--reset`,
Prometheus textfile). M4.2 done (fake network profile, E19). M4.3 done (benchmark harness, D057). M4.4 done (staging
descriptor LRU, D063). M4.7 done (fork P18, 4 files / 5 blocks, D058). M4.5 done (baseline, triage; untar criterion
≤ 15× local, D065; fsync SLO p99 < 30 ms, D066). M4.6.1–M4.6.4 done (store writes, no sidecar for new files D067,
durable relist queue D069, stable git row, Proton block buffer). M4.8 done (writeback cache off D064, option since removed D083, no
KILLPRIV_V2 D068, directory pages kept D072). M4.9 done (cache protections, ordered eviction, orphan sweep, `cache stats/gc/wipe`). M4.10 done (pins, D060). M4.11 done (queue tooling, `sync`, D061). M4.12 done (shutdown grace D062, `mount -o ro`, `fsync.remote_paths`, log rotation, status). M4.13 done (autostart). Quick CI is the default (D084). M4.14 done (user guide). M4.15 done (real-account run: no 429, Q7). Passphrase eviction fixed (D085). Next: M4.16.

## Next action

- **Publication is done (D089, D090):** the maintainer was told, then the SDK fork and PdMount were pushed.
- **Start M5** (`docs/plans/m5.md`). Before each push, run the full CI (D084).
- **SDK update, not done (user to schedule):** upstream is at 0.5.1 and its maintainer suggests updating. The patches stay on `base/0.4.0` until a separate task rebases and tests them; upstream PRs still wait for the user (D089).
- **M4 is done and on `master`.** Decisions D057, D062, D068, D070, D071, D072, D085, D087 accepted, D088 (no passphrase move); the full CI passed on `bc03e80`. Sandbox profiles `default` and `second` were cleared (D088); `default` is logged in again, `second` needs a login from the user before the Q1/Q6 probe. Then M5 (`docs/plans/m5.md`). No decision is `proposed`. Before relying on the latency numbers on a spinning disk, run `m4.sh` with `BENCH_ROOT` there (Known gaps).
- **Optional (user's call, 2026-10-27):** the rest of G6, pure refactors (report S-E4–S-E6, S-E9, rest of
  S-E10/S-E11, S-B2, S-B9, S-I1, S-L4 `Ids`, S-L6 `Ctx`/home dir): the staging-lock helpers, `bind_remote`,
  `keep_pending` returning renamed names, the write/truncate pairs, the journal outcome prologue and wrappers,
  `Backend::revisions`, `ReadData` → `Bytes`, the FUSE id/TTL struct and a command context in `main.rs`. About one
  session; they touch lock-ordered code or many call sites; check with `scripts/ci.sh` and 10 000-case proptests.
- **Optional (user's call, M4.15 step 3, not run):** `MODES="live fresh"` for the 20 R/G/N/A scenarios on the real account (about
  2 h); the write and sync paths changed since M2.8.
- Rerun the real-account matrix after changes to the write or sync path (command in Handoff notes). The trash may be emptied when less than 1 GiB is free (D036).

## Milestones

Scope and exit criteria: DESIGN.md §16 (the only source; scenario IDs are defined in §15.3). Tick a box here only
when every exit criterion in §16 passes. The task checklists of M0–M3 are in `docs/handoff-log.md`.

- [x] M0 Foundations (2026-10-19; browser opening at login waived)
- [x] M1 Read-only mount (2026-10-19; results under Done in `docs/handoff-log.md`)
- [x] M2 Writes, single device, access control (2026-10-22)
- [x] M3 Multi-device and sync robustness (2026-10-25)
- [x] M4 Performance and operations (2026-11-01)
- [ ] M5 User services, flush at session end, further roots (D074, D075, D078; plan `docs/plans/m5.md`)
- [ ] M6 Conflict notifications (D077; plan `docs/plans/m6.md`)
- [ ] M7 Upstream patches and crypto migration (upstream PRs once stable, when the user says so, D089)

## M4 tasks

Specifications: `docs/plans/m4.md` (one commit per task; several tasks per session as its "Sessions" table recommends,
D056). `[full]` needs the full `scripts/ci.sh`; the others `--quick` (D084). M4.15 is the real-account run, gated by the user. Exit criteria: DESIGN.md §16 (measured as D057 defines in M4.3).

- [x] M4.0 Plan into the repo, decisions D056 and D059, DESIGN §14 autostart, this checklist; tag `m4-start`
- [x] M4.0.1 First security review, whole codebase (Opus; reviewers on Fable); D070 threat model. 1 High, 5 Medium
- [x] M4.0.2.1 Secrets out of logs: fork P16, crate-wide SDK log filter, log 0600, no sign-in link in log/notify (S-1–S-3, D071)
- [x] M4.0.2.2 Manifest verification on block-map reads: fork P17 (S-4, S-11)
- [x] M4.0.2.3 Bounds on remote claimed sizes (S-5, S-6)
- [x] M4.0.2.4 Test-script safety: `rm -rf` targets, private `/dev/shm` roots (S-7, S-8)
- [x] M4.1 Latency histograms, gauges, `stats --json/--reset`, slow-request log, Prometheus textfile
- [x] M4.2 Fake network profile (RTT, shared bandwidth) for running daemons; E19 fake fixes
- [x] M4.3 Exit benchmark harness `scripts/bench/m4.sh`, datasets, SLO checker (D057)
- [x] M4.4 Staging descriptors on demand (D063)
- [x] M4.5 Baseline measurement, profiling and triage (Opus); D065, D066
- [x] M4.6.1 Store writes: cached statements, commit waiters (E2, E12)
- [x] M4.6.2 Staging without a sidecar for new files (D067; the parallel data and WAL flush was measured and dropped)
- [x] M4.6.3 Benchmark harness: a stable git row (D057)
- [x] M4.6.4 Proton block buffer sized once (E14)
- [x] M4.7 Fork patch P18 and P7 wiring (D058)
- [x] M4.8 Writeback-cache experiment (D064: stays off; D068; D072 directory pages)
- [x] M4.9 Disk cache: protections, ordered eviction, orphan sweep, `cache stats/gc/wipe`
- [x] M4.10 Pins (D060)
- [x] M4.11 Queue tooling and `sync [<path>]` (D061)
- [x] M4.12 Operations: shutdown grace (D062), `mount -o ro`, `fsync.remote_paths`, log rotation, status additions
- [x] M4.13 `autostart enable/disable` (D059), `doctor` line
- [x] M4.14 User guide and reference consistency
- [x] M4.15 Real-account run: P7 and Q7 (user-gated, D030)
- [x] [full] M4.16 Exit check, security review of the M4 changes (Fable), documentation pass, M4 ticked (Opus)
- [x] M4.17 Fixes of High/Critical findings of that review: none found, not needed

## M5 tasks

Start after M4. Specifications: `docs/plans/m5.md`.

- [ ] M5.0 Probe of logout and shutdown on Gentoo and Debian (needs the user)
- [ ] [full] M5.1 Flush on SIGTERM (D075)
- [ ] M5.2 Warning at start when a flush did not finish
- [ ] [full] M5.3 Daemon under a supervisor: readiness, stale mount, `unmount`
- [ ] M5.4 `pdmount service`, OpenRC (D074)
- [ ] M5.5 `pdmount service`, systemd
- [ ] M5.6 Manual runs on Gentoo and Debian (user)
- [ ] [full] M5.7 Pending marker (D076)
- [ ] M5.9 Probe of the further roots, read-only on the real account (needs a share, a device and a photo from the user)
- [ ] [full] M5.10 Several roots in one daemon (D079, D080), bulk-read guard (D081)
- [ ] [full] M5.11 Computers, read-only
- [ ] [full] M5.12 Shared-with-me, read-only
- [ ] [full] M5.13 Photos, read-only, folders in stages and stored thumbnails (D081)
- [ ] [full] M5.14 Read-write for Computers and writable shares (D080; real-account run needs the user's exception to D028)
- [ ] [full] M5.8 Exit check, security review, documentation (last)

## M6 tasks

Specifications: `docs/plans/m6.md`.

- [ ] M6.1 Notifications over D-Bus, replacing `notify-send`
- [ ] M6.2 Conflict notifications with name, kind and actions (D077)
- [ ] [full] M6.3 Conflicts missed while no desktop ran
- [ ] [full] M6.4 Exit check, security review, documentation

## Open questions

| ID | Question | How to resolve | Milestone |
|---|---|---|---|
| Q1 | ~~Does the server enforce `CurrentRevisionID` and `OriginalHash`? At draft creation or at commit?~~ **Resolved (M3.1, real API):** `CurrentRevisionID` only at draft creation; revision drafts are exclusive per file across clients (D043). `OriginalHash` is enforced on rename and move (P14). Details: `docs/plans/m3.md`, "M3.1 outcomes"; DESIGN.md §10.4. | — | M3 |
| Q2 | ~~Is there an API to update a folder's XAttr (mtime) after creation?~~ **Resolved: no.** Folder XAttr can only be set at creation. Rename has no XAttr; XAttr updates exist only for photo revisions (iOS-only) and albums (`driveTypes.ts` 4950–4980, 1782–1794, 3726–3736). D013 stands. | — | M2 |
| Q3 | ~~Does git's object finalization fall back from `link()` to `rename()` on EPERM?~~ **Answered by test:** G2, G3, G4, G6 and G7 pass on the fake mount, where `link()` returns EPERM (git 2.55). | — | M2 |
| Q4 | ~~Where is the SDK fork hosted?~~ **Resolved:** GitHub fork `colsen665/proton-sdk-rs2` (D089) | — | M0 |
| Q5 | Crypto migration timing, and how the community SDK will follow | Watch `cs/README.md`/changelogs and the upstream repo | M7 |
| Q6 | ~~Which server error codes signal a stale revision or name hash?~~ **Resolved (M3.1):** draft conflicts code 2500 (HTTP 409 for revisions, 422 for names, with `Details`); stale `OriginalHash` code 2000 "…out of date, rename/move failed."; move into a descendant code 2000 "Cannot move a folder into a sub-folder"; a removed draft at commit code 2511. Mapping: M3.12 in the plan's "M3.1 outcomes". | — | M3 |
| Q7 | Rate-limit thresholds for many small uploads (git, untar). **M1 data point:** prefetch at concurrency 4 got HTTP 429 within seconds; a cold rsync of 1000 small files (about 2 API calls each, unpaced) took 290–373 s without errors **M4.15 (4 uploads at once, P7): no 429 at this load.** 5000 small files drained at a steady 3.0 uploads/s, 3.1 calls/s; the rate follows from 4 uploads × about 1.2 s per call, so it is not a measured server limit. No default changes | — | M2/M4 |
| Q8 | ~~How are the fork patches submitted upstream?~~ **Route resolved:** PRs from the GitHub fork, once stable, when the user says so (D089) | User decision on timing | M7 |
| Q9 | ~~Should a daemon crash keep acknowledged changes that were not fsync'd (G5 `crash`)?~~ **Resolved (D032):** option (b), replies wait for the store commit (no fsync); measured within the §5.7 SLOs. | — | M2 |

## Handoff notes

<!-- Rewrite at the end of every session: current state and standing notes only, kept short. The session's full
     entry goes at the top of docs/handoff-log.md. -->

- **M4 exit result (2026-11-01, `afb99df`, on `worktree-M4`, not pushed):**
  - Passed on this code: `scripts/ci.sh`; `run-all.sh` on the fake in `live remount crash fresh`; pjdfstest; 10 000-case proptests. git 1.50, untar 11.36, read 1048 MB/s, gc 0.90 × cap. slo 13 of 13: `setattr` p99 0.75 ms (limit 2), `fsync` 32.7 ms (limit 40). Report: `target/bench/m4-2026-11-01/report.md`.
  - **The slo row often cannot get a verdict:** it needs 5 quiet rounds of 15. On 2026-11-01 7 of 8 attempts with the temporary build, the full run, and the first of two reruns at `afb99df` stopped at 3 or 4 quiet rounds. That gives no verdict on the limits; a rerun at a quiet moment can pass. Where the noise comes from is not known: it comes in bursts, sometimes after about 8 quiet probes in a run, so the benchmark's own deletes are a suspect, and btrfs has `discard=async`; discards were seen in only 3 % of 2 s samples, so this is unproven.
  - D087 closed the setattr miss: a WAL fsync and a staging fsync take turns. It costs `fsync` tail margin (Known gaps). The 10 000-case proptests also found an old bug, a zero-length write past the end set the size (M4.16.7); the kernel does not send such writes.
  - Security review: no High or Critical. No race test exists for C-3 and C-4 (fixed by reading the code). Remaining findings: Known gaps.
- **Current state (2026-11-01):** M4.9–M4.12 are on `master`, pushed (D060 and D061 accepted by the user; D062 proposed). M4.13–M4.16 are committed on `worktree-M4`, full run passed, **not pushed**. Per-session details: `docs/handoff-log.md`.
- **CLI bug, unfixed:** a command piped into a reader that closes early (`pdmount cache stats | head -2`) panics with a broken pipe after printing. Harmless to the mount. Seen in M4.14.
- **M5–M7 planned (2026-10-30, on `master`):** D073–D083, `docs/plans/m5.md`, `m6.md`. Appendix B gets the new keys in M5.1
  and M6.2.
- **The `gc` row runs again (M4.9):** it passes (0.90 × cap, 408 MiB after gc). It was last run from a cache already below the cap,
  so the case "cap lowered with a full cache" is covered there only by a full `all` run and the unit test.
- **SLOs after M4.10:** the slo row fails on most runs, on the base as well. Six alternating rounds, `342ae90` against
  M4.10: checks 13, 13, 12, 11, 11, 10 against 13, 12, 12, 11, 11, 9 of 13; setattr p99 1.26–3.64 against 0.90–3.04 ms (limit 2);
  rename 0.84–2.59 against 0.84–3.26 ms (limit 2); fsync 26.4–29.1 plus one 78 against 26.9–31.5 plus one 82 ms (limit 30; both
  spikes in round 6, so the machine). fsync's median is 2.4 ms higher (28.7 against 31.1), which six rounds cannot separate from
  noise; the diff does not touch that path. The M4 exit run needs an answer to this: the limits are not met reliably on this SSD.
- **Frozen test binaries must be called `pdmount`:** `is_pdmount_process` compares argv[0]'s basename, so with any other
  name `unmount --wait` and `reset` do not wait for the daemon, and scenarios fail in misleading ways (M4.8's first two
  matrices). Use `<tools dir>/bin/<dir>/pdmount`. `tests/fsx.sh` does not hit this: it never resets, so the `pdmount-fsx`
  name below works there.
- **Fake network (M4.2, DESIGN §15.1):** `pdmount fake profile --rtt 80 --up 20000000 --down 20000000 <dir>` gives
  the daemons on a fake drive round trips and shared bandwidth from their next call; `fake profile clear <dir>`.
  `fake seed`/`create` run in their own process and pay the profile too, so seed before setting it.
- **Benchmarks (M4.3, D057; limits amended by D065/D066):** `scripts/bench/m4.sh [git|untar|read|slo|gc|all]`;
  rows run in subshells, so a stopped row is reported and the others still run. After M4.8: git 1.48 (D072; 3.10–3.47
  before), untar 11.09×, read 1078 MB/s, slo 10–13 of 13. The untar row is noisy on this
  SSD (one base run 16.5×); compare binaries by alternating runs. Swap a binary under a running test with
  `cp --remove-destination` ("Text file busy" otherwise). M4.5 baseline (3 runs, at `7197651`):
  git 1.89 / 2.68 / 0.42 (noise on both sides, M4.6.3); untar 14.0–15.1× (limit now 15×); read 1043–1094 MB/s;
  slo 14 of 15, fsync p99 34–48 ms (limit now 30 ms), setattr p99 1.70–1.96 ms (limit 2 ms, close); gc SKIP
  (2026-10-28: 2.0 × cap). A full `all` run writes about 4–6 GB to the SSD, 1.5 GB of it in the gc row: skip
  gc until M4.9 (user, 2026-10-29). The git.git mirror stays in `~/.cache/pdmount-bench/git.git` (169 MiB).
- **Profiling (M4.5):** `perf_event_open` fails with EACCES for the sandbox user (even software events) and ptrace
  is blocked, so no perf or `eu-stack`. What worked: the daemon's histograms, A/B builds from a `git archive` copy
  with experiment patches switched by environment variables, and a driver that times untar into a fresh profile
  (method in "M4.5 outcomes"). Don't `SIGSTOP` a background task's shell: the harness ends the task and signals its
  process group, and `m4.sh`'s EXIT trap unmounts.
- **Upload limits (M4.7, D058):** every drive client gets `session::TransferLimits` (files = `upload.concurrency`,
  1–4; blocks 5). The client's download queue gets them too: `download_verified` takes one of its 5 block permits
  (6 before), the same as `DOWNLOADS`. Q7 (real-account rate with 4 uploads) is M4.15.
- **Deviations from docs/plans/m4.md (M4.12):** the shutdown test on a slow upload is `tests/scenarios/ops.sh` (`scripts/ci.sh`), with engine tests using pauses. Added: `read-only` on `status`'s mount line, `user.pdmount.access` answers `ro` on a read-only mount (`access effective` still shows the rules alone), and `unmount` waits for the daemon as long as the grace needs. `EventState` is one field, `Engine::last_poll`.
- **Deviations from docs/plans/m4.md (M4.11):** `drop` does not use `resolve_keep_remote`, which uploads the local copy and trashes
  the node; it follows the plan's description ("remove the staging and the pending namespace change, then relist"), D061. A local
  change does not resume a user park. `entries_under` filters a copy of the entries instead of working under the journal lock.
  The CLI smoke test is `tests/scenarios/queue.sh` (`scripts/ci.sh`). `sync` of a path whose file was deleted answers "not found";
  `sync` of its folder covers the delete.
- **Deviations from docs/plans/m4.md (M4.10):** no `Node.pinned` and no per-trigger block changes: one task reconciles from
  the tree (D060, "Reconciling"), which needs only wakes at the triggers. Added: a new turn every 16 blocks, a replacing file
  takes the pin over, `waiting-space` in `pin list`. Pin tests are in `engine/tests.rs` (`test_free_bytes` stands in for
  statvfs there); the statvfs path ran only in a manual smoke test on a fake mount.
- **Deviations from docs/plans/m4.md (M4.9):** the protection follows the staging's `base`, not `set_base_rev`: that call
  only changes the upload's precondition, and unmaterialized bytes still come from `base` (the plan's "rebased" would have
  let those blocks be evicted). It is a guard the `Staged` holds, counted per revision in `staging::Protections`, not a set
  in `DiskCache` that the engine updates, so no way out of the staged map can leak an entry. The sweep reads its rows from the
  store with no lock and takes the index mutex only to drop one directory's dangling rows. `entries` is a `BTreeMap`, so a
  revision's or file's blocks are a range (`wipe <path>`, M4.10).
- **Deviations from docs/plans/m4.md (M4.8):** D072 was not planned (the plan's M4.6 input pointed at kernel-side
  directory caching); it is what makes the git row pass. Linux 6.18 does not turn `O_WRONLY` into `O_RDWR`: reads arrive on
  write-only handles, and the engine checks access at open, so nothing changed for it. KILLPRIV_V2 dropped (D068). The
  fsx and writeback matrix ran on the binary before the last `opendir` fix (forget the hash next to another open
  handle); the default matrix, CI and benchmarks ran on the final code.
- **Deviations from docs/plans/m4.md (M4.6):** M4.6.2 keeps the sequential fsync: the parallel flush it specified
  broke the 2 ms namespace SLO (p99 4.7–7.0 ms, D067). M4.6.2 also carries D069, a fix the 10 000-case proptests
  required (the fault fails on `25f8118` too). M4.6.3's git row gained `sync -f` after the local clone; without it
  the spread was 0.41–3.82. "Skipping unchanged rows" and "UPDATE in place" (after M4.6.1) were not specified as a
  task: untar stayed below 12× in alternating runs (10.9–11.6×), though the daemon writes about 310 MiB per untar
  of a 44.5 MB tar, mostly WAL pages of many small commits (a Known gap). M4.6.4: the SDK still decrypts into its
  own buffer; only our `Vec` growth and the `copy_block` copy are gone.
- **Deviations from docs/plans/m4.md (M4.5):** no perf profile (above); the gc row ran once, not three times; run 1's
  rows come from two invocations; the getxattr round trips went to M4.8 (step 3a) instead of an M4.6 task.
- **Deviations from docs/plans/m4.md (M4.4):** the LRU's capacity is 1024 or a quarter of the soft open-file limit,
  whichever is lower (at least 16), since a fixed 1024 runs out under the test's `prlimit --nofile=512`; the
  `FdCache` lives in `StagingDir` (clones share it), `Staged` keeps the path; local EMFILE/ENFILE of a backend is
  `Transient` (`BackendError::local_io`), the Known-gaps item M4.4 also asked for. M4.7: the sessions' builder
  test covers `TransferLimits::of`; the builders themselves are covered by the fork's test (no getters, no client
  without a session).
- **Deviation from docs/plans/m4.md (M4.3):** `ops.py` (latency.sh's workload) gained an fsync every tenth
  iteration, so the slo row has fsync samples; `slo.py` fails a key without samples, except `readdir`/`readdirplus`
  (the kernel picks one) and `read_ram`; the gc row samples even without `cache gc` and reports SKIP.
- **Deviation from docs/plans/m4.md (M4.2):** `fetch_block_into` reads its blob with `block_in_place` (on a
  multi-thread runtime), not `spawn_blocking`, because it fills a borrowed buffer; `fetch_block` uses
  `spawn_blocking`. Both read after the state lock is released.
- **Instrumentation (M4.1, DESIGN §5.8):** `pdmount stats --json` gives per-op p50/p90/p99/max in µs, measured in the
  daemon from callback entry to reply (commit waits included); `--reset` clears the histograms between benchmark
  phases. Backend calls are timed as `backend_<method>`. `rate_limited` counts HTTP 429 answers (M4.15).
- **Deviations from docs/plans/m4.md (M4.1):** the slow-request WARN is for requests that stayed on the hot path
  (DESIGN §5.8 "on the hot path"), so slow-path lookups, setattrs and writes waiting on the network are not logged,
  not only `read_slow`; `access` and `releasedir` are timed too; `staged_open_files` is the descriptor LRU's length
  since M4.4; `queue_oldest_secs` counts entries that are not parked; Prometheus series carry a `profile` label.
- **Tools:** pjdfstest (`<tools dir>/pjdfstest`, upstream `85a8aea`, `autoreconf -ifs && ./configure && make pjdfstest`)
  and fsx (`<tools dir>/fsx/fsx`: xfstests' `ltp/fsx.c` compiled alone with `<tools dir>/fsx/config.h`, a hand-written config
  with `HAVE_*` defines plus `assert.h`, `getopt.h`, `sys/param.h`). Long fsx run: `SCENARIO_ROOT=/dev/shm/pdmount-fsx
  PROFILE=fake-fsx PDMOUNT=<tools dir>/bin/pdmount-fsx tests/fsx.sh` (a copy of the debug binary, so rebuilds do not touch
  it); it leaves the mount up, so unmount it (with the run's `XDG_*_HOME`) before deleting its `SCENARIO_ROOT`, or
  `rm -rf` deletes through the mount. Watch `state.db-wal` and `df /dev/shm` during long runs.
- **SDK fork:** its files mix CRLF and LF; edit them only with tools that keep line endings (rustfmt and Python
  rewrites have turned files into whole-file diffs).

- **Real-account load:** about 3 uploads/s of small files with 4 at once (M4.15; about 0.5 before P7); echo checks of created nodes cost one `get_details` per 50 nodes (P13;
  before, one per node: 437 calls during calibration). The 60-run matrix met no 429.
- **Pitfalls:** no cargo commands in a worktree while `scripts/ci.sh` runs there (a rebuild replaces the binary
  the scenarios use). `grep 429` over the daemon log matches revision ids and sizes; grep
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
- **GitHub (D089, D090):** `origin` is `colsen665/PdMount`, public. No hosted CI; upstream PRs wait for the user. Local checkouts and bare repos from before the milestone rewrite hold the old history: never merge or push from them. Local locations: AGENTS.md, "Local paths".
- **Deviations from DESIGN.md (already updated there):**
  - The SDK secret cache is in the data directory, the entity cache in the cache directory.
  - The browser is opened with `xdg-open`.
  - `nix` replaces `libc` (unsafe code is denied).
  - M1: one `RwLock` over the whole inode table, loaded completely at mount (§4.3); the `device-id` file stays a file (§13.1);
    the control socket falls back to the data directory without `$XDG_RUNTIME_DIR` (§13.1).
- **Tried and dropped:** the fuser `experimental::AsyncFilesystem`, because it covers only lookup, getattr, read and readdir (D014).
- **Unverified assumptions:** none open (Q1/Q6 resolved in M3.1; two devices of one account ran in M3.20).
- **Review pending:** none (M3 decisions D040–D042, D045–D047 and D052 accepted 2026-10-26; D067 and D069
  2026-10-29).

## Known gaps

By milestone.

- **Security (M4.16; R-n in `docs/security/2026-10-31-review/verification.md`).** Medium: R-1 Prometheus temp file follows a symlink planted in a shared-writable directory (`stats.rs`); R-9 the pin reserve is checked per file, so one large remote file can fill the disk with pinned blocks (`pins.rs`). Low: R-2 autostart for a profile never logged in prints the sign-in link to the session log; R-3 passphrases of a refused login stay in `sdk-keys.db`; R-12 unbounded `entry_path` recursion (plausible); R-14 `needs_login` by message match; R-15 unverified block listing refetched per read. Info: R-4–R-6, R-8, R-16. Fixed in M4.16.1–M4.16.5: R-7, R-10, R-11, R-13.
- **Code review, M4.16.** C-1 to C-5 fixed in M4.16.1–M4.16.5. As reported, not checked: C-6 pin nudges can starve fetching (`pins.rs:509`); C-7 `fetch_pinned` never writes a RAM-only block to disk (`pins.rs:560`); C-8 a failed relist is not retried and leaks its count (`journal.rs:834`); C-9 eviction scan on every `put` when protected blocks fill the cap (`cache.rs:895`); C-10 `pin_summary` costs a store read per pinned file on each `status` (`pins.rs:390`).
- **M4 numbers are for an SSD (2026-10-31).** Nothing is measured on a spinning disk. `BENCH_ROOT` chooses where the benchmarks run.
- **D087 costs fsync margin (2026-11-01).** Alternating rounds: `fsync` p99 32.1 → 37.5 ms against a 40 ms limit; rounds above 60 ms 4 of 11 against 1 of 9, too few to tell from noise. The `TRUNCATE` checkpoint on the writer still stalls replies about 40 ms when the WAL passes 64 MiB. Why btrfs stalls a commit while a WAL fsync overlaps another file's fsync is not established, and the gate is measured on this SSD only.
- M3 (accepted risks, DESIGN.md §17): a version restore inside our draft window is overwritten (D043); a revision
  written between the local-delete pre-check and the trash goes to the trash; a crash before the group commit loses a
  conflict record (not the copy); edit/edit above 16 MiB without a claimed SHA-1 always makes a copy (D040).
- M3: a slow pass on the receiving device (paced listings, D027) can exceed the 40 s visibility SLO (D050); not seen in
  M3.20.
- M3 (found by the M3.21 review, pre-dates M3): a node whose metadata fails to decrypt (SDK `Degraded`) is skipped by
  `get_nodes` and `list_children`, so it reads as absent: a clean local copy is removed, a pending one may be uploaded
  again (a duplicate), and a local trash of it counts as done without trashing. No data loss. Needs a third state in
  the `Backend` answers.
- M4: `timeouts.shutdown_grace` is parsed but not used: SIGTERM does not wait for in-flight uploads as §14.4 says
  (pending work persists and resumes at the next mount). Implement it or drop it from the spec.
- M4: every event page with a delete or trash scans all inodes (`Tree::pending_under_remote`) under the tree read
  lock; an index by remote parent would bound it.
- M4: a directory with many children that changes remotely at every poll is listed in full each time (D034); measure.
- M4: a trash of our own still lists the parent once (a trashed node is never an echo).
- M4 option, only if flagged (pre-M4 review S-A2, deferred by the user 2026-10-27): keep the remote node a name clash
  hides as a RAM-only shadow and show it directly when the name is free, instead of the relists (`Node::clashed`,
  `take_relist`/`list_later`, the listing in `yield_to_remote`, the throwaway listing in `settle_name_clash`).
  Saves one listing per freed clash name. Against it: a shadow is a second copy of remote state that can be stale
  (D040's single meeting point), the restart path (`clashed` for every created node, D055) stays, and it changes how
  accepted D046 and D055 are carried out (new decision needed). Cheaper alternative if Q7 shows the cost: after
  `settle_name_clash`, insert the node from its fresh `get_nodes` answer instead of listing the folder again. Never
  the variant where a shadowed folder keeps its clean subtree (reverses D052).
- M4 (left out by M4.5, numbers in `docs/plans/m4.md` "M4.5 outcomes"; pre-M4 review §2): E1 (listings mark all
  children changed), E3 (lookup takes the tree write lock; lookup p99 13–15 µs), E4/E5 (journal wake and scans; not
  visible in FUSE latencies), E6 (staging under the staged-map lock; create p99 1.2–1.5 ms), E7 (folder creates
  inline: 80 ms each, serial, delays the drain), E8, E9, E10, E11, E13, E15, E16 (read row at 1043–1094 MB/s), E17
  (getxattr p99 3 µs; round trips to M4.8), E18, S-A2. untar floor without the commit wait: 4.5× on the SSD (D065).
- M4 (M4.6.1): the daemon writes about 310 MiB per untar of a 44.5 MB tar (`/proc/<pid>/io` write_bytes), mostly WAL
  pages of many small commits. Skipping unchanged rows and `UPDATE` in place would cut it; not specified, since untar
  stays below 12×.
- M4 (M4.6.3): the git row fails at 3.10–3.47× (limit 2.0); about 1000 FUSE round trips per `git status` on the mount
  (M4.8 input).
- SDK calls other than `get_nodes`, renames and moves (fresh, F15) read the SDK's entity cache (F14), which our own
  revisions leave stale; uploads take the base revision from PdMount, not from that cache.
- M4: the first write to a remote file creates its staging under the staged-map lock, so other creates wait for that
  file creation.
- M4: cold reads of many small files are API-bound (about 2 calls per file); cold `sha` of the dataset took 553 s, against
  104 s with a partly warm cache. No log rotation. fuser's `ForgetOne` is private, so `batch_forget` uses the default loop.
- SDK (P4): without a `Size` XAttr, `claimed_size` falls back to the encrypted size (`sdk/node/revision.rs:321`). Uploads
  by the official web client carry it; all sizes in the M1 run were exact.
- **Security (M4.0.1, verified Lows and Infos; details and file:line in
  `docs/security/2026-10-27-review/` (`verification.md` maps S-n to the area findings)):**
  - Low S-9: a listing applied while a local move into a folder that vanished remotely has just completed can hit
    `expect("counted above")` (`vfs.rs` `apply_listing_at`); the folder looks empty in RAM until relisted, nothing
    persisted. The 4a4eb28 fix covers followers, not stamped nodes. Needs a proptest case.
  - Low S-10: `Retry-After` has no upper bound (fork `error.rs:191`, `http/retry.rs:115-122`; core `events.rs:96-105`
    sleeps it; the journal clamps at 300 s).
  - Low S-12: remote modes keep setuid/setgid/sticky bits (`proton.rs` `parse_mode`, `vfs.rs:73,99`); the mount is
    `nosuid`, copies to local disk keep them.
  - Low S-13: the data directory, `state.db`, `device-id` and `proton-root.json` are created with umask modes before
    `create_private_dir` restricts the directory (fake profiles never restrict it).
  - Low S-14: `unmount --force` acts on any mount at the path, not only a pdmount one (`daemon.rs:950-981`).
  - Low S-15: the pid check compares only argv[0]'s basename, so a reused pid of another profile's daemon matches.
  - Low S-16: config durations near `u64::MAX` panic the event and journal tasks (`events.rs:98`, `journal.rs:713`);
    `Config::validate` has lower bounds only.
  - Low/Info S-17: remote names reach logs (`vfs.rs:1370`, `journal.rs:963`, parked reasons) and CLI output
    unescaped (newlines, terminal escapes).
  - Low S-18: a 1 MiB remote `/.pdignore` with many patterns is matched under the tree read lock on every create.
  - Low S-25: a 5xx retry of an applied rename/move resends a stale `OriginalHash`, which looks like a concurrent rename.
  - Low (M4.0.2.2): a block map that fails (a revision failing the manifest check, a failed key lookup) is not
    cached, so every read of that revision lists its blocks again (plus a `keys/all` call for an address the key
    cache has no entry for). The first M4.0.2.2 real-account run made about 2 000 such reads in 52 min without a 429.
    A short negative cache in `ProtonBackend::block_map` would bound it (D027 pacing).
  - Low (left by P16): the fork's generic decode helpers (`sdk/api/mod.rs:249,284,298,307`, `storage.rs:139`,
    `docs.rs:238`, shared-by-me/with-me and album decoders) put the whole response body into the error of a
    malformed reply, which PdMount logs at WARN. Seen once in M2.8 (an event page without `EventID`). Truncate or
    drop the body there.
  - Info: S-19 (remote `/.pdignore` silently makes new files local-only; log it at info, show the rule count in
    `status`), S-20 (mode bits are advisory under kernel caching; `O_RDONLY|O_TRUNC`), S-21 (only the last directory
    component is chmod'ed; unbounded control `read_line`; `.tmp` reuse in `write_private_file`), S-22
    (`BlockKey::encode` not injective with NUL ids), S-23 (the fake backend module is compiled into every build;
    gated at run time by `cfg!`), S-24 (an empty name hash becomes an empty `OriginalHash`), S-26 (`$dir` inside
    `sh -c` strings in a1–a3).
- M2 (in DESIGN.md): journal in RAM (rebuilt from rows at mount; parked items retried at the next mount);
  `mount -o ro`, `fsync.remote_paths` (M4); A5 needs a second client (fake only); access rules are local, so fresh mode checks only uploaded data in A1–A6.
