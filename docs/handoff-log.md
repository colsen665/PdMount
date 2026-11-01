# Handoff log

Session-by-session handoff history, newest first, moved out of `TODO.md` on 2026-10-28 (user request) so that a
session start reads only the current state. Not read at session start: open it when `TODO.md` or a plan points to an
entry. At the end of a session, add its entry at the top of "Session entries" (AGENTS.md, "End of every session").

## Session entries

- **Decision review (2026-11-01, Opus; on `master`):** M4 pushed to `master` (`417a036`) on the user's request. The user accepted D057, D062, D068, D070, D071, D072, D085 and D087. The user amended D085 (D088): `move_passphrases` and its tests and the `rusqlite` dependency of `crates/pdmount` are gone. Before clearing, `default` had its passphrase in `sdk-keys.db` and `second` had one in `sdk-secrets.db`; both had empty journals and staging tables, and `second` held 6 conflict records and one orphan staging file (test data). `logout` on both got HTTP 401 from `/auth/v4`, so neither session was ended on the server (cause not looked into); both directories under `~/.local/share/pdmount` and `~/.cache/pdmount` were deleted, logs kept. The user wiped their own mount's state too and logged `default` in again: its passphrase is in `sdk-keys.db`, none in `sdk-secrets.db`. `scripts/ci.sh` (full) passed on `1f96727`.

- **M4 exit (2026-11-01, Opus; M4 ticked, not pushed):** the user chose to find the cause of the setattr miss rather than raise the limit. Temporary timing logs, not committed: commits of 2 ms or more ran inside a WAL `fdatasync` (272 of 275); a stand-alone SQLite test shows the stall only when that overlaps a `fdatasync` of another file; replies over 30 ms were the `TRUNCATE` checkpoint. D087: the two kinds of fsync take turns (`9a4cd1b`). A false lead: 4120-byte appends do not stall, and an early SQLite test was void because identical `UPDATE`s write no WAL frames.
  - Full run on this code: `scripts/ci.sh`, matrix, pjdfstest, 10 000-case proptests, git 1.50, untar 11.36, read 1048 MB/s, gc 0.90 × cap passed; slo stopped at 4 quiet rounds of 15. The attempt before it did not count: CI stopped at `cargo fmt` on my new test, and its proptests found the zero-length write bug (M4.16.7, reproduced by a regression case first); its slo row passed 13 of 13.
  - slo at `9a4cd1b`: attempt 1 stopped at 3 quiet rounds; attempt 2 passed 13 of 13 (4 rounds not counted), `setattr` p99 0.75 ms, `fsync` 32.7 ms. Attempts were capped at 3, retried on the quiet-round count only.
  - The toolchain line in `AGENTS.md` was stale (1.99 nightly): builds use 1.98.1 stable. The user installed rustup for the sandbox user with the `gentoo` default, for Miri in another session; the compiler stayed the same.

- **M4.16 fixes and final run (2026-10-31, Opus; M4 not ticked):** the user chose "Median, quiet disk" (D086: fsync limit 40 ms, median of 5 rounds, other limits stay) and "Five plus cheap Lows". Commits: `2374308` C-1, R-11, R-13; `263d651` C-2, R-10 (live corruption of the new staging file, reproduced by a test first: it grew from 1 to 4,194,315 bytes); `d2e1129` C-3; `6c1a3af` C-4; `92066fe` C-5, R-7; `1a8b8e3` D086. C-3 and C-4 have no race test.
  - Final run at `1a8b8e3`, 13:53–16:00: `scripts/ci.sh`, matrix on the fake (`live remount crash fresh`), pjdfstest, 10 000-case proptests passed. git 1.50, untar 11.24, read 1049 MB/s, gc 0.90 × cap pass. **slo fails on setattr:** median p99 3.00 ms, limit 2; rounds 3.28, 3.00, 3.75, 1.51, 2.32. fsync median 30.7 ms passes. 5 of 5 rounds quiet.
  - Diagnosis (scratch copies of `ops.py`, not committed; the numbers are in `TODO.md`): the tail belongs to the first committing operation after an fsync with other clients' fsyncs running, not to setattr. What it waits on is not established. A simple append-during-`fdatasync` test on btrfs does not reproduce it.

- **Passphrase eviction fix (2026-10-31, Opus):** the M4.15 blocker, D085. `secret_cache.rs` keeps `account:passphrase:*` in `sdk-keys.db` (no cap) and moves existing ones when the caches open; `map_error` turns locked keys into `Unauthorized`; `status` reports it and `mount` comes up waiting for the sign-in (`known_root_for_login`). No fork change. Checked with `status` on profile `default`, read-only: "needs login, run `pdmount login`" instead of the error; the passphrase there was already gone, so the user signed in again (`<test account>`). After it the passphrase is in `sdk-keys.db` and none in `sdk-secrets.db`. The next mount drained the 12 parked trash steps in about 30 s; `pdmount-test/untar/` is gone; unmounted afterwards. A daemon sitting in `needs_login` while the login happens was not run on the real account: the login was done with no mount up. Full `scripts/ci.sh` passed.

- **M4.15 (2026-10-31, Opus; the user agreed in the session):** real account, `74cabab`, P7 on (4 files, 5 blocks). Everything under `pdmount-test/`. No 429 in any run. Step 3 (optional matrix) not run.
  - G2 and G3 (`G3_COMMITS=200`) `live`: PASS, 82 s and 77 s to acknowledge, then about 10.5 min to drain. 1113 uploads, 3298 backend calls, `rate_limited` 0, nothing parked. `backend_upload_new` p50 1.96 s, p99 4.02 s.
  - Untar of the M4.3 tar (5000 files, 100 folders) into `pdmount-test/untar/`: acknowledged in 1.14 s, drained in 1675 s (27.9 min). 5000 uploads, 5250 backend calls, `rate_limited` 0, 0 429 lines in the log, `deadline_expired` 0, `hot_path_network_calls` 0. Steady about 3.0 uploads/s from start to end. p99: `upload_new` 3.11 s (p50 1.17 s, max 4.13 s), `create_folder` 348 ms, `get_nodes` 2.82 s, `poll_events` 398 ms, `storage_info` 634 ms.
  - Q7 answered: no 429 at this load. The rate is what 4 concurrent uploads at about 1.2 s each give, so this is no server limit; no default change proposed. The server content of the untar was not read back: the check is that all 5000 uploads were committed. 
  - **Bug found at the cleanup: the session lost its key passphrase.** `rm -rf` of the untar area parked 12 folder trash steps with "At least one active user key exists, but none could be unlocked", and `status` now fails the same way. `session.rs` `client_options` opens the secret cache with `open_file(.., Some(5_000))`, whose LRU drops the oldest 1250 entries when full. Each file adds an entry, and `account:passphrase:<key id>` is written only at login and read only when the cached user keys are gone, so it is the first to go; the database now holds 3927 entries and no passphrase. Only a new login restores it. The profile is unmounted; the 12 trash steps are pending and retry at the next mount, so `pdmount-test/untar/` is still on the server (`rate_limited` stayed 0, 5590 calls). The numbers above were taken before this and stand.

- **M4.14 (2026-10-31, Opus):** `docs/user-guide.md`; outputs in it come from a fake mount. Every `--help` checked against §14.2 and the guide. Fixes: `mount` lacked the `--browser`/`--no-browser` that §12.2 and §14.2 specify (added); missing help on `access`, `conflicts resolve`, `queue show`, `queue drop --yes`; D-numbers out of help; §11.5's example used an absolute path the CLI refuses; §14.3 listed the wrong commands as not using the socket. Quick CI only, not pushed.

- **M4.13 (2026-10-31, Opus):** `pdmount autostart [enable|disable]` (D059), a `doctor` line. The `Exec` value is quoted, then string-escaped: the Desktop Entry spec applies both, which the plan leaves out. The user made quick CI the default (D084); tasks that need the full run are marked in `TODO.md`. Quick CI only, not pushed.

- **M4.12 (2026-10-31, Opus):** operations. Journal `stop_starting`/`idle` and the grace in `Engine::shutdown` (D062), `EngineOptions::read_only` and `-o ro`, `fsync.remote_paths` compiled at open and in config validation, `rotate_log`, status lines (`Engine::last_poll`). New scenario `tests/scenarios/ops.sh`. The user accepted D061. Deviations in `TODO.md`.

- **M4.11 (2026-10-30, Opus):** queue tooling and `sync` (D061). `engine/queue.rs` has the views, `retry`, `park`, `drop`, `sync_path`;
  `Journal` gets user parks, a hold for `drop`, `sync_round`; steps recheck park and hold when they start; `blocked_on` is cleared
  when a plan moves on. New scenario `tests/scenarios/queue.sh`, in `scripts/ci.sh`. The user accepted D060. Deviations in `TODO.md`.

- **M4.10 (2026-10-30, Opus):** pins (D060). `engine/pins.rs` reconciles the pinned revisions from the tree and fetches
  what is missing, paced; `DiskIndex::pinned_revs`; `pin`, `unpin`, `pin list`, `status` line. Smoke test on a fake mount
  passed (12 MB file, checksum equal; survives `gc` and `wipe`; unpin frees it). Deviations in `TODO.md`.
  Benchmarks: read 1073 MB/s; slo against the base in `TODO.md` ("SLOs after M4.10"): no regression shown, not met reliably on either.

- **M4.9 (2026-10-30, Opus):** disk cache maintenance. Protections as a guard in `Staged` (`staging::Protections`), ordered
  index (`by_age`), RAM hits touch the disk entry at most once per second per block, pinned bytes apart, a once-per-mount
  orphan sweep, `cache stats/gc/wipe` with their control requests. Deviations in `TODO.md`.

- **M5–M7 planning (2026-10-30, Opus, worktree `M5_plus_planning`):** the user added two milestones ahead of the upstream work
  (D073): user services with flush at session end (D074, D075), further roots (D078; layout accepted as D079), and conflict
  notifications (D077). The user asked whether a flush can leave a marker for other devices; the user accepted that as D076, with real-account tests left on the fake (no D028 exception).
  Read-write for Computers and writable shares added as M5.14 (D080); the SDK sources confirmed that Photos is different (no folder
  listing, creation, move or revision upload in the official client), so it stays read-only.
  Browsing many images got a strategy (D081): folders in stages, stored thumbnails in the local cache, a bulk-read guard.
  Plans in `docs/plans/m5.md` and `m6.md`; scenarios X1–X10 in DESIGN §15.3. No code changed. DESIGN §5.4, Appendix B and the D064/D068/D072 entries were left alone for M4.8.
  OpenRC facts come from this machine; logout and shutdown behaviour on OpenRC and systemd is unverified (M5.0).
  After M4.8 reached `master`, it was merged in and the plans checked against §5.4/§5.4.1 and D072: M5.1's `syncfs` only matters with
  the option on, Photos needs D072's rule in its own `opendir`, and the writeback cache stays off further mounts (D082).
  The user then had the option removed altogether (D083, code and test switches; D082 rejected). `scripts/ci.sh` run on that commit.

- **M4.8 (2026-10-30, Opus):** writeback-cache experiment, step 3a, and the directory page cache. Results in
  `target/m4.8/` (not committed); decisions D064 (user: stays off), D068, D072.
  - **Wiring:** `kernel.writeback_cache` requests `FUSE_WRITEBACK_CACHE` with a WARN at mount; `KERNEL_WRITEBACK=1` in
    `lib.sh` and `m4.sh`, `PDMOUNT_TEST_WRITEBACK=1` in the mounted tests (all 6 pass both ways).
  - **Suite:** default 124 of 124, writeback 120 of 124 (A6, a real behaviour change, see D064), pjdfstest passes both ways,
    fsx 1 h with it on passes (small 50.5M, large 11.8M operations).
  - **Benchmarks (`m4.sh`, off / on):** git 1.48 / not run (22-run A/B: 2.72 against 2.53); untar 11.09 / 12.52; read
    1078 / 1079 MB/s; slo 10 of 13 / 12 of 13; repeats with `target/m4.8/slo-repeat`: default 10–12, writeback 12.
  - **Step 3a:** KILLPRIV_V2 took `getxattr` from 8819 to 5000 per untar, no measurable time gained (D068).
  - **Directory pages (D072):** git row from 3.10–3.47 to 1.48. A review of `opendir` found that a hash could stay valid
    while pages came from another handle's snapshot; fixed by forgetting it.
  - **My mistakes, corrected in session:** a frozen binary named `pdmount-m48` made `unmount --wait` return early, and I
    first blamed the kernel's mtime flush and a control-socket race, wrote one of them into DESIGN and patched
    `control.rs`. Both reverted; the first two matrices are `target/m4.8/invalid-suite-*`.
  - **Open:** setattr p99 on the current tree against base (TODO "SLOs after M4.8"), unexplained. The conflict hazard of dirty
    pages is from reading the kernel, not from a test.
- **M4.6.1–M4.6.4 (2026-10-29, Opus):** performance fixes from "M4.5 outcomes", one commit each (`eda253e`,
  `b83b0e9`, `82bcf5a`, and M4.6.4). Results in `target/m4.6/` (not committed); full numbers in the commit bodies.
  - **M4.6.1** (E2, E12): `prepare_cached` in `write_rows` and the cache index writes; `Store::after` waits in a
    waiter list the writer drains after each commit (no no-op write). CI and 10 000-case proptests pass. On the SSD
    the gain is within noise (untar A/B medians 1.22–1.65 s for every variant); on tmpfs untar 0.84 → 0.54 s,
    create p50 58 → 32–38 µs. Alternating untar rows: base 11.57 / 16.50, M4.6.1 11.62 / 10.93 (two earlier M4.6.1
    runs at 20–21× were disk noise). The daemon writes about 310 MiB per untar (WAL write amplification; Known gap).
  - **M4.6.2** (D067): no sidecar for staging without a base, written before the data file when there is one;
    startup lists data files; `sync` flushes a changed sidecar first. The specified parallel data/WAL flush was built
    and measured: fsync p99 23.6–32.1 ms, but create/mkdir/unlink/rename/setattr p99 4.7–7.0 ms against the 2 ms SLO
    (WAL fsync overlapping the staging fsync on btrfs stalls the writer). Kept sequential: fsync p50 15–19 ms, p99
    28.7–31.5 ms, namespace p99 0.62–0.83 ms (slo alone, 3 alternating rounds). The first A/B used the same binary
    for all runs ("Text file busy" on `cp` while the previous daemon exited); rerun with `cp --remove-destination`.
  - **D069 (in M4.6.2):** the 10 000-case `two_devices_and_an_app_converge` failed on a case that fails on `2380b62`
    too (16/20 replays): a clashed, never uploaded file unlinked, then a restart before the queued listing ran, left
    the app's node hidden for good. The relist queue is now kept in `meta` (`relist`) until each listing succeeds and
    requeued at start. Shrunk to 6 steps by a step-skipping replay (`REPLAY_SKIP`, removed again); now in
    `concurrent_regression_cases`. CI, crash-mode fake scenarios (31 PASS) and 10 000-case proptests pass.
  - **M4.6.3:** git row with 11 alternating pairs, index-rewrite note, daemon stats. Spread without a sync
    0.41–3.82; the local clone's writeback slowed the local side, so `sync -f` after it: 3.47 / 3.10 / 3.16
    (accepted). The row now fails steadily (limit 2.0). `git status` does not rewrite the mount's index (creates and
    unlinks `index.lock` per run); `stats --reset` resets latency only, so uploads are logged as a difference.
  - **M4.6.4** (E14): `download_block_into` writes into the caller's buffer through a `Cursor<&mut [u8]>` lent via a
    `tokio::sync::Mutex` (`authed`'s closure is `Fn`); a full buffer (`WriteZero`) or a claimed size above it is an
    integrity error; `fetch_block` sizes its buffer from the claimed size; `copy_block` removed. Backend tests and CI
    pass; the Proton read path is checked in M4.15.
  - Cleanup: a fake-scenarios daemon left mounted by the crash-mode run and a dead `/tmp/.tmp…/mnt` test mount were
    unmounted.

- **M4.5 (2026-10-29, Opus):** baseline measurement, profiling, triage. Outcomes, baseline table, experiments and the
  M4.6.1–M4.6.4 specs: `docs/plans/m4.md`, "M4.5 outcomes".
  - Baseline at `92337ce`, `scripts/bench/m4.sh` three times (`target/m4.5/bench-{1,2,3}/`):
    - git 1.89 / 2.68 / 0.42 (noisy on both sides);
    - untar 14.04 / 14.01 / 15.10;
    - read 1043 / 1081 / 1094 MB/s;
    - slo 14 of 15 each: fsync p99 48 / 35 / 34 ms, setattr p99 1.96 / 1.85 / 1.70 ms;
    - no 429s and no errors.
  - Changes to the runs: the gc row was dropped from runs 2 and 3 at the user's request (SSD writes: about 1.5 GB per
    gc run, its result is SKIP until M4.9). Stopping the first loop with `SIGSTOP` ended the background task and
    killed run 1 after its git row; run 1's other rows were rerun into its report.
  - No profiler: `perf_event_open` gives EACCES (even `cpu-clock`) and ptrace is blocked. Instead, A/B builds of a
    `git archive` copy with patches switched by environment variables (no commit wait, `prepare_cached` in
    `write_rows`, no sidecar for new files, parallel fsync), timed by an untar driver and an slo driver; plus a
    Python bare-disk fsync probe.
  - Findings:
    - untar on the SSD: about 0.5–0.6 s of 1.0 s is the commit wait (D032); with neither the wait nor the sidecar,
      4.5× local; on tmpfs 3.1×;
    - about 9 FUSE requests per file, 1.8 of them `getxattr(security.capability)`;
    - fsync makes three serial flushes, against a bare-disk `fdatasync` p99 of 8.9 ms under the same load.
  - User decisions: **D065** untar ≤ 15× local (was 3×); **D066** fsync p99 < 30 ms (was 10 ms). DESIGN §5.7 and
    §16, D057, `m4.sh` (15.0) and `slo.py` (30 000 µs) were updated.
  - Chosen fixes:
    - M4.6.1: E2 `prepare_cached`, E12 commit waiters;
    - M4.6.2: no sidecar without a base, data and WAL flushed in parallel (D067, to be added by the task). Found
      along the way: `staged_inos` lists `.state` files, so it must switch to data files first, or restart would
      recreate new files empty;
    - M4.6.3: git row, alternating sides and 11 pairs;
    - M4.6.4: E14.
    - M4.8 gained step 3a (the getxattr round trips, `FUSE_HANDLE_KILLPRIV_V2`).

    The left-out E items went to TODO Known gaps.
  - A full `all` run writes about 4–6 GB. A/B runs mostly used tmpfs, plus one confirming set on the SSD.

- **M4.4 + M4.7 (2026-10-28, Opus):** staging descriptors on demand (D063), fork patch P18 and P7 wiring (D058).
  - M4.4: `staging.rs` `FdCache` (`lru` of `Arc<File>` by inode under a `parking_lot::Mutex`; opens and evicted closes
    outside the lock), capacity `open_files_capacity()` = min(1024, soft `RLIMIT_NOFILE` / 4), at least 16;
    `StagingDir::with_capacity` for tests. `create` inserts its descriptor, `reopen` reads only the length, `remove`
    closes first. Gauge `staged_open_files` = LRU length. `BackendError::local_io` (EMFILE/ENFILE → `Transient`) in the
    fake's `io_error` and the Proton adapter's upload source. Daemon `grow_fd_table` comment updated.
  - Tests: `many_staged_files_share_a_few_descriptors` (50 files, capacity 4), `open_file_capacity_follows_the_limit`,
    `running_out_of_descriptors_is_retryable`, mounted `mount_keeps_more_pending_files_than_descriptors` (re-runs itself
    under `prlimit --nofile=512`, 2000 files written and read back with uploads held by a 1 h debounce, then drained;
    about 32 s). Checked that it fails (EMFILE at the first few hundred writes) with an unbounded capacity.
  - M4.7: fork branch `feat/block-parallelism` `cc50981` from `pdmount` (`with_block_transfer_parallelism`,
    `TransferQueue::with_max_degree_of_parallelism`, 2 tests), merged as `2ae4aa3`, fork workspace tests 290 pass;
    pinned. PdMount: `session::TransferLimits` passed to `drive_client`, `proton_backend`, `proton_backend_offline`,
    `Auth` (re-login), `status`, `trash`, probe; `upload.concurrency` 1–4 (error names D033).
  - Correction to the plan: `download_verified` does use the client's download queue (one block permit), so its limit
    went from 6 to 5, equal to `DOWNLOADS`; DESIGN §6.4 says so.
  - `scripts/ci.sh` PASS for both (`target/m4.4/ci.out`, `target/m4.7/ci.out`). `scripts/bench/m4.sh git untar slo`:
    git PASS 1.93, untar FAIL 21.2, slo 13/15 (fsync p99 105 ms, setattr p99 2.19 ms); details in TODO.md Handoff
    notes, files in `target/m4.7/bench-2026-10-28/`.

- **M4.3 (2026-10-28, Opus):** exit benchmark harness (D057; DESIGN §15.1 "Benchmarks", §16).
  - `scripts/bench/m4.sh [git|untar|read|slo|gc|all]`: release build with the fake backend, profile `bench` via
    `tests/scenarios/lib.sh` with `SCENARIO_ROOT=$BENCH_ROOT/root` (disk), fake drive in `/dev/shm`, network profile
    from `RTT_MS`/`UP`/`DOWN` (seeding runs without it), config written per run (`cache.max_bytes` 4 GiB, 512 MiB for
    gc), report `target/bench/m4-<date>/report.md` (a section per run) plus row logs, `daemon.log`, JSON results.
    Rows run in subshells; a stopped row becomes a FAIL row. The mount clone is marked complete in its `.git`.
  - `scripts/testdata/make-untar-dataset.sh` (deterministic, checked byte for byte), `scripts/bench/slo.py`,
    `read.py --only/--json`, `ops.py` fsync every tenth iteration.
  - Engine test `first_bytes_and_listings_cost_the_slo_round_trips` (per-method backend counts from the latency
    histograms): first listing one `list_children`, once; non-prefetched small file one `blocks` + one fetch;
    prefetched one no call.
  - Runs: `all` stopped at git (EMFILE in the checkout, M4.4; daemon stalled with 3834 pending behind 29 parked,
    force-unmounted, state removed); `read gc`: read PASS 1074 MB/s, gc SKIP (peak 2.0 × cap); slo by hand without
    git: all PASS but fsync p99 72 ms. Details in TODO.md Handoff notes.
  - `scripts/ci.sh` PASS (`target/m4.3/ci.out`).

- **M4.2 (2026-10-28, Opus):** fake network profile and E19 (DESIGN §15.1).
  - `fake.rs`: `NetProfile { rtt, up_bytes_per_sec, down_bytes_per_sec }`, `profile.json` (`read_profile`,
    `write_profile`, `clear_profile`), re-read on a changed (mtime, length) stamp in `FakeBackend::enter`, which
    checks `faults.json`, then the injected one-shot failure, then sleeps the call's round trips without the lock.
    Costs: one RTT per call, `list_children` one per 150 children (the extra pages after the read), `get_nodes` one per
    150 ids, uploads three (draft; blocks plus `size / up`; commit, before the pause point), block fetches one plus
    `len / down`. `Links`: one `Mutex<Instant>` "busy until" per direction, shared by the handles of a process.
    `set_profile` for in-memory fakes; `set_latency` unchanged. `with_state` lost its fault argument.
  - E19: `State::children` (parent → name → link, non-trashed) and `State::used`, kept by `put_node`/`remove_node`/
    `update_node` (every insert, rename, move, trash, retention drop and journal record), rebuilt by `reindex` at
    load and moved over on reload. Block reads locate the blob under the lock (`BlockRead`) and read after it.
  - `pdmount fake profile [--rtt] [--up] [--down] <dir>` (no options: print it) and `fake profile clear <dir>`.
  - Tests: `round_trips_cost_time_but_overlap`, `transfers_share_one_pipe_per_direction`,
    `a_daemon_picks_up_a_changed_profile_file`, `faults_fire_before_the_round_trip`,
    `the_child_index_follows_random_changes` (400 seeded steps over two handles on one directory with compactions,
    index and used total checked against a scan after each). Existing fake and engine tests unchanged.
  - `scripts/ci.sh` PASS (`target/m4.2/ci.out`).

- **M4.1 (2026-10-28, Opus):** latency histograms and stats (DESIGN §5.8, App. B `[stats]`).
  - `metrics.rs`: `Op` (23 FUSE ops, 4 read sources, 16 backend methods), `Latency` (`parking_lot::Mutex<Histogram<u64>>`
    per op, µs, 1 µs–120 s, 3 digits; new dependency `hdrhistogram` 7.6 without default features), `Summary`,
    `Metrics::snapshot_latency/reset_latency`, counter `rate_limited`. `Stopwatch` records on drop, logs a slow
    hot-path request at WARN (one line per op per 10 s, with `suppressed`) and makes the slow-path span.
    `TimedBackend` wraps the backend inside `Monitored` (`Engine::open`) and counts `RateLimited`.
  - FUSE: `handle(op, req, ino, body(watch))`; `slow`, `retrying` and `committed` take and pass the stopwatch, so it
    drops right after the reply (after the commit for deferred replies); the slow-path future runs in
    `debug_span!("fuse", op, unique, ino)`. `read_local` returns `(ReadData, ReadFrom)` (`cache.rs`
    `get_local_from`), labelled `read_ram/disk/staging`; the slow branch is `read_slow`.
  - Gauges: `Engine::gauges` (`queue_pending`, `queue_parked`, `queue_oldest_secs` from `Journal::oldest_age`,
    `staged_open_files`).
  - `pdmount stats [--json] [--reset]`, new `crates/pdmount/src/stats.rs` (reply, text table, Prometheus text, textfile
    task); `Request::Stats { reset }` (`reset` defaults to false), `StatsReply.gauges/latency`. Textfile:
    `[stats] prometheus_textfile` (`~/` expanded, must be absolute), written every 15 s via `.<name>.tmp` + rename.
  - Tests: `latency_records_and_summarises`, `a_stopwatch_records_once_when_its_deferred_reply_drops`,
    `slow_requests_log_once_per_interval_with_a_count_of_the_rest`, `timed_backend_times_calls_and_counts_rate_limits`,
    the config test for `[stats]`, three `stats.rs` tests (Prometheus text, text table, atomic replace), and the
    mounted `mount_times_requests`. By hand on a fake mount: `stats`, `stats --json --reset` (histograms empty after),
    and the textfile.
  - Deviations: see TODO.md Handoff notes. `scripts/ci.sh` PASS (`target/m4.1/ci.out`).

- **Handoff split (2026-10-28, Opus; user request):** `TODO.md` had grown to about 27k tokens, read in full at
  every session start (Handoff notes alone about 20k, an append-only log since M3.1). The per-session entries, the
  M0–M3 checklists and the Done list moved here unchanged; `TODO.md` keeps the current state, the standing notes and a
  new "Known gaps" section (about 5k tokens). AGENTS.md (document map, end-of-session rule) and D021 (amendment)
  updated. Before that, the user had `worktree-M4` merged into `master` in the main checkout (fast-forward to
  `799b1dd`, `.claude/` ignored) and pushed to `origin`.

- **M4.0.2.4 (2026-10-28, Opus):** test-script safety (S-7, S-8).
  - `lib.sh`: `private_root` (mkdir 0700, refuse a symlink or another user's directory, chmod 0700) for
    `SCENARIO_ROOT`, which also covers the soak and bench roots, and for the bench scripts' `FAKE_DIR`;
    `remove_fake_dir` (only an empty directory or one with `snapshot.json` / `.seeded*`) replaces `rm -rf "$FAKE_DIR"`
    in `lib.sh` and `scripts/bench/{latency,read}.sh`; `ensure_dataset` removes only `$DATASET/pdmount-m1` and refuses a
    non-empty `DATASET` without it.
  - `tests/scenarios/selftest-lib.sh` (9 checks), run by every `scripts/ci.sh` (also `--quick`). AGENTS.md Commands
    names the refusals.
  - `scripts/ci.sh` PASS (`target/m4.0.2/ci-4.out`).
  - Session summary (M4.0.2.1–M4.0.2.4): 10 000-case proptests after M4.0.2.3: PASS (486 s, `target/m4.0.2/proptests.out`). Fork `pdmount` is at
    `28cacff` (P16, P17). Where to work: this worktree, `.claude/worktrees/M4` (branch `worktree-M4`); nothing pushed.

- **M4.0.2.3 (2026-10-27, Opus):** bounds on remote claimed sizes (S-5, S-6).
  - `proton.rs` `file_size`: the saturating sum of the claimed block sizes wins over a differing claimed size (DEBUG
    log); an empty block list keeps the claimed size.
  - `Layout::new`: a declared block above 4 MiB, an empty non-last block or a sum other than the file size is
    `Integrity` (was a warning); an empty last block stays valid, and the 4 MiB fallback without sizes is unchanged.
    `BlockCache::get` refuses `expected_len > MAX_BLOCK_BYTES` before taking a buffer.
  - `stage()`: a size above `MAX_STAGED_SIZE` (4 TiB) is `CoreError::FileTooLarge` (EFBIG; new variant), before any
    file is created. `missing_blocks(limit)`; the snapshot materializes in batches of 64 and gives up after eight
    batches without progress (was: eight rounds over the whole list).
  - Tests: `the_block_sizes_decide_a_file_size`, `declared_sizes_must_fit_blocks_and_the_file`,
    `a_remote_file_too_large_to_stage_is_efbig`, `missing_blocks(1)` in the staging test.
  - `scripts/ci.sh` PASS (`target/m4.0.2/ci-3.out`).

- **M4.0.2.2 (2026-10-27, Opus):** manifest verification on block-map reads (S-4, S-11).
  - Fork P17 (`feat/block-map-manifest`, `d6e26ba` + `b0da02f`; `pdmount` `28cacff`, pinned): `get_block_map` verifies the
    manifest signature over the listed thumbnail and block hashes (`verify_block_listing`; key selection shared with
    the full download as `manifest_verification_keys`) and refuses an empty block hash. PdMount's `map_error` already
    mapped both errors to `Integrity`; a test now covers the manifest one. `block_map()` caches only `Ok` maps.
  - Before P17 the SDK's manifest check had never run against real data (PdMount never used the full download). The
    official JS and C# SDKs build the same manifest from the listed hashes. Real-account check (user's OK,
    2026-10-27): `MODES=fresh tests/scenarios/run-all.sh sha`, read-only, plus reads of PdMount-uploaded files under
    `pdmount-test/`. First run: FAIL, 989 files EIO in 52 min (no 429): `get_address_public_keys` had never
    worked (its response type does not match the server's `keys/all` answer), so every address-signed file failed
    before any signature was checked. Fixed in the fork (`b0da02f`, docs/sdk-patches.md P17). The failed lookups
    were not cached, so each failing read repeated a listing and a `keys/all` call; Known gaps. Second run (`28cacff`):
    PASS, all 1004 files match, 735 s (cold `sha256sum` 12 min), no WARN/ERROR, no 429, 0 hot-path calls; then 115
    files under `pdmount-test/` (15 per scenario folder, under 20 MiB) read without error. Logs: `target/m4.0.2/real-*`.
  - Behaviour change: a revision whose listing fails the check is EIO on every read (docs/sdk-patches.md).
  - `scripts/ci.sh` PASS (`target/m4.0.2/ci-2.out`).

- **M4.0.2.1 (2026-10-27, Opus):** secrets out of logs (S-1 High, S-2, S-3).
  - Fork P16 (`fix/no-secret-logs`, `ed2a9a6`; `pdmount` `e931883`, pinned): the passphrase, address-key, `/users`,
    probe-body and serialized-passphrase debug logs are gone, and so are the drive SDK's share, thumbnail-block,
    link-move, copy, link-details and trash body logs; trash/share/thumbnail decode errors drop the body. The fork's
    generic decode helpers still put a malformed reply's body into the error (Known gaps, Low).
  - `main.rs` `log_filter`: `RUST_LOG` or `-v`, then `proton_sdk_rs2=warn`, **and** a hard cap that drops every
    `proton_sdk_rs2` record above WARN. Deviation from the plan: the plan's `EnvFilter` alone is not enough, since a
    more specific directive (`proton_sdk_rs2::account=debug`, a span directive) wins over a crate-wide one; the test
    showed the passphrase passing without the cap. Test: `sdk_debug_records_never_reach_the_log` (through the real
    `log`→tracing bridge; dev-deps `log` and `tracing-log`, both already in the lock).
  - `daemon.rs` `open_log`: log 0600, state directory 0700, also for existing ones (test `the_log_is_private`). The
    existing `~/.local/state/pdmount/*.log` become 0600 at the next background `mount`.
  - Sign-in link (D071, proposed): log and notification name the code only; `status`/`login` still get the link
    over the control socket. `notify::argv` factored out (test `the_sign_in_link_stays_out_of_the_log_and_the_notification`).
    D042 status: link-in-log part superseded.
  - `scripts/ci.sh` PASS (`target/m4.0.2/ci-1.out`; its `a1 [second]` failed because a `cargo run` in the worktree
    swapped the fake-backend binary mid-run, so the steps from there were rerun: `target/m4.0.2/ci-1b.out`, PASS).
    Lesson: no cargo commands in the worktree while `scripts/ci.sh` runs.

- **M4.0.1 (2026-10-27, Opus; reviewers and verifier on Fable):** first security review, whole codebase.
  - Threat model D070 (proposed), text in `docs/security/threat-model.md`. Five area reviews and the verification
    table in `docs/security/2026-10-27-review/`. Fix specs and batches: `docs/plans/m4.md` "M4.0.1 outcomes".
  - High S-1: the SDK fork logs the key passphrase, the unlocked address keys (every mount) and the `/users` body at
    debug; PdMount filters only `proton_sdk_rs2::secret`; `-vv` or `RUST_LOG` reach it; the log is world-readable
    (S-3). Mediums: S-2 sign-in link in the log and the notify argv, S-3 log modes, S-4 no manifest check on block-map
    reads (DESIGN §6.3 claims one), S-5 remote claimed size → unbounded staging allocation, S-7 `rm -rf "$DATASET"`.
  - User decision: fix the High and all Mediums before M4.1 (M4.0.2.1–M4.0.2.4, with Lows S-6, S-8, S-11 on the same
    code). Fork patch numbers: P16 secret logs, P17 manifest; M4.7's patch becomes P18. D071 (sign-in link) follows in
    M4.0.2.1. Other Lows/Infos in Known gaps ("Security (M4.0.1)").
  - Where to work (then): `.claude/worktrees/pre-m4`; M4.0.2 was done in `.claude/worktrees/M4` (see above).
  - Existing logs checked by count only (2026-10-27): `~/.local/state/pdmount/{default,second}.log` hold no
    passphrase/key lines (0 matches), but they are 0644 in a 0755 directory; M4.0.2.1 makes them 0600/0700.

- **M4.0 (2026-10-27, Opus):** M4 planned with the user; plan in `docs/plans/m4.md`.
  - User decisions: D056 (M4 workflow: one commit per task, batches per session); D059 (XDG autostart entry instead
    of a systemd unit; the machine has no systemd). Efficiency items are measured first (M4.3 harness, M4.5 triage);
    one user-gated real-account task (M4.15); git.git for the benchmark from kernel.org.
  - Docs: DESIGN §3.1, §12.2, §14.2, §14.4, §16, §17 for autostart; D020's systemd part superseded by D059;
    AGENTS.md (document map, Git exception); docs/pdcli-evaluation.md (`service.rs` not used).
  - Facts found while planning (details in the plan's "Today's state"): the staged-file descriptor limit (hard 4096
    here) blocks the 5k untar benchmark until M4.4; P7 is in the fork but never called; the fake has no latency for
    running daemons; `pins`, `cache_blocks.pin`, `shutdown_grace`, `fsync.remote_paths` and `writeback_cache` exist
    but are unused.
  - Risk noted in the plan: untar ≤ 3× local may be out of reach; M4.5 measures and asks the user.
  - Security (user): a first whole-codebase review by Fable comes before any M4 code (M4.0.1, fixes M4.0.2.n), and
    M4.16 reviews the M4 changes again (fixes M4.17.n).

- **Flaky test fixed (2026-10-27, Opus):** `events_flow_while_a_refresh_lists_again` no longer depends on the fake's
  latency. New fake pause point `FakePoint::ListBeforeRead` (in `list_children`); the test stops the refresh's first
  listing, checks "refreshing, no cursor stored" while it holds, and starts the second event pass before releasing
  it. That pass waits until it holds `event_cursor`, so it runs mid-refresh (the refresh ends only under that lock).
  A paused listing holds the background turn (`revalidate` keeps it across `list`), so nothing else can poll while
  it is stopped. Stress: 0/40 at 8-way and 0/80 at 16-way (before: 8/40 at 8-way). `scripts/ci.sh --quick`
  PASS (`target/g8/ci-flaky.out`).

- **Pre-M4 fix group G8, S-A3 (2026-10-27, Opus):** one effects accumulator for tree mutations (`vfs.rs`).
  - `Effects` collects rows, touched directory entries (each with its node before the first change) and attribute
    changes; `Effects::finish` derives every invalidation from before/after (rule in DESIGN §9.5). `Tree::mutate`
    wraps `split_conflict`, `yield_name`, `adopt_remote_placement` and `resurrect_minimal`; the listing records its
    whole directory (`Effects::listing`, since it takes the children) and finishes itself. `place`/`unplace` are
    the only entry primitives; `remove_subtree` unplaces its top; `detach_from_parent` and the B20 special case in
    `lose_name` are gone (the listing's before/after covers it), as is `lose_name`'s `dir_listed`.
  - Behaviour changes, all toward exactness: a node that a listing moves out of another folder now also drops that
    folder's cached pages (`Inode` on the old folder was missing); one invalidation per name (a replaced name sent
    both `Delete` and `Entry`); nothing for entries of a folder that is removed in the same mutation (resurrection
    sent `Entry`/`Inode` for the folder a moved-in node left). Order: per directory, names then the directory, then
    attribute inodes.
  - Tests: eight `vfs` tests now assert exact invalidation lists (one reordered); new
    `a_node_renamed_into_a_removed_folder_goes_home_without_touching_the_folder`.
  - Checks: 10 000-case proptests PASS, lock-order check on (`target/g8/proptests-sa3.out`, 445 s);
    `scripts/ci.sh` (default set) PASS (`target/g8/ci-sa3.out`).
  - S-A2: deferred by the user as an M4 option, picked up only if flagged (Known gaps, with the pros and cons).

- **Pre-M4 fix group G8, first part (2026-10-27, Opus):** structural items S-A7, S-A6, S-L3, S-L1, S-L2.
  - S-A7: the lock order is ranked (staging, staged map, access, tree, leaves) in the `engine.rs` module doc and
    DESIGN §4.3, and checked: `lock_order.rs` wraps those locks; debug builds panic when a lock is taken while one of
    equal or higher rank is held. The registry is a true leaf (B28's "documented rule" was the wrong way round; taking
    it under the tree is fine). The check found one real hazard at once: `resolve_vanished_now` read the tree inside
    the arguments of `record_conflict`, so the guard lived through the call, which reads the tree again; with a writer
    waiting between the two reads (parking_lot is fair) the thread deadlocks. Fixed; the engine tests that reach it
    (`m3_…`, `m6_…`, `events_keep_local_changes`, two trash tests) failed under the check before. 10 000-case
    proptests pass with the check on (`target/g8/proptests-sa7.out`).
  - S-A6: a semaphore permit per running upload replaces `uploads_running` and the `upload: bool` threaded through
    13 call sites; the permit is dropped before the outcome wakes the loop, and a panicking upload no longer leaks
    its slot.
  - S-L3: "needs listing" is `Err(NeedsListing)` everywhere (`Lookup::NeedsListing` and `opendir`'s `Ok(None)` are
    gone); FUSE `lookup` and `opendir` go through `retrying` (4 slow-path attempts like the other ops).
  - S-L1: typed control-socket replies in `control.rs`, used by the daemon to build and the CLI to read
    (`call_ok`, `ask`); the wire format is unchanged except the never-read `read_only` field. A wrong or missing
    field is now an "invalid daemon response" error instead of a silent 0. `queue_counts()` once per Status.
  - S-L2: `run_foreground` split into `open_backend`/`open_proton` (an `Opened` struct replaces the 5-tuple),
    `wait_for_stop` and `shutdown` (returns the commit result; tokens are still saved when it fails).
  - Checks: `scripts/ci.sh` (default set) PASS on the tree with all five items (`target/g8/ci-2.out`); 10 000-case
    proptests PASS on the final tree, lock-order check on (`target/g8/proptests-final.out`, 454 s).
  - Left: S-A3 (done in the next session) and S-A2 (M4 option).
  - Flaky test, pre-dates G8: `events_flow_while_a_refresh_lists_again` (fixed since, see above).

- **Pre-M4 fix group G6, first part (2026-10-26, Opus):** behaviour-preserving deduplication.
  - Engine: `Tree::subtree` (four walks), `split_conflict` on `yield_name`, `trash_finished` through
    `drop_detached`, `Tree::has_pending_work` (twice), `settle_name_clash` through `fresh_live`,
    `fresh_or_vanished` (five preambles), `UploadError::retry`, `events::write_cursor` (twice), `BlockKey::new`,
    `Content::of`, one name limit (`vfs::MAX_NAME_BYTES`), `load` deletes stale rows through `write_rows` (now also
    drops their unreferenced `remote` rows), a doc comment moved to `ensure_allowed`.
  - Backend: `fetch_block_into` is the required trait method, `fetch_block` the default (`MAX_BLOCK_BYTES`,
    `copy_block`); the Proton adapter's download is `download_block`. `upload_block_size` comes from the SDK
    (`target_block_size`, plaintext per block despite its doc). `hex` replaces three hand-written encoders.
  - Core infra and CLI: `store_layout` (twice), `paths::sqlite_files` (four loops), `doctor::find_in_path` reused by
    `notify`, `access::parse_mode`/`RuleSource::as_str`/`Connectivity::parse` in core, `reply_empty` in the FUSE
    adapter (six matches).
  - Bug found by the 10 000-case proptest while checking this part (pre-dates the review: `a24c39b` fails the same
    replay on its first run): a listing that finds a folder gone removed its whole subtree, including a node the
    same listing shows here (the remote moved it out, then trashed the folder). The listing then needed an inode
    number it had not counted and panicked (`counted above`), so the event page failed for good. Followers inside a
    removed subtree are now taken out first (`Tree::detach_from_parent`). Test
    `a_node_moved_out_of_a_vanished_folder_survives_the_listing` (fails on the old code). The failing run's output is
    in `target/g6/proptests.out`.
  - Left for later, optional (user, 2026-10-27; report S-E4–S-E6, S-E9, rest of S-E10/S-E11, S-B2, S-B9, S-I1,
    S-L4 `Ids`, S-L6 `Ctx`/home dir): the staging-lock helpers, `bind_remote`, `keep_pending` returning renamed names, the write/truncate pairs, the
    journal outcome prologue and wrappers, `Backend::revisions`, `ReadData` → `Bytes`, the FUSE id/TTL struct and a
    command context in `main.rs`. These touch lock-ordered code or many call sites and deserve their own session.

- **Pre-M4 fix group G5 (2026-10-26, Opus):** dead code and the conflict registry.
  - Removed: `ConflictKind::InterleavedRevision` (§10.8; old rows of it are skipped at load), the never-set
    `remote_device` (struct fields and CLI output; the column stays, unused, §13), `Renamed.old_parent/old_name`,
    `CoreError::{NotPermitted, FileTooLarge}`, `Staged::base_range`'s alias (the method is now `base_range`),
    `SignIn::expires_at`, `start_login`'s unused bool, `Mounted::join`, `Engine::is_listed`, the fake's migration
    from the old `state.json` format and its test.
  - `ConflictNote::new(kind, ino, orig_name)` plus struct update syntax replaces the 13 ten-field literals.
    `Conflicts::load` tracks the last id directly. `conflict_resolve` writes the state once (durably) and looks the
    conflict up with `Conflicts::get`, as `find_conflict` now does.
  - Test-only: `Engine::conflict_count`, `Conflicts::count_active` and `Tree::apply_listing` are `#[cfg(test)]`.
    Narrowed: `map_error`, `MODE_METADATA_KEY`, `ProtonBackend::client`, the fake's two default constants,
    `Gate::foreground_in_flight`.
  - One minimum for `cache.ram_bytes`: `pool::MIN_RAM_BYTES`, used by the config check and `Pool::new`.
  - Kept, with reasons: `kernel.writeback_cache` (M4 evaluates it, §5); `timeouts.shutdown_grace` (unimplemented;
    now an M4 Known gap); `control::remove` and `TokenSaver::snapshot` (used, and they name the intent);
    `RamCache.bytes` (E11 replaces that structure in M4). Skipped: S-C2 (embedding `ConflictNote` in `Conflict`;
    about 40 field accesses for little gain).

- **Pre-M4 fix group G4 (2026-10-26, Opus):** backend error classes.
  - B12: a reqwest error whose connection broke mid-request (`is_request`, `is_body`, `is_decode`) is `Transient`, not
    `Other`; `is_connect` stays `Offline` (Appendix A). Test `connections_that_break_mid_request_are_transient`
    (local servers that close early; fails on the old mapping).
  - B27: `ProtonBackend::fetch_block` downloads through `block_downloader().download_verified` and checks the claimed
    plain size itself, so a mismatch is `Integrity` instead of the SDK's untyped `ensure!` (`Other`). No test: it needs
    a real block map.
  - B26: a file revision without a claimed size takes the sum of its claimed block sizes; without a claimed mtime, its
    commit time. The P4 gap stays: a claimed size from P4's encrypted-size fallback is used as is. No test (needs SDK
    node structs).
  - S-B3: `BackendError::{is_unreachable, is_permanent, needs_the_user}` replace the journal's `permanent_reason`,
    `parks_at_once` and its inline list; same behaviour. S-B8: `map_error` finds every typed cause through one
    `cause::<T>()` chain search.

- **Pre-M4 fix group G3 (2026-10-26, Opus):** sync freshness.
  - B2 (D054, accepted by the user; supersedes D046's second bullet): `Engine::list` is answered by a listing that
    started after the call. Each listing takes a number from `listing_epoch`; after joining an older one, `list` lists
    again (at most two rounds). `ensure_listed` alone takes any listing. `yield_to_remote` lists once instead of up to
    three times. Test `a_listing_request_is_not_answered_by_an_older_listing` fails on the old behaviour. It holds
    the tree lock with std sleeps: a listing blocked on the lock can hold the worker that drives tokio's timers, so a
    tokio sleep there never woke.
  - B16: the six places that stored a freshly read remote state without a stamp use `Tree::set_remote_at`. Covered by
    the unit test `a_listing_fetched_before_a_stamp_leaves_the_node_alone` (helper level; the sites themselves are
    covered by CI and the proptests only).
  - B15 dropped, a false positive: `resolve_vanished` runs inside a listing that already holds a gate turn in the
    background (events, revalidation); a nested `gate.background()` would deadlock on the turn mutex, and in the
    foreground wait forever. Comment added.
  - S-A5 (running `resolve_vanished` outside the listing flight) not done: with D054 it no longer narrows a stale
    window, and it would change when resolution happens relative to the listing.
  - D055 (user's choice to fix it inside G3): the 10 000-case `two_devices_and_an_app_converge` failed, reproducibly
    on the code before G3 too (probably the M3.21 run's lost failure). A restart aborted `yield_to_remote` while it
    listed, before the conflict was recorded, so nothing showed the remote node again. The record now comes before the
    listing, open `CreateCreate`/`NameTaken` conflicts with a missing other copy list their folder at start, and a
    failed listing goes to the relist queue. Replay: 200/200 after, failed within 2–47 runs before; regression case
    in `concurrent_regression_cases`. The failing run's output is in `target/g3/proptests.out`.

- **Pre-M4 fix group G7 (2026-10-26, Opus):** tests and scripts, from the review report.
  - B17: `make_workdir`'s EXIT trap (`scenario_cleanup`) clears a fault the scenario injected, so a failed m4/m5/m7/e2
    no longer leaves A offline for the scenarios after it. `need_uid_a`, `a_offline` and `a_back` replace the copies in
    m4, m5, m7 and e2.
  - m1, m3, m6 and m7 no longer sleep `M_WAIT` (45 s, removed). `reach_b_while_held` sends a marker into the
    changed directory; B's listing that shows it shows the change (§15.3). The scenarios now check that B applied
    the change while the file was open. They are hardly faster: the marker waits for B's next poll, about as long as
    the old sleep (CI 2084 s; m7 312 → 250 s, m1 95 → 122 s, m3 and m6 unchanged).
  - Checked by hand: a scenario that fails while A is offline leaves `faults.json` empty (throwaway script; m4 with
    `QUEUE_TIMEOUT=2` failed only after its own clear, so it proved nothing).
  - `kill_daemon` (lib.sh) replaces kill9's `sleep 1` and the crash branch's copy. ci.sh keeps kill9's log on failure.
    r4 patches with one `dd bs=1M` instead of 1 M one-byte writes. g6 lost a one-item loop. latency.sh waits for each
    client and removes its temp directory on exit.
  - Engine tests: one `eventually`/`eventually_within` replaces `until` and four hand-written loops. The `.pdignore`
    loop now fails instead of going on silently. The pacing test runs on paused tokio time (exactly 6 calls; was a
    1.2 s wall-clock window with 2..=6). `EngineOptions::in_dir` replaces five literals; `mounted.rs` has
    `setup()` and `git_command()`.
  - Not done: e2 still waits out its 90 s outage, which §15.3 defines (shortening it is a spec change).

- **Pre-M4 fix group G2 (2026-10-26, Opus):** correctness, from the review report; each with a regression test that
  fails on the old code.
  - B8: a staged read plans again when a write moves its base pieces, instead of EIO (DESIGN §6.5; test
    `a_staged_read_survives_a_write_while_it_fetches_the_base`).
  - B9: keep-remote is refused for a local-only copy (D053, user's choice; §10.8).
  - B10: `.pdignore`: `*` and `?` stay within a name, and a middle `/` anchors (§7.7).
  - B11: staging writes and truncates put the data first, then the sidecar. `reopen` finishes a cut whose sidecar save
    was lost (§7.1).
  - B13: `fsync` persists the cleared deferred error (test `fsync_reports_a_deferred_error_once_across_a_crash`).
  - B18: conflict names cut the device label at 64 bytes, not chars (§10.7).
  - B19: `conflicts_for` also finds the other copy known only by its link, so the xattrs mark it. `find_conflict` lost
    its fallback, which took the registry under the tree lock (B28).
  - B20: `lose_name` sends the Delete invalidation for a folder whose only pending items were renames inside it.
  - B25: `Layout::new` refuses zero blocks for a non-empty file; `materialize` refuses a short base read.

- **Pre-M4 fix group G1 (2026-10-26, Opus):** session, token and config safety, from the review report.
  - B1: allow-file mode with the keyring unavailable and no `cache.key` creates a key only when no cache encrypted by
    the old one exists; otherwise the command fails (D029's "only missing is no entry"; DESIGN §12.4). Test
    `an_unavailable_keyring_is_not_a_missing_cache_key`.
  - B3: the daemon's watchdog reads the mount table again after an error instead of unmounting.
  - B4: a failed store commit at shutdown no longer skips the token save and the pid file removal.
  - B5: `unmount --force` waits 30 s (was 10 s) before SIGKILL.
  - B6: `release_access_rules` (scenario lib) removes only rules under `/pdmount-test/`. Checked read-only:
    `access_rules` in the `state.db` of `default` and `second` is empty; nothing suggests user rules were lost (only
    test scripts set rules as the sandbox user).
  - B7: the control socket backs off 100 ms after a failed `accept`.
  - B22: `Auth::adopt` takes the saver lock before storing the new session.
  - B23: `reset`, `trash empty` and `status` refuse while the daemon is still shutting down
    (`daemon::refuse_while_running`; DESIGN §14).
  - Only B1 has a regression test; B3–B5, B7, B22 and B23 are shutdown or race paths without a test harness, checked
    by review and CI only.

- **Pre-M4 review (2026-10-26, Opus; report only, user's choice):** `/code-review high` on 4 partitions (engine
  core, core infra, backend, FUSE and CLI) plus `/simplify` angles over all crates, tests and scripts; every bug
  candidate checked by hand against the code (no scratch tests). Report: `target/pre-m4-review/report.md`
  (`target/` is not versioned; the report is the only copy).
  - 29 bugs (B1–B29) plus 2 fake-fidelity items (F1–F2), 19 efficiency items (E1–E19, M4 input), and about 60
    simplifications, in fix groups G1–G8.
  - Most serious:
    - B1: in allow-file mode a transient keyring error creates a new cache key and deletes the SDK and block caches.
    - B2: event and refresh relists join a stale shared listing, and the cursor moves past the change (contradicts
      D046's premise).
    - B3: the watchdog unmounts on a `/proc/self/mountinfo` read error.
    - B4/B5: shutdown can skip `save_tokens`.
    - B6: `release_access_rules` removes all access rules of the profile in `second` mode (real account).
  - Decisions touched: D046 (B2, shadow entries), D034 (E9), D049 (E14: `block_downloader().download_verified` writes
    into the caller's buffer without a fork patch).
  - Dropped candidates are listed in report §5 so they are not re-raised.

- **M3.21 (2026-10-25, Opus):** exit check against §16 and a consistency pass. Results in `target/m3.21/`.
  - `scripts/ci.sh --full` (proptests at 10 000 cases, fake matrix `live remount crash fresh second` × 31 scenarios,
    kill9): PASS. pjdfstest: PASS (771 passed, 625 expected failures). fsx 1 h: PASS (44.3 M small, 12.5 M large ops;
    binary built before the D052 fix, which single-device fsx does not reach). Soak (M3.19) and real account (M3.20)
    passed earlier. A second `ci.sh --full` after the review fixes passed too. §16 reading: M2 is the engine test `m2_edit_edit_interleaved_at_commit`; A5 is skipped in `second`
    mode (§15.3); D051 for the real-account M cases.
  - The 10 000-case two-device proptest found two faults, both fixed with regression cases in
    `concurrent_regression_cases`:
    - Engine (D052): a folder that lost its name to a remote rename while it had pending items split off and took its
      clean bound children along, so they vanished from the remote folder. `lose_name` and `resurrect_minimal` now share
      `Tree::keep_pending`; for `lose_name`, nodes moved in keep their binding (their pending move goes on).
    - Harness: the ack of a write was looked up by path after the write, but a create/create yield could rename the file
      meanwhile and record a stale hash ("content acknowledged … is unreachable"). `apply` now returns the inode; acks
      are keyed on it.
  - One further 10 000-case run failed once; the output was lost (filtered before saving). Reruns at 10 000 (all four
    proptests) and 20 000 (`two_devices_and_an_app_converge` alone) passed, and so did `ci.sh --full`. If it recurs,
    save the full output first; the failing case then minimises by delta debugging over the printed steps, under
    parallel load (a single replay rarely fails).
  - `/code-review` (high) over `m3-start` to the working tree, 9 candidates, checked by hand; fixed with tests:
    - D052 fix, second part: a node moved into the folder kept its link, but its subtree was handled as the folder's
      own (clean children removed, pending ones unbound). Stay-bound nodes now keep their subtree. Renames inside the
      folder that lose are now recorded (`RenameRename`), as `resolve_vanished` records them.
    - `trash_check`: a folder counted a child folder it had deleted as unchanged without listing it; in one batch a new
      file inside that child went to the trash with the parent, unrecorded. The parent now waits for deleted child
      folders (engine test `a_local_folder_delete_keeps_what_is_new_in_a_nested_folder`).
    - `Auth::save_tokens` takes the token snapshot under the saver lock (an older snapshot could overwrite a newer one;
      refresh tokens are single-use); the daemon login swaps the saver and session under it too.
    - `Journal::replanned`: after the burst it backs off (1 s doubling to 300 s) instead of a flat second (D046 text).
    - `BlockCache::seed` takes a slot without waiting for readers (the commit waited up to 5 s per block).
    - Not changed: `Degraded` nodes read as absent (pre-M3; Known gaps); the O(inodes) scan per delete page (M4, Known
      gaps); closing M3 with the lost proptest failure: the user kept M3 closed (2026-10-26).
  - Consistency pass over §7.4, §9.2/§9.3/§9.6, §10.3–§10.8, §12, §13, §15, §16, §17: §9.2 polling cadence (interval
    from the end of a pass); §9.3 rename row (D052); §10.8 notifications as built (D042), `interleaved_revision`
    reserved; §15.1 fake toggles and real-account rows; §16 M3 row (D050, D051); §17 the local-delete trash window and
    a conflict record lost before the group commit. Spot checks: the rename race is branch A (server enforces
    `OriginalHash`); duplicates from nodes moved into a remotely trashed folder are §10.5 (D046).

- **M3.20 (2026-10-25, Opus; real account, user's OK for steps 1 and 2):** profile `second` re-signed in to `<test account>`
  by the user (device `adf6248f…`; `default` is `75f309cb…`). Results in `target/m3.20/` (`m.out`, `second.out`, logs).
  - First m1 run failed the 40 s marker check (B→A after about 39 s): B's debounce and upload (~6 s) + A's wait (30 s
    after the end of the previous poll, as the official event manager) + A's paced poll, `get_nodes`, listing (~9 s).
    The user chose D050: `send_marker` starts the clock when the writer's queue is empty (as the M3.18 plan text already
    said). Checked on the fake (m1 live, r1 second), then rerun.
  - Step 1: m1 m3 m6 m7 live, 4/4 PASS (97–138 s each); markers 2–18 s.
  - Step 2: `second` matrix (r1–r7, g1–g7, n1, a1–a4, a6; `R_FILES=200 R_BIG_MIB=20 G3_COMMITS=50`), 20/20 PASS,
    17:23–20:27 local (3 h; g3 1426 s, g5 1373 s, g2 1167 s, r6 1130 s). Scenario markers 3–33 s (max r7), catch-up
    barriers up to 69 s (limit 900). No HTTP 429. G7 verifies through `fresh` by design, so it checks no data on B.
  - Step 3 (`live fresh` rerun) not run: the user chose steps 1 and 2 only.
  - **Exit reading (confirmed by the user):** §16's "M1–M7 … on two real profiles" is read with §15.3's "selected cases
    repeated on two real profiles": M2, M4, M5 and the folder and app parts of M7 need fake injection and are covered on
    the fake (M3.18); the real account ran m1, m3, m6, m7.
  - The login link printed `&amp;` in its query (`app=drive&amp;pv=3`); sign-in worked anyway. Not in our sources
    (probably the server's URL); not investigated.

- **M3.19.3 (2026-10-25, Opus):** `Backend::fetch_block_into` (default: `fetch_block` and copy; longer than the buffer →
  `Integrity`); the fake reads into the buffer (`read_range_into`, `pread` from the blob), `Monitored` forwards it, the
  Proton adapter keeps the default. The cache downloads into the slot, then seals, writes and opens it in place.
  - `cache::ReadData { Slice(Bytes), Owned(Vec<u8>) }` (derefs to `[u8]`, compares with anything `AsRef<[u8]>`):
    `Engine::read_local`, `read`, `read_revision` return it. One block: a slice of its slot. Across blocks:
    `ReadBuf` in a scratch slot (pool class `SCRATCH`, 4 × 1 MiB, outside `ram_bytes`; `pdmount-fuse` asserts
    `FUSER_THREADS <= SCRATCH_SLOTS`); the slow path takes it after the first block arrives. Larger reads and staging:
    the heap. `assemble`/`append_span` are gone. FUSE replies from the slice unchanged (`reply.data(&data)`).
  - Tests: the allocation test also reads through `Engine` (1000 RAM hits, 100 disk hits, 100 reads across a block
    boundary: no allocation of 64 KiB or more, no fallbacks); fake `fetch_block_into` in memory and from a blob.
  - Not measured yet: `scripts/bench/read.sh` after the pool (compare with the M3.19.1 table below).

- **M3.19.2 (2026-10-24/25, Opus):** `crates/pdmount-core/src/pool.rs` (new): one `MAP_POPULATE` mapping per class
  (64 KiB, 1 MiB, 4 MiB; slots 40 bytes larger, laid out like a sealed disk-cache file, plaintext at 24), free lists,
  `SlotMut` frozen into `Bytes` with `Bytes::from_owner`, `Buffer` (slot or heap fallback). The 32 MiB default gives 79,
  7 and 5 slots (5/8 rounded to whole slots, so one 4 MiB slot per download in flight). DESIGN.md §3.4, §6.2, App. B.
  - `cache.rs`: the RAM LRU holds slots and evicts per class when a slot is needed; `get_local` never waits (heap fallback,
    `cache_pool_fallbacks`, not kept); downloads and `seed` wait up to 5 s. `DiskCache` seals/unseals in place; **disk
    hits are already read with `pread` into a slot and decrypted in place** (planned for M3.19.3, moved here because the
    M3.19.2 allocation test needs it). A download is still copied from the backend's `Bytes` into a slot, sealed, written
    and opened again in place (M3.19.3 replaces the copy with `fetch_block_into`).
  - `seed` is async now, so `Journal::commit_upload` is split into a sync `record_upload` (under the locks) and an async
    tail.
  - Config refuses `cache.ram_bytes` below 16 MiB; test configs use 16 MiB; `scripts/bench/read.sh` defaults to
    16 32 256. `pd stats` prints `cache_pool_slots_<class>  <n> of <m> in use`.
  - `unsafe`: the workspace denies it; `pool.rs` and the counting allocator in
    `crates/pdmount-core/tests/block_allocations.rs` allow it locally (mmap, raw slot slices, `GlobalAlloc`).
  - Found while testing: a read over many blocks held every block's `Bytes` (`try_join_all` in `read_layout`, the
    collecting loops in `read_local`/`read_revision_local`). With the fake's 4-byte blocks that pinned all 79 small slots,
    and every further download waited the full 5 s (`writes_racing_upload_commits_are_kept` went from 10 s to 18 min).
    Reads now copy each block out as it arrives (`append_span`, `buffered(READ_BLOCKS_HELD = 2)`); test
    `a_read_over_more_blocks_than_pool_slots_holds_few`.
  - Tests: pool unit tests, three cache tests (held slots and hot-path fallback, a download waiting for a held slot,
    `seed` round trip), the allocation-count test (1000 RAM hits and 100 disk hits after warm-up: no allocation of 64 KiB
    or more; checked to fail with heap buffers).

- **M3.19.1 (2026-10-24, Sonnet):** `cache.ram_bytes` default 256 → 32 MiB (config, config test, `tests/scenarios/lib.sh`,
  DESIGN.md §6.2 and App. B). New `scripts/bench/read.sh [ram_mib ...]` (+ `read.py`): fake release mount, 64 MiB file,
  sequential 1 MiB reads, 2000 random 128 KiB reads, 2000 mmap copies of 128 KiB; kernel page cache dropped before each
  test. Needs `cargo build --release -p pdmount --features fake-backend`.
  - Results (MiB/s, p50/p99 per read in µs; `disk` = after remount, `ram` = same mount again; 16 blocks in the file):

    | ram_bytes | state | seq | rand | mmap |
    |---|---|---|---|---|
    | 8 MiB | download | 871, 304/9014 | 158, 11/4633 | 273, 7/7054 |
    | 8 MiB | disk | 649, 213/9669 | 157, 12/5357 | 321, 7/4645 |
    | 8 MiB | ram | 839, 229/5285 | 159, 11/4250 | 339, 7/3964 |
    | 32 MiB | disk | 527, 221/26492 | 258, 11/4599 | 461, 6/5385 |
    | 32 MiB | ram | 944, 237/6811 | 287, 11/3453 | 471, 7/4673 |
    | 256 MiB | disk | 594, 259/11596 | 6776, 10/78 | 9400, 6/80 |
    | 256 MiB | ram | 4820, 181/576 | 6361, 11/88 | 8685, 5/110 |

  - Reading: sequential reads need only a few blocks (8 MiB is enough; 32 MiB is no worse). Random reads over a file
    larger than the RAM cache thrash it (a 128 KiB read decrypts a 4 MiB block): 6.8 GiB/s with 256 MiB (the whole file
    fits) against 0.16–0.29 GiB/s with 8 and 32 MiB. The kernel page cache (`FOPEN_KEEP_CACHE`) serves repeat reads in
    practice; this bench drops it on purpose. The `disk`/`ram` rows of one size share the RAM cache of the `seq` test
    before them, so the 256 MiB `disk` rand/mmap numbers are RAM hits.
  - M3.19.2 note: its pool needs at least 2 slots of 4 MiB + 40 bytes in the largest class (about 5/8 of `ram_bytes`),
    so `ram_bytes = 8 MiB` will be refused at startup; the 8 MiB bench size needs a different size then (16 MiB).
  - Soak/release note: `target/release/pdmount` now has the fake backend built in (the bench needed it).

- **M3.19 soak, the run that counts (2026-10-25, Opus):** 3 h, all five checks PASS. (c): A 64 → 71 MiB (×1.10), B 60 → 63
  MiB (×1.05); A was at 64, 68 and 70 MiB after 1, 2 and 3 h. (a): 4 failures, all inside the 7 outage windows. 359
  commits converged; no panics, no hot-path network calls. Results in `target/soak-results/full-3h/`.
  - After M3.19.3 the compressed soak still failed (c): A ×1.44, B ×1.45 (`target/soak-results/post-pool/`). Diagnosis:
    a parser of the heaptrack trace (live bytes per stack at two times; heaptrack's massif export loses the detail)
    showed live state grows only about 5 MiB an hour; the rest of the growth was the old heap RAM cache filling. RSS
    growth was glibc keeping freed buffers of 1 MiB and more (its mmap threshold rises to the largest freed mmapped
    chunk): `journal::hash_file` (1 MiB per upload), the fake's `stage_blocking` (1 MiB per upload) and the fake's
    `refresh`, which reread the whole journal (growing to 8 MiB) after every append by the other daemon.
  - Fix: 64 KiB stack buffers in both loops (DESIGN §6.2), and `refresh` reads only the journal after its offset. The
    compressed rerun passed (A ×1.11, B ×1.05; `target/soak-results/no-churn/`).
  - Runner: `target/soak-bin/run-soak.sh compressed|full <name>` (untracked; copies logs to
    `target/soak-results/<name>` every 5 min and unmounts at the end). Rebuild `target/soak-bin/pdmount` with
    `cargo build --release -p pdmount --features fake-backend` first.

- **M3.19 soak, first runs (2026-10-23/24, Sonnet then Opus):** `tests/soak.sh` works.
  - Checks (a), (b), (d) and (e) pass. Check (c) fails: 3 h run A ×1.64, B ×1.86; compressed 1 h runs about ×1.3.
  - heaptrack 1.5.0 is built in `<tools dir>/heaptrack` with libunwind 1.8.1 in `<tools dir>/libunwind`; sources and build
    fixes in `<tools dir>/src/`. It finds no leak.
  - Cause: glibc malloc keeps freed 4 MiB block buffers (`seed`, disk hits, `assemble`). With
    `GLIBC_TUNABLES=glibc.malloc.mmap_threshold=1048576:glibc.malloc.arena_max=4`, RSS halves and B stays flat. A still
    grows by about 25 MiB (explained 2026-10-25: mostly the heap RAM cache filling, see above).
  - The user decided D049, so M3.19.1–M3.19.3 come before the 3 h run that counts.
  - Results are in `target/soak-results/` (`heap/`, `malloc-ctl/`, `malloc-tuned/`, `ram8/`, `ram128/`). The runners
    are in `target/soak-bin/` (`run-variant.sh`, `run-heap.sh`, `run-malloc.sh`), and the binary copy is
    `target/soak-bin/pdmount`, because `/dev/shm` is noexec. The original 3 h logs were lost in a reboot (`/dev/shm`);
    keep results on disk.
  - Compressed soak (3 h of work in 1 h): `SOAK_DURATION=3600 RSYNC_EVERY=200 GIT_EVERY=10 BEDIT_EVERY=20 APP_EVERY=100
    AEDIT_EVERY=200 FLAP_EVERY=800 FLAP_JITTER=200 TOKEN_FIRST=600 TOKEN_EVERY=1200 OUTAGE_LENGTH=100 SAMPLE_EVERY=20
    tests/soak.sh`, with its own `SCENARIO_ROOT`.
  - Unmount after a run: `(BACKEND=fake SCENARIO_ROOT=<root> PROFILE=fake-soak PROFILE2=fake-soak-2 source
    tests/scenarios/lib.sh && pd unmount && pd2 unmount)`.

- **Runtime-dir bug (2026-10-23, Opus):** a user's first mount failed right after `mounted` with a bare
  `No such file or directory`: the daemon wrote the PID file before `control::serve` created `$XDG_RUNTIME_DIR/pdmount/`
  (since M1; after every reboot too, as the runtime dir is a tmpfs). The sandbox user has no `XDG_RUNTIME_DIR`, so its
  socket falls back to the data dir and nothing caught it. Fixed (`control::create_dir` before the PID file). Fake scenarios
  now set `XDG_RUNTIME_DIR=$SCENARIO_ROOT/xdg/run`, and `scripts/ci.sh` deletes it at the end so the next run's first mount
  creates it. Daemons started before this change (the soak's) have their sockets in the data dir: reach them with
  `XDG_RUNTIME_DIR=` empty.

- **Soak duration (2026-10-23, Opus):** the user cut the M3 soak from 24 h to 3 h and accepted the risk (D048).
  `docs/plans/m3.md` M3.19 now has `SOAK_DURATION=10800`, flaps every 40 ± 10 min, token expiry hourly from 30 min, and an
  RSS check on windows relative to the run length. DESIGN §15.1, §16 and §17 follow. No code changed; the script does not exist yet.

- **M3.18 (2026-10-22, Sonnet):** D047 (proposed). `tests/scenarios/`: `lib.sh` (`PROFILE2`/`MNT2`, `pd2`, `mount_second`,
  `device_uid`, the `second` mode with `send_marker`/`second_marker`, `event_barrier`, and for the M scenarios `prepare_two`,
  `sync_devices`, `hold_open`/`release_hold`, `expect_conflicts`, `fake_fault`), `m1 m3 m4 m5 m6 m7 e2`, `run-all.sh` (default
  list, both profiles' logs for the 429 check), `scripts/ci.sh` (second mode for r1 r3 g3 a1; `e1 e2 m1..m7` in live).
  - M scenarios, e2: live mode only (skip otherwise); m4, m5, e2 and the folder/app parts of m7 need the fake. E1: 40 s.
  - Daemon change (test builds' behaviour only): `watch_fake_session` ends `needs_login` after `fake fault clear`.
  - Timing of `scripts/ci.sh` on a fresh `SCENARIO_ROOT` (2026-10-23, passes): the live/crash set 4.5 min, the second set
    (r1 65 s, r3 102 s, g3 72 s, a1 463 s: a1 waits behind g3's event backlog at its start barrier) 12 min, e1 e2 m1 m3 m4 m5 m6
    m7 about 16 min; about 40 min with the build and tests. Each M scenario with an open file waits `M_WAIT` = 45 s. The fake
    accumulates state (50 000 changes, GBs of blobs) and slows down: delete `/dev/shm/pdmount-tests` now and then.
  - Findings that were not engine bugs: `git status` on B refreshes `.git/index`, a real remote edit, so A deleting the old
    area afterwards revived it (`local_delete_remote_edit`, `rm -rf` ENOTEMPTY): hence the B-to-A marker before each second run.
    Conflicts stay open across scenarios, so `expect_conflicts` filters by path.
  - Not done: the real-account variants (M3.20; the second profile must be signed in again first, see Next action). The
    marker allowance for big scenarios on the real account is unmeasured.
- **M3.17 (2026-10-22, Sonnet):** D046 (proposed). `engine/proptests.rs`: `two_devices_and_an_app_converge` (`Fleet` of two
  `Instance`s with their own runtimes, an app handle, `ack` and `app_written`, `quiesce`, `check_settled`) and
  `concurrent_regression_cases`; `check_tree` (`Tree::check_invariants`) also runs after every step of the single-device
  test. 2000 cases pass (72 s), 8000 passed once (306 s); the M3.21 exit check runs 10 000. It found five engine faults, all
  fixed (D046): a trashed folder failed the event page for good; a shared in-flight listing hid the remote node after a
  create/create yield; a hidden remote node stayed hidden when its occupant was unlinked before upload (`Node::clashed`,
  `Journal::list_later`); a node renamed into a folder deleted on the drive was dropped locally but lives on the drive
  (`resurrect_minimal` re-homing, `move_node` NotFound); a `Replan` hot loop (`Journal::replanned`).
  - Harness notes: the harness retries `NeedsListing` for rename like the FUSE layer (`lib.rs` `resolve`). A failing case
    does not always shrink to a stable minimum, since background tasks make runs differ: replay a printed case in a loop
    (a throwaway test with 200–400 iterations) and cut it down by removing one step at a time.
  - A node that vanishes right after a successful write is not acknowledged (`read_if_present`).
- **M3 planning (2026-10-22, Opus):** the plan was made from a code exploration of the conflict, auth/offline and test
  code; its findings (bugs and gaps with file:line) are in the plan's Context and in each task. Notable: connection
  errors probably map to `Other`, not `Offline` (fixed in M3.2); `timeouts.offline_read` is never read; the `conflicts` table
  and `inodes.remote_newer` exist but are unused; the fake has no pause points or faults for a running daemon.
- **M3.16 (2026-10-22, Sonnet):** D042 (proposed).
  - `proton.rs`: `Live { client, credential }` behind an `RwLock`, `replace_session`, `with_reauth` over a `Reauth` trait (impl
    for `TokenCredential`) around every `Backend` method, block-map and block-download calls included; `ProtonBackend::new` /
    `with_root` take the `TokenCredential`. The closure type is `Fn() -> Result<T>` (backend errors) instead of the plan's
    `anyhow::Result`, so `open_source` errors pass through. Tests: five with a mock `Reauth`. The expired-refresh case could
    not use a real `TokenCredential` (its error type and the response types are private to the SDK), so it checks that a refresh
    error goes through `map_error` (401 → `Unauthorized`, 503 → `Transient`).
  - `crates/pdmount/src/auth.rs` (`Auth`: session behind a tokio `Mutex`, token watcher, 30 min check, login task, sign-in slot,
    last login error); `notify.rs` (`desktop`, `Coalescer` with a deadline so held notices are flushed); `session.rs`:
    `begin_web_login` (no storing) + `store_login` + `end_unstored` + `drive_client`, `TokenSaver::{snapshot, save}` (save runs in
    `spawn_blocking`), `Resumed::NeedsLogin { logged_in, error }`. Deviation: the plan's `begin_web_login(on_sign_in) -> LoggedIn`
    is split so the daemon can refuse another account before anything is stored.
  - Daemon: starts in `NeedsLogin` with local state (`state.db` and `proton-root.json`), else bails. `Request::Login` (fake:
    error), `status` has `sign_in_url`, `sign_in_code`, `sign_in_error`; `pdmount login` talks to a running daemon
    (`login_via_daemon`); `mount` logs in first, `--no-login` fails. `announce_conflicts` notifies on the desktop.
  - Engine: `run_health` now probes on `kick()` while `NeedsLogin` (the probe bypasses `Monitored`'s refusal) and sets `Online`
    on the first answer; D041's "a rejected session is not probed" holds apart from this kick. Test:
    `needs_login_accepts_writes_and_a_kick_after_the_login_drains_them`.
  - Checked by hand: `login` against a mounted fake daemon prints the fake's error. **Not tested:** the web login inside the
    daemon, token rotation and the 30 min check against the real server (a real session; M3.20 or the user: expire the access
    token, or `pdmount login` against a mounted real profile), `notify-send` itself (no session bus here).
  - Do not smoke-test `pdmount login` without a running daemon: it starts a real web login (it only waits for a link, but it
    calls Proton).
- **M3.1 (2026-10-22, Opus):**
  - Fork: P14 `feat/caller-name-hash` (`2137adc`) and P15 `fix/link-trash-time` (`30c061a`), merged into `pdmount`
    `d1c8994`, pinned; the fork's 281 tests pass. Edits to the fork's mixed CRLF/LF files went through a script that
    keeps each region's line endings (`git show --stat` stays small).
  - P15 fixes a real M2 bug on the real account: `trashed` was never set (F16), so an event for a node trashed on another
    device looked like an echo and was ignored. Approved by the user (D044).
  - The probe is `crates/pdmount/src/probe.rs` (`#[ignore]`, command in its module doc; `PROBE_ONLY=S9,…` selects
    scenarios). It needs the daemon's 16 MiB worker stacks (`WORKER_STACK_SIZE`): with the test default it dies of
    SIGSEGV in key parsing. One SDK client instance runs one upload at a time (F5): two uploads through one instance,
    the first held open, deadlock; the probe gives a restarted A its own instance.
  - Profile `second` is signed in to another account (its `get_my_files_share` answers 2501 "Volume not found"); B ran as
    `default`'s session with another client UID. See Next action.
  - All probe `run-*` folders under `pdmount-test/q1/` are in the trash (a few bytes each).
- **M3.2 (2026-10-22, Sonnet):**
  - `map_error` (`proton.rs`): `transport_error` finds the `reqwest::Error` inside `reqwest_middleware::Error`
    (a test shows the plain chain search misses it); connect failures → `Offline`, timeouts → `Transient`. Body `Code` 401
    → `Unauthorized`. `HttpTransferError` (401 → `Unauthorized`, 429 → `RateLimited`, 408/425/5xx → `Transient`, other
    → `Other`) and `TooManyRequestsException` → `RateLimited` with `retry_after`. `reqwest-middleware = "0.5"` added
    (lock entry unchanged). DESIGN.md Appendix A and the §12.3 citations updated. `Offline` was already non-permanent
    in `journal.rs` `permanent_reason`.
  - Not fixable in the adapter: the SDK's own block-download retry (`block/download.rs` `classify_error`) downcasts
    only a bare `reqwest::Error`, so a connect failure through the storage client's middleware is not retried by the
    SDK (it fails at once and we classify it `Offline`). The storage upload loop's "network error on attempt N" is a
    formatted `anyhow!` string, so such an upload failure still maps to `Other`. Both are fork changes; not done.
- **M3.3 (2026-10-22, Sonnet):**
  - `BackendError::InvalidMove` (EINVAL), `RevisionMeta` and the `Backend::revisions` default (`lib.rs`); the fake got
    clients (`FakeClient`, `with_client`), revision metadata (`client_uid`, `sha1_claimed`, `seq`), pause points
    (`pause_at`, `FakePause`), two-phase uploads, the check toggles, `all_revision_sha1s`, `revisions`, and `faults.json`
    (`add_fault`, `clear_faults`, `FaultRule`). The daemon runs the fake as client `pdmount-<device id>`;
    `pdmount fake write|create [--app]`, `mkdir`, `fault offline|unauthorized|clear` exist. DESIGN.md §15.1 and Appendix A
    are updated.
  - `seq` is the fake's id counter at creation (the generation does not advance in an in-memory fake), and
    `revisions()` reports `created = UNIX_EPOCH + seq seconds`. A fault rule is checked by every call that has an
    operation tag (all trait methods); one-shot `inject_failure` fires after it. Pauses are per process (a daemon cannot
    be paused from the CLI).
  - A `fault clear` without `--client` removes every rule; with it, only that client's rules.
- **M3.4 (2026-10-22, Sonnet):** `crates/pdmount-core/src/conflicts.rs` with `conflict_name` (9 tests); `chrono` added to
  core. `attempt` 0 and 1 both mean no number. One addition to the plan: the sanitized device label is cut at 64
  characters (`DEVICE_MAX_CHARS`), so the 255-byte bound holds for any label; DESIGN.md §10.7 says so.
- **M3.5 (2026-10-22, Sonnet):** `Clock`, `system_clock`, `Namer` in `conflicts.rs`; `EngineOptions` has `device` and `clock`
  (the engine keeps both, `Engine::device()` / `clock()`); `Tree::set_namer` and `pick_conflict_name`, called by
  `Engine::open`. Test helpers in `engine/tests.rs`: `test_device`, `fixed_clock`, `CLOCK_SECS` (`pub(super)`, also used by
  `proptests.rs`), `open_device`; `open_with` is device `dev-a`. `mounted.rs` uses the system clock. No behaviour change.
- **M3.6 (2026-10-22, Sonnet):** `store/0003.sql` (schema version 3). In `conflicts.rs`: `ConflictKind` (with `ALL`, `parse`,
  `initial_state`), `ConflictState`, `ConflictNote`, `Conflict`, `ConflictNotice`, `Conflicts`, `store_conflict`, `store_state`.
  `Engine` has `record_conflict` (pub, so unused code does not warn before M3.8), `set_conflict_state`, `conflicts`,
  `conflicts_for`, `conflict_count`, `subscribe_conflicts`. `Node`/`NodeRow` carry `remote_newer` (stored, loaded; nothing
  sets it yet). A conflict row with a kind or state this version does not know is skipped at load (warn) but its id stays
  taken. `Conflicts::set_state` and `Engine::set_conflict_state` are additions for M3.14's `resolve`.
- **M3.7 (2026-10-22, Sonnet):** in `vfs.rs`: `Changes` has `vanished`, `clashes`, `notes`, `remote_newer` (nothing fills them
  yet; `Engine::apply_changes` ignores them with `..` until M3.8 wires them). `split_conflict`, `yield_name` (returns the old and
  the new name), `unbind_tombstone`, `adopt_remote_placement` (`Some(empty)` when the node is in place already, `None` when
  refused), `resurrect_minimal` (returns `Resurrected`; `kept` is in traversal order, parents before children, not sorted),
  test-only `check_invariants`. `split_conflict` puts the base revision (the node's old remote rev) into the note's `local_rev`
  and does not touch the generation counters of X (edit/edit means it is dirty already). DESIGN.md §10.5 names the primitives.
- **M3.8 (2026-10-22, Sonnet):** D040 created (proposed). `Tree::apply_listing_at` implements a)–f) of the task: tombstone
  revival (`Tree::revives`, shared with `unknown_count`), followers incl. `!is_within(dir, ino)` and contested renames
  (`RenameRename` only when the node actually moves), `vanished` instead of `forget_remote_subtree` (deleted), layer 1,
  `lose_name` (remote rename onto a used name) and the lazy/`local_only` rules for a new remote node on a used name.
  `Engine::apply_changes` now returns `vanished`, records `notes`, touches `clashes`/`remote_newer` and sends a
  `ConflictNotice{id: 0, EditEdit}` for layer 1. `Engine::resolve_vanished` (boxed future; spawns listings, never awaits
  one) and `events.rs` `apply_events` call it; `Tree::renamed_in` and `Tree::pending_under_remote` are its helpers. Tests:
  the seven engine tests of the plan, three vfs tests (`local_only` yield, pending node onto a used name, no cycle).
  Property tests pass at 2000 cases.
  - Things the next sessions must know: a clash (`changes.clashes`) is only touched in the journal; M3.10 must resolve it
    (the remote node stays hidden until then). The note for `RemoteDeleteLocalEdit` has `ino` = the top of the resurrected
    subtree and no link; `RemoteDeleteLocalRename` notes are recorded for nodes that are already removed (no path).
    A vanished node keeps its old remote state until `resolve_vanished` has run; the journal may see it in between
    (M3.9 must treat a base revision on a trashed link as "gone").
- **M3.9 (2026-10-22, Sonnet):** all in `journal.rs` (+ `Engine::allocate_inos` is `pub(crate)`, `RemoteState::with_content_of`
  in `vfs.rs`). `upload_now` runs `precheck_revision` before the snapshot; `settle_cheap` (metadata-only → `readopt_remote_content`;
  sizes differ → split) and `settle_with_snapshot` (`remote_matches` → `adopt_base`, else `split_edit_conflict`) are shared with
  layer 3 (`RevisionConflict`). `UploadError::Replan` ends a job with `journal.succeeded`. `commit_upload` returns `bool`
  (the commit guard applies to revision uploads only) and clears `remote_newer`. `remote_newer` bypasses the debounce in
  `upload_wait` (not with an open writer). `snapshot()` parks "content only in the emptied trash" when the base revision is gone.
  11 new engine tests (the eight of the plan, plus the trashed-before-upload case; `m1_two_engines` uses two engines on
  one fake). Property tests pass at 2000 cases; `scripts/ci.sh` passes.
  - For M3.10/M3.11: `Target::New`'s `AlreadyExists` branch still parks (M3.10). `vanished(ino)` (journal) is the helper
    for "the remote file is gone": `resolve_vanished` then `Replan`. `fresh_live(link)` is `get_nodes` for one link, trashed
    counted as gone. Layer 3 and the commit guard have no test of their own (the fake cannot hold another client's draft open).
- **M3.10 (2026-10-22, Sonnet):** all in `journal.rs`. `name_clash` / `settle_name_clash` is the one resolver for `upload_new` and
  `create_folder` on `AlreadyExists` (holder link from the error, else a listing of the parent); it always ends in
  `UploadError::Replan` or an error. Identical file (`remote_matches`, so also without a claimed SHA-1) → `adopt_identical_file`
  (bind, staging `set_base_rev`; the replan is a `Target::Revision` that uploads only a differing mode/mtime); folder →
  `bind_folder` + spawned `list(ino)`; otherwise `yield_name`, `list(parent)`, `record_conflict(CreateCreate)`. `same_content`
  is gone. `create_folder` now returns `UploadError` (`bind_folder` is the old binding block); `execute` maps `move_node`'s
  `BackendError` into it and handles `Replan`/`Park` for steps (M3.11 can return them from `move_node`/`trash`).
  `parent_vanished` (NotFound from create → `resolve_vanished` on the parent) is covered by
  `a_new_file_in_a_folder_trashed_remotely_is_created_again`. Seven new engine tests. Property tests pass at 2000 cases.
  - For M3.11: a holder known by an *attached* other inode is Transient (the next plan blocks on it); a deleted tombstone
    holder is unbound without trashing (unreachable in practice, `plan()` blocks the new node on the tombstone's trash).
- **M3.11 (2026-10-22, Sonnet):** all in `journal.rs` (+ `RemoteState::with_placement_of` in `vfs.rs`).
  - `move_node` is `self: &Arc<Self>` and returns `UploadError`: `fresh_live` first; placement changed →
    `remote_moved_first` (inode follows, `RenameRename`; a remote move to our own target is silent; if it cannot follow the
    local placement is kept and the next plan pushes it); else the rename carries `fresh.name_hash` (the plan's `expected`
    only as a fallback). `NameConflict` → fetch again, same handling, park when the node did not move;
    `InvalidMove` → `keep_server_placement` (`MoveCycle`, park if the inode cannot follow); `AlreadyExists` →
    `yield_to_remote` (`NameTaken`; the tail of `settle_name_clash` extracted, M3.10 tests unchanged); `NotFound` → `vanished`.
  - `trash` is `self: &Arc<Self>`: `get_nodes` for the batch, `trash_check` per entry (`base_of` = staged base or remote rev),
    `keep_remote` (unbind, await a listing of the remote parent, record with `other_ino`), `trash_finished` (the old
    `Ok | NotFound` arm). A folder waits (Transient) while a kid is attached elsewhere.
  - Tests: the eight of the plan, run with the journal alone (`start_journal` in `engine/tests.rs`, so no event loop
    races the executor). Verified by mutation that `a_folder_waits_for_children_moved_out` fails without the wait.
  - **One unexplained failure:** the first 2000-case run of `random_changes_converge_to_the_model` (about 14 s in) failed;
    I only saw the tail of the output. About 115 000 further cases in 55 runs (including 2 × 10 000 and 3 parallel runs) passed.
    If it shows up again, capture the full output (proptest prints the minimal failing input).
  - For M3.14: `LocalDeleteRemoteChildren` has `detail` = the number of foreign children, `other_ino` = the folder's new inode.
- **M3.12 (2026-10-22, Sonnet):** all in `proton.rs`. `get_nodes` uses `get_nodes_uncached_with_name_hashes` and fills
  `name_hash` with the lowercase hex digest (`hash_to_hex`; `list_children` and `root` leave it `None`). `rename` decodes
  `expected` (`hash_from_hex`; a malformed one is `Other`, so the journal parks it) and calls the `_with_original_hash` variant
  of `rename_node` / `move_node`; `None` sends no precondition. `map_api_error`: code 2000 with "out of date" →
  `NameConflict`, with "sub-folder" → `InvalidMove`, other 2000 stay `Other`; code 2511 → `Transient`. DESIGN.md Appendix A and
  F15's status are updated, no new decision. The real API was not touched (the conditional calls were verified in M3.1);
  the first real-account run that renames is M3.20. No own-draft deletion (P11 does it, per "M3.1 outcomes").
- **M3.14 (2026-10-22, Sonnet):** D045 (proposed).
  - The engine side is `engine/surface.rs` (a submodule of `engine`, because it needs the engine's private fields), not
    `conflicts.rs`; the types `ConflictView` and `Resolution` are in `conflicts.rs`. `Engine::conflicts_list` / `conflict_show` /
    `conflict_resolve` / `xattr` / `xattr_names` / `conflict_open_count`; `CoreError::Refused(String)` (EINVAL) carries the
    user-facing refusals; `Journal::entry_state` feeds `parked` / `uploading` / `pending`.
  - The other copy of a conflict is `other_ino` while it is attached, else the node now bound to the conflict's link
    (`local_delete_remote_edit` records the old tombstone as `ino`; the file came back as a new inode).
  - `status` shows only `open` conflicts (notices are in `conflicts list`). `list` hides resolved ones unless `--all`.
  - The daemon logs each new conflict with the command to show it (`announce_conflicts`); M3.16's `notify::desktop` joins there.
  - Tests: six engine tests (`resolve_*`, `conflicts_list_and_show_*`, `xattrs_*`), a mounted `getfattr` test
    (`mount_reports_state_in_xattrs`), formatting and control-serialization unit tests. Checked by hand on the fake with the
    CLI: an edit/edit, `list`, `show`, `getfattr`, `resolve keep-local`.
  - Not tested: `keep-local` with a copy that was never uploaded (the `adopt_on_replace` path; the engine tests of §7.6 cover
    the adoption itself); `keep-remote` of a directory.
- **M3.15 (2026-10-22, Sonnet):** D041 (proposed). `crates/pdmount-core/src/health.rs`: `Connectivity`, `Health` (`set`,
  `observe`, `state`, `since`, `subscribe`, `wait_online`, `kick`) and the `Monitored` backend decorator. `Engine::open` wraps the
  backend (`engine.backend` is the monitored one, so the block cache is watched too); `Engine::health()`; `Engine::run_health`
  (started by `start_background`) probes every `sync.probe_interval` while `Offline` and calls `Journal::wake_all` on the
  transition to `Online`. `run_journal` and `run_events` wait for `Online`; `failed()` leaves `attempts` alone for
  `Offline` / `Unauthorized`. FUSE: `deadline_for` (unit test) shortens the metadata and block deadlines to `offline_read`
  when the state is not `Online`; local `fsync` keeps its deadline. Status: `connectivity` and `connectivity_since` in the
  reply, a `Connection:` line, and `needs_login` shows as the session line. Offline mount: `Resumed::Offline`,
  `ProtonBackend::with_root` / `volume_id`, `proton::is_connection_error`, `session::KnownRoot` (`proton-root.json`, written
  after every online mount), `ProfilePaths::proton_root_file`; the daemon sets `Offline` before `start_background`.
  Five engine tests (names as in the plan) on a fake opened from a directory, switched by `faults.json`; they ran 5 times
  without a failure.
  - **Not tested:** the offline mount itself (needs a real session and no network); `Resumed::Offline` rests on
    `is_connection_error` finding a `reqwest` connect error inside what `ensure_authenticated` returns. M3.20 or the user
    should try it once: block the network, `pdmount mount`.
  - **For M3.16 (done, see its note):** `NeedsLogin` is sticky: `observe` never leaves it, `Monitored` refuses calls, and `run_health` does not
    probe it. The login must call `health.set(Online)`. A session mounted offline is unchecked, so its access token may be
    expired when the connection returns; the reactive 401 refresh (M3.16 part 1) must cover that, or the first request
    sets `NeedsLogin`. The probe task and `kick()` are ready for the login task.
- **Deviations from docs/plans/m3.md:** M3.15: `Resumed::Offline` carries the error as well, so `status`, `trash` and the probe keep their old error; `needs_login` is not probed (the plan says every non-`Online` state); the event task, after a failed poll while not `Online`, waits for `Online` and polls again at once instead of backing off;  M3.14: see above (module location, `other copy` by link, `status` counts open only; the
  notification hook only logs until M3.16); M3.11: the remote state is refreshed with placement and hash only (`with_placement_of`,
  or the hash alone when unchanged), not `set_remote(fresh)`, so a newer revision stays visible to the upload pre-check; the
  keep-case awaits the parent listing instead of spawning it, to name `other_ino`; M3.10: the identical-file case binds and replans through the revision path as the plan says, but the existing crash-adoption test now exercises it too (no separate path); the yield records `remote_rev` and leaves `remote_device` empty; M3.9: the "base is fresh" case and `readopt`/`adopt_base` update only the content fields
  of the node's remote state (`with_content_of`), not its parent and name, because a full `set_remote(fresh)` would make a
  remote rename look like a pending local one and push it back; the conflict note keeps `local_rev` = the base revision that
  `Tree::split_conflict` records (M3.7, tested) instead of `None`; M3.8: `resolve_vanished` builds each chain from fresh `get_nodes` results
  (node, then its parents up to the root, through trashed nodes) instead of the tree's remote parents, so an unknown or stale
  parent link is covered; batches are `events::NODE_BATCH` (100), not 50; nested targets are dropped by an `is_within` check
  instead of a sort by depth; `Tree::unknown_count` counts revivable tombstones and `list_now` allocates what `NeedInos`
  asks for (it spun forever otherwise); M3.7: none; M3.6: `Engine::record_conflict` is `pub` (not `pub(crate)`) to avoid dead-code warnings until M3.8; `set_conflict_state` / `Conflicts::set_state` added for M3.14; rows with an unknown kind or state are skipped at load; M3.1: P14's API is three new SDK methods instead of new parameters; P15 added
  (D044); M3.13 skipped (D043); the M3.12 text is amended by "M3.1 outcomes". Record later ones here and in the task's
  commit body.
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

## Milestone checklists M0–M3

Moved from `TODO.md` once the milestones were done; `TODO.md` keeps the milestone boxes.

### M0 tasks (in order)

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

### M1 tasks

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

### M2 tasks (in order)

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

### M3 tasks (in order)

Specifications: `docs/plans/m3.md` (one task per session, one commit per task, D039). Exit criteria: DESIGN.md §16.

- [x] M3.0 Plan into the repo, decisions D037–D039, SLO amendment (§16), this checklist; tag `m3-start`
- [x] M3.1 Fork patches P14 and P15 (D044), real-API verification of Q1/Q6: results in `docs/plans/m3.md`, "M3.1 outcomes"
- [x] M3.2 Error classification fixes in the Proton adapter (connection errors, body 401, block downloads)
- [x] M3.3 Fake groundwork: clients, pause points, check toggles, revision metadata, fault file, CLI
- [x] M3.4 Conflict naming (pure function)
- [x] M3.5 Device identity, clock and namer in the engine and tree
- [x] M3.6 Conflict registry, migration 0003, persisted `remote_newer`
- [x] M3.7 Tree resolution primitives
- [x] M3.8 Listing rules: layer 1, remote deletes, rename/rename, tombstone revival, name clashes (D040)
- [x] M3.9 Upload path: layers 2 and 3, identical content, metadata-only races, edit/edit split
- [x] M3.10 Create/create (files and folders)
- [x] M3.11 Namespace executor: rename/rename, move cycles, local delete vs remote edit, folder delete
- [x] M3.12 P14 wiring in the adapter and the M3.1 error codes
- [x] M3.13 ~~Revision post-check~~ skipped by the user (D043): drafts are exclusive per file; only a restore can race
- [x] M3.14 Surfacing: `conflicts` CLI, status, xattrs, notification hook
- [x] M3.15 Health: connectivity state, offline mode, probe (D041)
- [x] M3.16 Auth: reactive 401, rotation, proactive refresh, `needs_login`, daemon login, `mount` login (D042)
- [x] M3.17 Property tests: two devices plus an app; "no acknowledged content unreachable", "two cores converge" (D046)
- [x] M3.18 Scenarios: `second` mode, `m1`…`m7`, `e2`, CI (D047)
- [x] M3.19.1 RAM cache at working-set size (D049)
- [x] M3.19.2 Block pool reserved at startup; the RAM cache and `seed` use it (D049)
- [x] M3.19.3 Reads through the pool: disk hits, downloads, replies (D049)
- [x] M3.19 Soak script and the 3 h run on the fake (D048)
- [x] M3.20 Real-account runs: `second` mode and selected M scenarios (user-gated, D030)
- [x] M3.21 Exit check and documentation consistency pass (Opus)

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
