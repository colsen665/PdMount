# TODO

Last updated: 2026-11-18 (**v0.5.0 released:** M5 pushed as one commit `318c015` and tagged; the Release has the archive, `sha256sum -c` OK, `--version` 0.5.0. GitHub's first quick CI failed: Ubuntu's `protobuf-compiler` without recommends lacks `google/protobuf/any.proto`; fixed locally (`libprotobuf-dev`, a `build-doctor` check), not pushed; next M6.0.1). 2026-11-17 (**M5 squashed, decisions consolidated** (D129, user): 103 commits became one `M5:` commit on `master`, not pushed; D091–D128 became 20 entries and 17 `merged into` stubs; next: the release of M5). 2026-11-17 (**D124–D128 accepted** by the user; `worktree-M5` merged into `master`, not pushed; next: the release of M5). 2026-11-16 (**M5.8 done** (exit check, security review, documentation): five area reviews and a verifier, 33 groups; V-1 (High) to V-8 fixed by agents (D124–D128 proposed, D115 and D117 amended); a pre-existing data-loss race found by the proptests fixed (D128); `doctor` lines for further roots; full CI green but svc (a test race, fixed; the live list green on rerun); pjdfstest and fsx on a share green; next: the user reviews D124–D128, then the v0.5.0 release). 2026-11-15 (**X10 through the existing tests** (D123, user)). 2026-11-15 (**Computers out of M5's scope** (D122, user): M5.11 and M5.14b dropped; DESIGN RQ8, §14.5, X8, X9, §16 and Appendix B updated). 2026-11-15 (**Q10–Q12 answered, real-account checks passed:** M5.12, X9 in the editor share, a role change viewer and back, M5.13 Photos and its thumbnails; D120 accepted (Q12); the web app's "can edit" is the server's `admin`; the owner's side of X9 open; next M5.8). 2026-11-15 (**Run paused for three open questions** (Q10–Q12): the final full CI on `e991d2d` passed every step; D111–D119 and D121 accepted by the user, D114 with option (a); D120 waits for Q12; the real-account checks wait for Q10 and Q11). 2026-11-15 (**Review fixes P2 merged** (D120): a gone tree's refresh ends, sync turns wait for users at most 2 s, pin and thumbnail downloads hold no turn, a user's read raises a scan download, scan blocks follow the disk; full CI on P1, R and J green). 2026-11-15 (**Review fixes J merged** (D121): a deleted folder whose child went into a local-only folder is kept, no wake loop, waits apart from attempts, holds cover new entries). 2026-11-15 (**Review fixes R merged** (D118): the stop takes every further mount and is hurried by a second signal, dead root mounts are cleared, gone shares are retired before their export and their cleanup tracked, removed trees answer ENOENT/EIO consistently, `reset --orphan-trees`). 2026-11-15 (**Cross-task review:** three read-only reviewers, 27 findings (3 high), fixed by groups J, P1, R, P2; P1 merged (D119): Photos timeline in PdMount's own types, no early-end deletes, user waits outside the gate, degraded vs absent nodes, one session's clients per call). 2026-11-15 (**Journal cycles fixed** (D117): nodes waiting for each other's places move one aside under `.pdmount-swap-<ino>`; seven cycles reproduced and fixed). 2026-11-15 (**Photos on the session's gate and bulk-read guard** (fix); combined CI on the folder-trash fix, M5.14a and M5.13b green). 2026-11-15 (**M5.13b merged** (D115 proposed): Photos thumbnails in `~/.cache/thumbnails` (normal, large, x-large), verified against the revision's signed manifest, one listing per photo). 2026-11-15 (**M5.14a merged** (D114 proposed): Editor/Admin shares writable, a lost right holds the changes, a revoked share's queued changes go to `orphaned/`, marker per root, `TARGET=share` scenarios, x16). 2026-11-15 (**Folder-trash fix** (D116): a folder's trash waits on its child's entry instead of a doubling back-off; My Files was affected too). 2026-11-15 (**M5.13a merged** (D113 proposed): Photos as a read-only tree in a mount of its own, years and months from the timeline, names per month in batches, a store so a remount fetches nothing again, Live Photos beside their photo; scenario x17). 2026-11-15 (**Transfer limits per session** (fix): download slots, upload permits and the background gate shared by all trees; combined CI on M5.12 and M5.10b green but x12 (a test race, fixed)). 2026-11-15 (**M5.10b merged, M5.10 done** (D111 proposed: scan downloads 100 ms apart, not D027's 500 ms, for the user's review); the fake scenarios use `scan.interval = 10ms`; transfer limits found per tree, fix agent running). 2026-11-15 (**M5.12 merged** (D112 proposed): one tree per share, D107 names, trees that come and go in a live mount, revocation on the listing or the root's first "not found"; scenario x15). 2026-11-15 (**Module split** (user): `engine.rs`, `daemon.rs`, `cache.rs` and the FUSE layer split into modules with module maps, no behaviour change; combined full CI on M5.10a green; M5.10b and M5.12 running). 2026-11-15 (**M5.10a done:** the full CI on its second part passed; its cache part (one profile-wide `cache.max_bytes`, My Files keeps half) merged; combined full CI next, the module split in parallel). 2026-11-15 (**M5.10a's second part:** per-mount status/unmount/`-o ro` with per-mount states, roots mount after readiness and after a keyring wait, outside unmounts noticed, x14; D110 accepted with two amendments; M5.11 skipped for now, M5.14 split, D028 exception for an editor share (user)). 2026-11-15 (**M5.10a, first part** (D110 proposed): trees with their own engines, the router, the write gate, one poller per volume, `[[roots]]` with a test kind, a fake with volumes and roles; full CI green; per-mount status/unmount/`-o ro` drafted, open for a second agent). 2026-11-14 (**Attach race and CLI keyring bus fixed** (D102 amended): an attach before the daemon is saved for its start; commands without a daemon also look for the keyring on the service's bus; the user's check passed; full CI green but one outdated test, updated; next M5.10). 2026-11-14 (**M5.16 done:** `doctor` desktop bus and keyring lines, D101's check on the own bus; fake scenario profiles get their own mountpoint, so a real drive at ~/ProtonDrive no longer breaks `reset` in tests). 2026-11-14 (**Finding 5 fixed** (D109): a failed service start is final on OpenRC and systemd; the CI clean-up no longer unmounts a real ~/ProtonDrive). 2026-11-14 (**Finding 8 fixed** (D106 amended): failed token writes no longer fail a start or latch the keyring, timed-out writes are read back; full CI green). 2026-11-14 (**D108 done:** `login` with a working session says so, `--force` signs in anew; finding 8 investigated, fix approved). 2026-11-14 (**Restart check passed** on Gentoo; findings 8 (the keyring library's 2 s call limit) and 9 (`login` on an online daemon) recorded). 2026-11-13 (**The attach survives a daemon restart** (D102 amended, user; saved in the runtime directory, checked at start)). 2026-11-13 (**M5.17 done:** the third Gentoo check passed: the unlock dialog opens after the attach, online after the unlock, a cancel leaves the mount waiting). 2026-11-13 (**M5.17 Gentoo check, second round:** `Bus:` line right, B2 passed (one start, waited, online after the unlock); the keyring-needed notification never came and the unlock dialog could not open on the OpenRC bus; fixed: the notification counts once sent, `session attach` passes the display to the daemon's own bus (D102 amended, user); the user checks a third time). 2026-11-13 (**M5.17 Gentoo check, first round:** the keyring sat on the service's bus and i3 runs no XDG autostart; fixed by a per-service bus choice and an i3/sway hint (D102 amended, user); flaky tests fixed; full CI green at `eba5175`; the user re-checks). 2026-11-13 (**M5.9 done** (watch phase with the user; D107: share polling 10 min/30 s, owner suffix, 512 px WebP thumbnails); **M5.17 merged** with its review fixes, full CI green but one flaky unit test (fix agent running); D102 bullet, D106 accepted). 2026-11-13 (**Findings 1–4 and 7 fixed** by agents, full CI passed at `5ecb4ea` after one m7 rerun; schema 4 (D105); D104 and D105 accepted; next M5.17). 2026-11-13 (**`~/.xprofile` bus fix withdrawn:** it broke USB mounting in Thunar; D102 accepted, M5.17 planned: the desktop hands its bus to the service). 2026-11-13 (**M5.6 done:** Gentoo results in `docs/plans/m5.md`, Debian accepted by the user; next the findings 1–7). 2026-11-12 (**M5.6 in progress:** `scripts/repro/m56.sh`; Gentoo found a desktop session bus separate from the service's; D100, D101 proposed, M5.16 planned). 2026-11-12 (**D099:** SDK rebase and official-SDK parity check planned before M6, M6.0.1–M6.0.3). 2026-11-12 (**D098:** `ci.sh` runs every step, one locked scenario root per checkout; `docs/multi-task-runs.md`). 2026-11-12 (**M5.15 planned:** cargo-dist release archive and GitHub CI, D097 accepted). 2026-11-12 (**M5.1–M5.5 and M5.7 done** by agents, merged one commit per task, review fixes after a cross-task review; D092–D096 proposed; next M5.6 by the user). 2026-11-12 (**M5.0 done:** probe results on Gentoo and Debian in `docs/plans/m5.md`; D091 accepted: the shutdown flush also starts at logind's PrepareForShutdown, under a delay lock, with a notification; amends D075). 2026-11-09 (**M5 started:** M5.0 probe script and checklist). 2026-11-01 (**On GitHub, D089, D090**: public MIT repo and real SDK fork, one commit per milestone. **M4 done**: M4.16 exit check passed at `afb99df`, not pushed; M4.8 done; D064 decided by the user: writeback cache stays off; M5–M7 planned, D073–D083, merged with M4.8; `kernel.writeback_cache` removed, D083). M3 done (M3.21: every §16 criterion passes; results in its entry in `docs/handoff-log.md`). Pre-M4
review done; fix groups G1–G5, G7 and G8 applied, G6 in part (the rest is optional, user's call). **M4 started:**
plan in `docs/plans/m4.md` (D056), M4.0 done, tag `m4-start`. M4.0.1 security review done (1 High, 5 Medium).
Fixes M4.0.2.1–M4.0.2.4 done (the High and every Medium). M4.1 done (latency histograms, `stats --json/--reset`,
Prometheus textfile). M4.2 done (fake network profile, E19). M4.3 done (benchmark harness, D057). M4.4 done (staging
descriptor LRU, D063). M4.7 done (fork P18, 4 files / 5 blocks, D058). M4.5 done (baseline, triage; untar criterion
≤ 15× local, D065; fsync SLO p99 < 30 ms, D066). M4.6.1–M4.6.4 done (store writes, no sidecar for new files D067,
durable relist queue D069, stable git row, Proton block buffer). M4.8 done (writeback cache off D064, option since removed D083, no
KILLPRIV_V2 D068, directory pages kept D072). M4.9 done (cache protections, ordered eviction, orphan sweep, `cache stats/gc/wipe`). M4.10 done (pins, D060). M4.11 done (queue tooling, `sync`, D061). M4.12 done (shutdown grace D062, `mount -o ro`, `fsync.remote_paths`, log rotation, status). M4.13 done (autostart). Quick CI is the default (D084). M4.14 done (user guide). M4.15 done (real-account run: no 429, Q7). Passphrase eviction fixed (D085). Next: M4.16.

## Next action

- **v0.5.0 released (2026-11-18):** M5 is one commit on GitHub (`318c015`, tag `v0.5.0`); D091–D128 consolidated into
  20 entries and 17 stubs (D129). GitHub's quick CI failed on its first run (no `any.proto` on Ubuntu without
  `libprotobuf-dev`); the fix is a local commit, to be pushed with the user's agreement (it goes green only then).
  **Next: M6.0.1**, the SDK fork rebase onto upstream `v0.5.1` (D099, `docs/plans/m6.md`).
- **Publication is done (D089, D090):** the maintainer was told, then the SDK fork and PdMount were pushed.
- **M5.6 done:** Gentoo parts A and B with `scripts/repro/m56.sh`, results in `docs/plans/m5.md` ("Results: Gentoo"; the
  missing `-v` was fixed during the run, B2 gave findings 5–7). Debian accepted by the user (2026-11-13), no step details
  recorded.
- **Session bus split on Gentoo (2026-11-13):** the `~/.xprofile` snippet that moved the desktop onto the OpenRC bus broke
  USB mounting (Thunar: "not permitted"); the user reverted it, so the Gentoo service again reaches neither keyring nor
  notifications. The user guide now warns against it; workaround `--allow-file-secrets`. Fix: **M5.17** (D102, accepted,
  approach chosen by the user): `session attach` at login hands the desktop's bus to the daemon; includes finding 6.
- **Findings 1–4 and 7 fixed (2026-11-13),** one commit each on `worktree-M5`; full CI passed at `5ecb4ea` (m7 failed
  once on the visibility SLO, passed on rerun; Known gaps). Store schema 4 (`queued`, D105) for finding 3; the fake now
  starts offline from the stored account (E2 tests it); `status` does not prompt for a locked keyring (D104).
- **M5.17 done (2026-11-13):** `d0e3951`, review fixes `55c2575`, and after the user's first Gentoo check `eba5175`:
  the keyring and notifications each use the bus their service runs on (the PAM-unlocked gnome-keyring was on the
  service's bus, the notification server on the desktop's), and `service install` prints the `exec … session attach`
  line for i3 and sway, which run no XDG autostart (D102 amended, approved by the user). Full CI green at `eba5175`.
  Second Gentoo check: the `Bus:` line and B2 passed; then (D102 amended again, user) the keyring-needed notification
  counts only once `notify-send` succeeded, and `session attach` passes `DISPLAY`/`XAUTHORITY`/`WAYLAND_DISPLAY` to the
  daemon's own bus (`UpdateActivationEnvironment`) so gnome-keyring's unlock dialog opens there. Full CI: every
  step passed except x13, a `grep -q` broken-pipe race in the script (fixed; x13 passed twice alone). **Third Gentoo check passed (2026-11-13):** the attach sets the
  display on the service's bus, the unlock dialog appears, an unlock brings the mount online, a cancel leaves it waiting;
  M5.17 done. After it (D102 amended, user): the attach survives a restart: saved in
  `$XDG_RUNTIME_DIR/pdmount/<profile>.attach` and checked again at the daemon's start. **For the user:** reinstall,
  `session attach`, `rc-service --user pdmount.default restart`, then `status` still shows the `Bus:` line. Full CI
  green. The user's check passed (2026-11-14): the restarted daemon took the saved bus and display without an attach.
- **M5.9 done (2026-11-13):** outcome in `docs/plans/m5.md`; D107 (user) amends D080/D081. Question 3 waits for a device
  (M5.11 blocked); question 6's desktop part (Thunar and tumbler on a fake mount) is optional before M5.13.
- **Finding 8 fixed (2026-11-14, D106 amended, user):** a start writes the session only when the tokens changed; a
  failed write after a verified session keeps the daemon online with the tokens unsaved (`status` `Tokens:` line,
  saved later after a no-prompt probe) and no longer latches the keyring; a write that timed out (`NoReply`) is read
  back and retried once (`keyring-d024.sh` case `w1`). Full CI green with D108. The user's check passed: after a
  restart online with the saved bus, no `Tokens:` line, no write warnings; `login` said "already signed in".
- **M5.16 done (2026-11-14):** `doctor` has a `desktop bus` line (same / attached / not attached, by bus ID) and a keyring
  line that tells running, locked, started on demand, none and failed to start apart; before keyring calls on the own bus,
  D101's ownership check says "no Secret Service" at once or "D-Bus is starting the Secret Service" (D100, D101 amended
  as built, user). Full CI: all steps passed except x13 (its own `fake_config` call lacked the new mountpoint argument;
  fixed, x13 passed twice). The user's check found two gaps, fixed (D102 amended, user): `session attach` with no daemon
  saves the attach for the next start (the daemon also re-reads it once its socket listens), and commands without a
  daemon choose the keyring's bus between their own and the service's (`$XDG_RUNTIME_DIR/bus`). The user's re-check
  after a new login passed (the start took the saved attach; `doctor` keyring, login and desktop bus ok); at that login
  gnome-keyring ran on the desktop's bus, so the fallback to the service's bus was covered by tests only (`keyring-d024.sh`
  o4). Full CI: every scenario step passed; the test step failed on one integration test that expected the old
  no-daemon output (updated; quick CI green).
- **M5.10a done but its cache part (2026-11-15, agents; D110 accepted):** the tree shape without the bulk-read guard. Built: a tree is a
  backend value (`ProtonBackend::tree`, `FakeBackend::tree`) with its own engine and state under `trees/<key>/`, shared
  connectivity, a write gate per tree (closed for further trees), a router for mounts with several trees, one poller per
  volume (`pdmount_core::poller`, D107 intervals, `DriveShareRefresh` hook), `[[roots]]` (only `kind = "test"`, test
  builds), the fake's volumes, shares and roles (`pdmount fake root`), `reset` covering `trees/`. Unit tests (fake,
  poller, gate, router, config, paths) and the mounted test `a_mount_with_several_trees_serves_each_from_its_own_engine`
  pass; quick CI, the fake scenarios `r1 g1 ops svc x12` and the full CI green (2026-11-15).
  **Second part (2026-11-15, agent):** per-mount `status`, `unmount <mountpoint>` and `mount [-o ro] <mountpoint>`
  finished and tested (scenario X14, unit tests in `roots.rs`): per-mount states behind a short lock, so `status`
  never waits and a second command on a mount is refused; the roots mount in the background after the readiness
  report; an unmount from outside is noticed within a second; after a keyring wait the roots mount by themselves
  (D110 amended, checked in X13). D110's draft remark is resolved (D110 accepted and updated; DESIGN §14.5).
  Quick CI and the fake scenarios `x14 svc x12 r1 x13` green; the full CI is the main session's. Open: the cache
  part (one profile-wide `cache.max_bytes`, D110 amended), for another agent.
  After it (user, 2026-11-14): a split of `engine.rs`, `daemon.rs` and the FUSE router into smaller modules, no change
  in behaviour, its own commit; then M5.10b with a code map in its brief.
- **Next: the release of M5** (AGENTS.md, "Release"; D124–D128 accepted by the user, 2026-11-17; M5 merged into
  `master`, not pushed): the final full CI passed on `e0a9b03` (every step, 2026-11-16); `dist plan`, tag `v0.5.0-rc.1`, push with the user's agreement after the
  GitHub settings below. M5.8 done (2026-11-16): review reports and verification in `docs/security/2026-11-15-review/`;
  unfixed Lows and Infos in Known gaps. Open from the real-account checks: the owner's side of X9 (content and signer
  of `pdmount-test/x9` in the editor test share). Nothing is pushed. The open findings:
  9. Done (D108, user): `pdmount login` with a working session says "already signed in" and exits 0; `login --force`
     signs in anew; a requested sign-in is announced as "sign-in link ready".
  5. Fixed by D109 (user, 2026-11-14): a start that fails before readiness is final. The OpenRC script's `start()`
     stops the supervisor after a failed `default_start` (a stray supervisor from before is left, and `status` and
     `doctor` name the command that stops it); under `--service` a failure before readiness exits 78 and the systemd
     unit has `RestartPreventExitStatus=78`; `notify_fd_ready` writes only to a pipe. Sandbox OpenRC test in
     `tests/service.rs`. The user's check passed on Gentoo (2026-11-14): a broken `default.toml` gave "failed to
     start" with no supervisor left; after the fix the start went online.
- **Decisions:** D092–D096 and D100–D107 accepted by the user (2026-11-13), D102's own-user bus bullet confirmed; D110 (M5.10a) accepted by the user (2026-11-15) with two amendments (one profile-wide `cache.max_bytes`, roots mount after a keyring wait); D111–D119 and D121 accepted by the user (2026-11-15; D114 with option (a)), D120 accepted by the user (Q12, 2026-11-15); D124–D128 accepted by the user (2026-11-17); D026's status line (SIGHUP part
  superseded by D075/D092) confirmed.
- **M5.15 done, GitHub side unchecked:** before the first tag the user enables Actions, allows the release job
  `contents: write` (Settings → Actions → General), and preferably restricts who may push `v*` tags; then push `master`,
  check that `ci.yml` passes, push `v0.5.0-rc.1` and check its Release (`sha256sum -c`, `--version`, only `/home/runner`
  paths in the binary).
- **Computers out of M5's scope (D122, user, 2026-11-15):** M5.11 and M5.14b are dropped from M5 and wait, with no
  milestone, for a device on the test account; their specification stays in `docs/plans/m5.md`. **Test share (user, 2026-11-15):** the user shared a folder from their own account
  with the test account as **editor**; its name is `PDMOUNT_WRITE_SHARE` in `.pdmount-local.env` (AGENTS.md, "Local
  paths"). Reads of it need no further asking; writes only under `pdmount-test/` inside it (D028 amended). M5.8 is last.
  Before each push, run the full CI (D084).
- **Before M6 (D099):** M6.0.1 rebases the fork onto upstream `v0.5.1`, M6.0.2 checks PdMount and the fork against the newest official SDK tag, M6.0.3 is a security review with Fable reviewers, M6.0.4 fixes its High/Critical findings and the parity findings the user picks (`docs/plans/m6.md`). Upstream PRs still wait for the user (D089).
- **M4 is done and on `master`.** Decisions D057, D062, D068, D070, D071, D072, D085, D087 accepted, D088 (no passphrase move); the full CI passed on `bc03e80`. Sandbox profiles `default` and `second` were cleared (D088); `default` is logged in again, `second` needs a login from the user before the Q1/Q6 probe. Then M5 (`docs/plans/m5.md`). Before relying on the latency numbers on a spinning disk, run `m4.sh` with `BENCH_ROOT` there (Known gaps).
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

- [x] M5.0 Probe of logout and shutdown on Gentoo and Debian (D091)
- [x] [full] M5.1 Flush on SIGTERM and at PrepareForShutdown, delay lock, notification (D075, D091, D092)
- [x] M5.2 Warning at start when a flush did not finish (D095)
- [x] [full] M5.3 Daemon under a supervisor: readiness, stale mount, `unmount` (D093)
- [x] M5.4 `pdmount service`, OpenRC (D074, D094)
- [x] M5.5 `pdmount service`, systemd (D094)
- [x] M5.6 Manual runs on Gentoo and Debian (user)
- [x] [full] M5.7 Pending marker (D076, D096)
- [x] M5.9 Probe of the further roots, read-only on the real account (D107; question 3 waits for a device, question 6's desktop part not run)
- [x] [full] M5.10 Several roots in one daemon (D079, D080, D110), bulk-read guard (D081, D111)
  - [x] M5.10a Tree shape: engines per tree, router, write gate, pollers, `[[roots]]`, fake volumes; per-mount
    status/unmount/`-o ro` (X14); roots mount after a keyring wait (D110 amended, X13)
  - [x] M5.10a cache part: profile-wide `cache.max_bytes` across trees (D110 amended)
  - [x] M5.10b Bulk-read guard (D081, D111)
- ~~M5.11 Computers, read-only~~ out of M5's scope (D122)
- [x] [full] M5.12 Shared-with-me, read-only (D112)
- [x] [full] M5.13 Photos, read-only, folders in stages and stored thumbnails (D081)
  - [x] M5.13a Photos tree, folders in stages, reads (D113)
  - [x] M5.13b Stored thumbnails (D081, D107, D115)
- [x] [full] M5.14a Read-write for shares where the role allows (D080; real-account check in the editor test share, D028 amended)
- ~~M5.14b Read-write for Computers~~ out of M5's scope (D122)
- [x] M5.15 Release archive with cargo-dist, GitHub CI (D097)
- [x] [full] M5.17 The desktop hands its session bus to the service; a daemon without a keyring waits (D102, finding 6)
- [x] M5.16 Session bus check in `doctor`, Secret Service ownership before keyring calls (D100, D101)
- [x] [full] M5.8 Exit check, security review, documentation (last; D124–D128)

## M6 tasks

Specifications: `docs/plans/m6.md`.

- [ ] [full] M6.0.1 SDK fork rebase onto upstream `v0.5.1` (D099)
- [ ] M6.0.2 Parity check against the official SDK (D099)
- [ ] M6.0.3 Security review of M5 so far, the release pipeline and the rebased fork (Opus; reviewers on Fable)
- [ ] M6.0.4 Fixes: High/Critical review findings, and the parity findings the user picks
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
| Q10 | ~~Real-account checks of M5.12 and X9 (M5.14a): how to run them?~~ **Resolved (user, 2026-11-15): (A), the agent ran them.** Passed: the listing, names, roles, poll intervals, reads, X9's rsync and git with a fresh remount, a role change viewer and back. The web app's "can edit" is `admin` (22) on the server. Not checked on the real account: a refusal while the listing allows writing, a revocation with queued changes, `Degraded` names, an empty page after a deleted anchor. Results: `docs/plans/m5.md`, "Results: real account" | — | M5 |
| Q11 | ~~May the agent read the test account's Photos?~~ **Resolved (user, 2026-11-15): yes.** M5.13's check passed (timeline, local-time folders, reads, thumbnails in three sizes, remount from the store) | — | M5 |
| Q12 | ~~D120: may a pin download during the user's activity?~~ **Resolved (user, 2026-11-15): yes**, D120 accepted | — | M5 |

## Handoff notes

<!-- Rewrite at the end of every session: current state and standing notes only, kept short. The session's full
     entry goes at the top of docs/handoff-log.md. -->

- **M5 published (2026-11-18):** commit hashes of M5 cited in the documents (for example `e991d2d`, `eba5175`) resolve
  only on the local branch `backup/m5-before-squash`, not on `master` or GitHub (D090, D129). GitHub job logs need an
  admin login: the user pastes them (no `gh` or token here). OpenRC 0.64 needs `-U` for a direct `supervise-daemon`.
- **M5.8 (2026-11-16):** fixes merged on `worktree-M5` from agent worktrees (removed). Mounts now carry
  `fsname=pdmount:<profile>` (D127); a share's `.pdignore` does not apply to members (D124); share names are kept in
  `shared-names.json` (D125). The real `~/ProtonDrive` mount of the sandbox user runs the `e991d2d` release build
  (`target/mountbin/pdmount`), which predates these.
- **Real-account checks (2026-11-15):** `~/.config/pdmount/default.toml` of the sandbox user was written for them
  (`shared` and `photos` roots) and moved aside afterwards to `default.toml.m5-checks`, so profile `default` mounts My
  Files only again; the test data stays under the editor share's `pdmount-test/x9` for the user's owner-side check.
- **M4 exit result (2026-11-01, `afb99df`, on `worktree-M4`, not pushed):**
  - Passed on this code: `scripts/ci.sh`; `run-all.sh` on the fake in `live remount crash fresh`; pjdfstest; 10 000-case proptests. git 1.50, untar 11.36, read 1048 MB/s, gc 0.90 × cap. slo 13 of 13: `setattr` p99 0.75 ms (limit 2), `fsync` 32.7 ms (limit 40). Report: `target/bench/m4-2026-11-01/report.md`.
  - **The slo row often cannot get a verdict:** it needs 5 quiet rounds of 15. On 2026-11-01 7 of 8 attempts with the temporary build, the full run, and the first of two reruns at `afb99df` stopped at 3 or 4 quiet rounds. That gives no verdict on the limits; a rerun at a quiet moment can pass. Where the noise comes from is not known: it comes in bursts, sometimes after about 8 quiet probes in a run, so the benchmark's own deletes are a suspect, and btrfs has `discard=async`; discards were seen in only 3 % of 2 s samples, so this is unproven.
  - D087 closed the setattr miss: a WAL fsync and a staging fsync take turns. It costs `fsync` tail margin (Known gaps). The 10 000-case proptests also found an old bug, a zero-length write past the end set the size (M4.16.7); the kernel does not send such writes.
  - Security review: no High or Critical. No race test exists for C-3 and C-4 (fixed by reading the code). Remaining findings: Known gaps.
- **Findings after M5.6 (2026-11-13):** 1–4 and 7 fixed on `worktree-M5` (not pushed); 5 and 6 open (6 is part of
  M5.17). Store schema 4 adds `queued` (D105): the journal writes a row per entry under its lock (a store write is
  queued without waiting; DESIGN §4). A flaky unit test was seen once by an agent:
  `daemon::tests::unmount_under_a_service_refuses_and_force_still_works` (`running_service` returned `None`, possibly
  before the stand-in process exec'd); passed on rerun and in the full CI.
- **Current state (2026-11-12):** M4 is done and pushed. On `worktree-M5`, not pushed: M5.0–M5.5, M5.7 and two
  review-fix commits. The daemon flushes at SIGTERM/SIGINT/SIGHUP and at logind's PrepareForShutdown under a delay lock
  (`flush.rs`, `logind.rs`, zbus 5.19), warns at the next mount (`resume.rs`), writes an opt-in pending marker
  (`engine/marker.rs`), runs under a supervisor (`supervise.rs`) and installs itself as a service (`service.rs`).
  Scenarios x1–x4, x11, x12, svc in the CI's live list. M5 works as M4 did: one commit per task, agents per the plan's
  Model column (user, 2026-11-12). Per-session details: `docs/handoff-log.md`.
- **Gentoo desktop bus (2026-11-13):** the desktop runs its own bus (SDDM, `dbus-launch`); never move it onto the OpenRC
  user bus (`~/.xprofile`): that broke udisks mounting in Thunar. The service reaches the keyring and notifications only
  after M5.17 (D102); until then `--allow-file-secrets` there.
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

- **M5.8 security review, not fixed (2026-11-15; details, file:line and fixes in
  `docs/security/2026-11-15-review/verification.md`).** Fixed: V-1 to V-8 (D124–D128, D115 and D117 amended). Open,
  Low: V-9 share folders and block requests signed with the default address, not the share's membership address (SDK
  fork); V-10 share names and owners cleaned of control characters only (bidi, zero-width), `%` log fields raw; V-11
  a degraded share root reported gone by `poll_events` (plausible); V-12 each photo insert renames its whole month
  (quadratic); V-13 thumbnail cache unbounded, survives `logout`/`reset`, keeps an existing folder's mode; V-14 `login
  --force` does not end the old session on the server; V-15 the sign-in link in `xdg-open`'s argv (pre-M5); V-16 forged
  unicast `PrepareForShutdown` can stall the logind task (the flush then starts at SIGTERM); V-17 stop signals between
  the mount and the signal handlers kill the daemon without a flush; V-18 swap names unique per device only; V-19 a
  kept folder can come back under its swap name; V-20 about 20 MiB per further tree (640 MiB at 32 shares). Info:
  V-21 to V-33 (overflow of a huge `Retry-After`, export modes, listing before the share cap, tree slots never reused,
  a share revoked while unmounted, thumbnail hardening, peer-uid check age, unescaped wait reason, OpenRC `eval`
  quoting, the readiness pipe inherited, unit file modes, `reset` of an unreadable tree's state). Also: the engine's
  long-running tasks other than the marker reader and the Photos stages are not restarted after a panic (C5-5).
- **D125 and D128 limits (M5.8).** A share first shown under a name like `Team (alice@…)` keeps it, and Alice's later
  share gets the number (user's call whether to always suffix such names). A file brought back by D128 whose removed
  folders were already dropped from memory comes back in the root, not at its old path.
- **Flaky under load (M5.8, 2026-11-15/16).** With three fix agents building and testing in parallel: `queue`
  ("no base revision"), `ops` ("the upload was not slow"), `svc` ("no datagram") in the full CI on `976ea61`, all
  passed on rerun; unit tests `cache::tests::the_sweep_removes_orphan_files_and_dangling_rows` (2 s grace),
  `engine::tests::a_read_over_more_blocks_than_pool_slots_holds_few`, `fake::tests::faults_fire_before_the_round_trip`,
  and the proptest `random_changes_converge_to_the_model` ("engine still referenced after the kill") failed once each
  and passed on rerun. Watch for recurrences without load.
- **A name held by a node in a local-only folder (D121):** a node waiting for that name never gets it (no loop: a check
  every 5 s, no API calls); older than D116.
- **Shares (M5.14a, D114):** path commands (`access`, `conflicts`, `queue`, `pin`, `sync`), `stats` and the hot-path
  check reach My Files only; a Shared mount left stale by a crash needs `unmount --force <its mountpoint>` (My Files'
  `unmount --force` does not clear it); at a stop My Files' flush runs first, so a large queue can leave the shares no time.
  D076 in shares: option (a) chosen by the user (D114); since D126 every editor's PdMount reads a share's markers (at
  most 16 per pass, one notification per tree), so a member's marker is not limited to their own devices.
- **Thumbnails (M5.13b, D115):** the manifest check costs one listing per photo (about 2 photos/s, more than the
  official clients, which do not verify thumbnails); an SDK fork patch keeping the thumbnails' digests in `BlockMap`
  would avoid it (needs a push). URIs use the mountpoint as configured. Decoders reviewed in M5.8 (V-5 fixed: frame sizes checked first).
- **Photos and the session (M5.13a):** fixed: the session's gate, download slots and bulk-read guard. With the shared
  gate a Photos `ls` waiting for a month's names competes with My Files' user requests and could reach its metadata
  deadline under heavy load (a bounded-patience turn would help; for the review). No bound on the photos held in memory (about
  0.5 KB each); the threat model does not cover Photos metadata yet (M5.8). The SDK's `PhotoTag` is a closed enum, so an
  unknown tag would fail a whole timeline page: check on the real account.
- **Scans are per session, not per process (M5.10b, D111):** a photo opened while a thumbnailer runs waits its turn for
  its first 8 MiB; M5.13b's stored thumbnails should remove most of those reads.
- **Profile cache cap (M5.10a, D110):** an unmounted tree's blocks stay on disk uncounted until it mounts again; `cache
  gc` reaches My Files only; above 90 % a tree's write may wait on the profile-wide eviction lock while another evicts.
- **Double polling after a keyring wait (M5.10a, D110):** a daemon that waited for the keyring keeps My Files' own event
  task, so a further tree on My Files' volume has that volume polled twice until the next start. Fix: let My Files join
  a poller after its own task started.

- **Security (M4.16; R-n in `docs/security/2026-10-31-review/verification.md`).** Medium: R-1 Prometheus temp file follows a symlink planted in a shared-writable directory (`stats.rs`); R-9 the pin reserve is checked per file, so one large remote file can fill the disk with pinned blocks (`pins.rs`). Low: R-2 autostart for a profile never logged in prints the sign-in link to the session log; R-3 passphrases of a refused login stay in `sdk-keys.db`; R-12 unbounded `entry_path` recursion (plausible); R-14 `needs_login` by message match; R-15 unverified block listing refetched per read. Info: R-4–R-6, R-8, R-16. Fixed in M4.16.1–M4.16.5: R-7, R-10, R-11, R-13.
- **Code review, M4.16.** C-1 to C-5 fixed in M4.16.1–M4.16.5. As reported, not checked: C-6 pin nudges can starve fetching (`pins.rs:509`); C-7 `fetch_pinned` never writes a RAM-only block to disk (`pins.rs:560`); C-8 a failed relist is not retried and leaks its count (`journal.rs:834`); C-9 eviction scan on every `put` when protected blocks fill the cap (`cache/disk.rs:299`, `DiskCache::put`); C-10 `pin_summary` costs a store read per pinned file on each `status` (`pins.rs:390`).
- **Proptest regression case failed once (2026-11-12):** `engine::proptests::concurrent_regression_cases` failed in a quick CI at load about 12 ("names of core B against the fake": B had `[]`, the fake `["/a"]`, `proptests.rs:1029`), then passed 5 reruns at the same load. A convergence failure may be a real race (possibly from M5.1's back-off or barrier changes), not test timing: investigate before M6 if it recurs.
- **M5.3 test failed once (2026-11-12):** `daemon::client::tests::unmount_under_a_service_refuses_and_force_still_works` (`daemon/client.rs:268`, `running_service` gave `None` for the stand-in daemon) in a quick CI at load about 6; passed 10 reruns. Not investigated.
- **Under OpenRC the daemon's log is not rotated (M5.4, D094):** `supervise-daemon` writes its output to the profile's log, and `mount --foreground` does not rotate. An external `fusermount3 -u` makes the daemon exit 0, which OpenRC restarts (D093).
- **Latency samples land after the reply (2026-11-12).** A request's sample is recorded when its stopwatch drops, after the reply, so `stats` read right after a request can miss it. Two mounted tests (`mount_lists_unchanged_directories_from_the_kernel_cache`, `mount_times_requests`) failed under load for this reason and now wait for the samples. Separately, a second `opendir` can arrive before the first handle's `releasedir`, which costs a cache hit (D072's conservative case). The cache unit test `the_sweep_removes_orphan_files_and_dangling_rows` failed once under parallel load and passed 5 reruns; not investigated.
- **M4 numbers are for an SSD (2026-10-31).** Nothing is measured on a spinning disk. `BENCH_ROOT` chooses where the benchmarks run.
- **D087 costs fsync margin (2026-11-01).** Alternating rounds: `fsync` p99 32.1 → 37.5 ms against a 40 ms limit; rounds above 60 ms 4 of 11 against 1 of 9, too few to tell from noise. The `TRUNCATE` checkpoint on the writer still stalls replies about 40 ms when the WAL passes 64 MiB. Why btrfs stalls a commit while a WAL fsync overlaps another file's fsync is not established, and the gate is measured on this SSD only.
- M3 (accepted risks, DESIGN.md §17): a version restore inside our draft window is overwritten (D043); a revision
  written between the local-delete pre-check and the trash goes to the trash; a crash before the group commit loses a
  conflict record (not the copy); edit/edit above 16 MiB without a claimed SHA-1 always makes a copy (D040).
- M3: a slow pass on the receiving device (paced listings, D027) can exceed the 40 s visibility SLO (D050); not seen in
  M3.20. Seen again 2026-11-13 in the full CI's `scenarios, live` (m7, marker b2a > 40 s; the others 26–29 s); m7
  passed alone on rerun (27–28 s).
- Scenario state survives between full CI runs in one checkout (`/dev/shm/pdmount-ci-<n>` keeps the profiles): a run that
  fails halfway leaves open conflicts, and the next run's m7 then counts them ("create_create create_create
  remote_delete_local_edit", 2026-11-13). Remove the root before a rerun; m7 should count only conflicts from its own run.
- Flaky unit tests (2026-11-13): fixed the exec race of `unmount_under_a_service_refuses_and_force_still_works`
  (`fake_daemon` waits for exec), the "Text file busy" race of the stub scripts (`test_support::write_executable`), and a
  broken pipe that hid a helper's missing answer (`run_helper`). 30 runs under a fork storm passed. One `scripts/ci.sh
  --quick` failed in its test step once afterwards with the log not kept; two reruns passed. Keep the log next time.
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
  - Low S-14: `unmount --force` acts on any mount at the path, not only a pdmount one (`daemon/client.rs:158-205`, `force_unmount`).
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
