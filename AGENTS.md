# AGENTS.md

Instructions for any agent (Claude Code or other) working in this repository.

## What this is

PdMount is a Linux FUSE filesystem that mounts Proton Drive "My Files" read-write. It is written in Rust using fuser 0.18
and builds on the community SDK (`proton-drive-sdk` / `proton-sdk-rs2` 0.4) through a patched fork.

It exists because the user verified that the write sync in pdcli's mount breaks `rsync` and `git`
(details: `docs/pdcli-evaluation.md`). The requirements are D001–D010 in `docs/decisions.md`.

## Status

Status, the next action, open questions and handoff notes are in **`TODO.md`**.

## Session start

1. Read `TODO.md`: its Next action and Handoff notes sections.
2. Read the index table in `docs/decisions.md`.
3. Read only the `DESIGN.md` sections the task needs (map below).

## Document map

| File | Use it for |
|---|---|
| `TODO.md` | Status, next action, milestone and task checklist, open questions (Q*), handoff notes, known gaps |
| `docs/handoff-log.md` | Full handoff history, one entry per session, newest first; M0–M3 checklists. Not read at session start |
| `docs/decisions.md` | Decision log (D*): what was decided, by whom, and why |
| `DESIGN.md` | The specification |
| `docs/user-guide.md` | The user guide: task-oriented, points to `DESIGN.md` for reference |
| `docs/sdk-patches.md` | SDK fork patches P1–P13: targets, fixes, upstream tracking |
| `docs/pdcli-evaluation.md` | Why the project is not a pdcli fork; what is reused from it |
| `docs/plans/m3.md` | M3 task specifications (D039); work one task per session, status stays in `TODO.md` |
| `docs/plans/m4.md` | M4 task specifications (D056); one task or a recommended batch per session, status stays in `TODO.md` |
| `docs/plans/m5.md`, `docs/plans/m6.md` | M5 (D074–D076, D078–D081) and M6 (D077) task specifications; status stays in `TODO.md` |
| `docs/security/` | Threat model (D070) and security review reports (`<date>-review/`: area reports, `verification.md`) |

`DESIGN.md` sections:

| § | Topic | § | Topic |
|---|---|---|---|
| 1 | Requirements RQ1–RQ8, non-goals, principles | 10 | Multi-device conflicts: detection, policy, names, git guidance |
| 2 | Prior attempts, SDK findings F1–F13, environment | 11 | Per-path access control (ro/rw), CLI |
| 3 | Workspace crates, `Backend` trait, data flows | 12 | Auth: web login, link presentation, tokens, secrets |
| 4 | Concurrency (fuser facts, hot/slow path, locking) | 13 | Storage paths, SQLite schema |
| 5 | Low latency: caching, fsync modes, SLOs | 14 | CLI commands, control socket, unmount safety |
| 6 | Read path: block map, cache, integrity | 15 | Tests: layers, rsync/git semantics, scenarios R/G/N/A/M |
| 7 | Write path: staging, upload job, atomic-save rename, `.pdignore` | 16 | Milestones M0–M7 and exit criteria |
| 8 | Namespace ops: journal, rename flags, setattr, unsupported ops | 17 | Risks |
| 9 | Sync engine: events, refresh, invalidation, offline | App. A/B | errno mapping, config reference |

## Rules

- **Decisions:** entries with `accepted` / `user` in `docs/decisions.md` are fixed; ask the user before changing one.
  Entries with `proposed` / `claude` are open for review.
- **Keeping documents in sync:** every design change updates `DESIGN.md`, `docs/decisions.md` (new or superseding entry) and
  `TODO.md` in the same commit.
- **End of every session:** update `TODO.md`: the "Last updated" line, the Next action, the checkboxes, and a rewrite of Handoff notes
  (current state and standing notes only, kept short). Add the session's full entry at the top of `docs/handoff-log.md`.
- **Single source of truth:** each fact lives in one place. Specifications go in `DESIGN.md`, reasons in `docs/decisions.md`, status in
  `TODO.md`. This file and `README.md` only point to them.
- **Conventions (accepted):**
  - CLI: path last (D009).
  - Milestone exit criteria are in DESIGN.md §16, built from the rsync/git scenarios in §15.3 (D005).
  - Denied writes return EROFS (D019).
  - Secrets go to files only with `--allow-file-secrets` or `--force-file-secrets` (D029).
  - Permission checks in the daemon instead of `default_permissions` (D031).
- **Secrets:** never print the contents of `~/.config/pdcli`, keyrings, tokens or session files.

## Source citations

Design docs cite `prefix/file:line` against local sources:

| Prefix | Path | Version |
|---|---|---|
| `fuser/` | `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/fuser-0.18.0/src/` | 0.18.0 |
| `sdk/` | `…/proton-drive-sdk-0.4.0/src/` | 0.4.0 |
| `rs2/` | `…/proton-sdk-rs2-0.4.0/src/` | 0.4.0 |
| `pdcli/` | `…/pdcli-0.4.0/src/` | 0.4.0 |
| `cs/` | `<official SDK checkout>/` (official Proton Drive SDK, C# and JS) | tag `js/v0.21.0` = `e6a661f5` (cs 0.24) |

Also used: `proton-crypto` 0.12.1. A cargo registry cleanup deletes these sources. To restore them, create a scratch crate under the
scratchpad directory that depends on the exact versions and run `cargo fetch`. To restore `cs/`: `git clone https://github.com/ProtonDriveApps/sdk <dir> && git -C <dir> checkout js/v0.21.0`.

## Git

- Remote: `origin` = a local bare repo, branch `master`, until the user says otherwise.
- **No GitHub use** (repos, forks, PRs, issues, Actions) until the user says otherwise (D023).
- SDK fork: `<fork>` (local bare repo, D022). See docs/sdk-patches.md, "Fork repository".
- Commit or push only when the user asks. Exception during M3 (D039) and M4 (D056): commit after each task of
  `docs/plans/m3.md` / `docs/plans/m4.md` once CI passes (next bullet), one commit per task, message `M3.n: <title>` /
  `M4.n: <title>`; never push. In M4 a session may do several tasks (the plan's "Sessions" table), each its own commit.
- **Which CI (D084):** `scripts/ci.sh --quick` before a commit by default, plus any scenario the task adds or changes. The full run for
  tasks marked `[full]` in `TODO.md`, or any that touch the journal, engine, tree, staging, store, cache, events, FUSE layer, backend
  adapters, the daemon's start or stop, or the scenario harness. Nothing goes to `master` until a full run has passed on what is pushed.
- Author: the repository's local git config (`user.name=Claus`, `user.email=colsen665@proton.me`), set for the sandbox user.
- Commit message trailer: the `Co-Authored-By` line of the model that made the change, e.g.
  `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.

## Environment (verified 2026-10-19)

| Item | State |
|---|---|
| System | Gentoo |
| `/home` | btrfs (FICLONE reflinks available) |
| `/etc/fuse.conf` | no `user_allow_other`, so no AutoUnmount (D020) |
| Toolchain | stable Rust (cargo), Go, clang, protoc, pkg-config, fusermount3 |
| Tools | git, rsync; pjdfstest and fsx built from source (locations: see "Local paths") |
| Agent user | a sandbox user: no session bus or display, so no keyring (run commands with `--force-file-secrets`, D029) and no browser |

## Commands

- `scripts/ci.sh`: local CI (fmt, clippy `-D warnings`, tests, release build, scenarios; about 35 min).
  `scripts/ci.sh --quick` skips the release build and the scenarios. Which to run: Git, "Which CI".
- `cargo test --workspace` / `cargo test -p pdmount-backend fake`: tests only.
- `cargo run -p pdmount -- doctor | status | login [--browser|--no-browser] | logout`, with `--profile <name>` to select a profile
  and, as the sandbox user, `--force-file-secrets` (D029).
- `pdmount mount [--foreground] [<mnt>] | unmount [--force] [<mnt>] | stats [--json] [--reset] | reset --keep-login | trash empty --yes`
  (D036). The daemon logs to `~/.local/state/pdmount/<profile>.log`.
- `scripts/repro/keyring-d024.sh [case ...]`: keyring tests (D024 hang, D029 modes) against gnome-keyring in a private D-Bus
  session (about 2 min). Not part of `scripts/ci.sh`.
- `scripts/testdata/make-m1-dataset.sh <dir>`: the deterministic M1 dataset (upload it with the official client).
- Scenario scripts: `[BACKEND=fake] VERIFY_MODE=live|remount|crash|fresh tests/scenarios/<name>.sh` (R1–R7 `r1`…`r7`, G1–G7
  `g1`…`g7` plus `g6_read`, `n1`, `a1`…`a6`, `e1`, `sha`, `queue`, `ops`, `kill9`); `tests/scenarios/run-all.sh [names]` runs a matrix
  (`MODES`, `SCENARIOS`; `STOP_ON`, default `429,fail` on the real account, none on the fake; `TRASH_MIN_FREE`, D036).
  The default backend is the real account; `BACKEND=fake` needs `cargo build -p pdmount --features fake-backend` and
  keeps its profile, mountpoint and fake drive in tmpfs under `SCENARIO_ROOT` (default `/dev/shm/pdmount-tests`).
  Writes go only to `<mount>/pdmount-test/<scenario>` (D028). Sizes: `R_FILES`, `R_BIG_MIB`, `G3_COMMITS`,
  `G4_BLOBS`.
  The scripts remove only their own output: a non-empty `DATASET` without `pdmount-m1/`, or a `FAKE_DIR` that is not
  a fake drive (`snapshot.json` or a `.seeded*` marker), is refused; `SCENARIO_ROOT` and the bench `FAKE_DIR`s are
  made 0700 and refused when they are a symlink or another user's (`tests/scenarios/selftest-lib.sh`, run by `scripts/ci.sh`).
- `tests/pjdfstest.sh [<dir>/<nn>.t | <dir> ...]`: the pjdfstest subset on the fake (about 75 s); expected failures in
  `tests/pjdfstest.list`. `FSX_DURATION=<s> tests/fsx.sh`: fsx with mmap on the fake (default 6 h). Both use the scenario
  setup (`SCENARIO_ROOT`, `PROFILE`); give a long fsx run its own `SCENARIO_ROOT` and `PROFILE` so other fake runs can
  go on meanwhile.
- Q1/Q6 probe (M3.1, real account, about a minute; writes only under `pdmount-test/q1/`): `PROBE_PROFILE_A=default
  PROBE_PROFILE_B=second PROBE_SECRETS=force-file cargo test -p pdmount probe -- --ignored --nocapture`
  (`PROBE_PROFILE_B=same`: A's session with another client UID).
- Journal property tests: `PROPTEST_CASES=10000 cargo test -p pdmount-core proptests` (about 140 s; default 32 cases).
- `scripts/bench/read.sh [ram_mib ...]`: client-side read MiB/s and p50/p99 on a fake mount at several `cache.ram_bytes`
  (M3.19.1; same release build).
- `scripts/bench/latency.sh [clients] [iterations]`: client-side p50/p99/max of namespace operations on a fake mount, for
  the §5.7 SLOs (release build with `--features fake-backend`; `SCENARIO_ROOT` on disk to measure the disk).
- `pdmount fake profile [--rtt <ms>] [--up <B/s>] [--down <B/s>] <dir>` / `fake profile clear <dir>` (test builds): a
  simulated network for the daemons on a fake drive (DESIGN §15.1), picked up at their next call.
- `scripts/bench/m4.sh [git|untar|read|slo|gc|all]`: the M4 exit benchmarks on a fake mount with the 80 ms / 20 MB/s
  profile (D057); about 15 min plus the first clone of git.git from kernel.org. State in `BENCH_ROOT` (default
  `~/.cache/pdmount-bench`), report in `target/bench/m4-<date>/report.md`; exit 1 if a row fails.
- Mounted fake-backend tests (`crates/pdmount-fuse/tests/`) need `/dev/fuse` and `fusermount3`; they skip otherwise.

Workspace layout:
- `crates/pdmount-core`: config, paths, device identity, store, inode tree, block cache, engine.
- `crates/pdmount-backend`: `Backend` trait, `fake`, `proton` adapter.
- `crates/pdmount-fuse`: the fuser `Filesystem` over the engine; `mount()`.
- `crates/pdmount`: the CLI binary, daemon and control socket.

Never run `login` against the user's account without being asked; it creates a real session.
Real-account tests (profile `default`, account `<test account>`) may create, change and delete only under
`My Files/pdmount-test/`. Everything else, `pdmount-m1/` in particular, is read-only for agents (D028). Exception: the
trash may be emptied when storage runs low during real-account tests (D036).
fsx, pjdfstest and soak runs use the fake backend only. Real-account scenarios run one at a time and stop at the first
HTTP 429. Ask the user before any long-running test against the real account (D030).
Commands that write to the keyring (`login`, `logout`) or open a browser can open desktop dialogs. Tell the user before
running them, or run them as the sandbox user once it exists.
For smoke tests, use a throwaway `--profile` and run `logout` afterwards.
