# AGENTS.md

Instructions for any agent (Claude Code or other) working in this repository.

## What this is

PdMount is a Linux FUSE filesystem that mounts Proton Drive "My Files" read-write. It is written in Rust using fuser 0.18
and builds on the community SDK (`proton-drive-sdk` / `proton-sdk-rs2` 0.4) through a patched fork.

It exists because the user verified that the write sync in pdcli's mount breaks `rsync` and `git`
(details: `docs/pdcli-evaluation.md`). The requirements are D001–D010 in `docs/decisions.md`.

## Status

The design phase is complete and implementation has not started. Status, the next action, open questions and handoff notes are in
**`TODO.md`**.

## Session start

1. Read `TODO.md`: its Next action and Handoff notes sections.
2. Read the index table in `docs/decisions.md`.
3. Read only the `DESIGN.md` sections the task needs (map below).

## Document map

| File | Use it for |
|---|---|
| `TODO.md` | Status, next action, milestone and task checklist, open questions (Q*), handoff notes |
| `docs/decisions.md` | Decision log (D*): what was decided, by whom, and why |
| `DESIGN.md` | The specification |
| `docs/sdk-patches.md` | SDK fork patches P1–P12: targets, fixes, upstream tracking |
| `docs/pdcli-evaluation.md` | Why the project is not a pdcli fork; what is reused from it |

`DESIGN.md` sections:

| § | Topic | § | Topic |
|---|---|---|---|
| 1 | Requirements RQ1–RQ8, non-goals, principles | 10 | Multi-device conflicts: detection, policy, names, git guidance |
| 2 | Prior attempts, SDK findings F1–F13, environment | 11 | Per-path access control (ro/rw), CLI |
| 3 | Workspace crates, `Backend` trait, data flows | 12 | Auth: web login, link presentation, tokens, secrets |
| 4 | Concurrency (fuser facts, hot/slow path, locking) | 13 | Storage paths, SQLite schema |
| 5 | Low latency: caching, fsync modes, SLOs | 14 | CLI commands, control socket, unmount safety |
| 6 | Read path: block map, cache, integrity | 15 | Tests: layers, rsync/git semantics, scenarios R/G/N/A/M |
| 7 | Write path: staging, upload job, atomic-save rename, `.pdignore` | 16 | Milestones M0–M5 and exit criteria |
| 8 | Namespace ops: journal, rename flags, setattr, unsupported ops | 17 | Risks |
| 9 | Sync engine: events, refresh, invalidation, offline | App. A/B | errno mapping, config reference |

## Rules

- **Decisions:** entries with `accepted` / `user` in `docs/decisions.md` are fixed; ask the user before changing one.
  Entries with `proposed` / `claude` are open for review.
- **Keeping documents in sync:** every design change updates `DESIGN.md`, `docs/decisions.md` (new or superseding entry) and
  `TODO.md` in the same commit.
- **End of every session:** update `TODO.md`: the "Last updated" line, the Next action, the checkboxes, and a rewrite of Handoff notes.
- **Single source of truth:** each fact lives in one place. Specifications go in `DESIGN.md`, reasons in `docs/decisions.md`, status in
  `TODO.md`. This file and `README.md` only point to them.
- **Conventions (accepted):**
  - CLI: path last (D009).
  - Milestone exit criteria are in DESIGN.md §16, built from the rsync/git scenarios in §15.3 (D005).
- **Working assumptions (proposed, pending user review; follow them unless the user decides otherwise):**
  - Denied writes return EROFS (D019).
  - Everything else in D011–D020.
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
- Commit or push only when the user asks.
- Commit message trailer: `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.

## Environment (verified 2026-10-19)

| Item | State |
|---|---|
| System | Gentoo |
| `/home` | btrfs (FICLONE reflinks available) |
| `/etc/fuse.conf` | no `user_allow_other`, so no AutoUnmount (proposal D020) |
| Toolchain | stable Rust (cargo), Go, clang, protoc, pkg-config, fusermount3 |
| Tools | git, rsync; pjdfstest and fsx built from source (locations: see "Local paths") |
| Agent user | a sandbox user: no session bus or display, so no keyring (0600 file fallback, D024) and no browser |

## Commands

- `scripts/ci.sh`: local CI (fmt, clippy `-D warnings`, tests, release build). Run it before every commit.
  `scripts/ci.sh --quick` skips the release build.
- `cargo test --workspace` / `cargo test -p pdmount-backend fake`: tests only.
- `cargo run -p pdmount -- doctor | status | login [--browser|--no-browser] | logout`, with `--profile <name>` to select a profile.
- The scenario scripts (`tests/scenarios/<name>.sh`, `VERIFY_MODE=live|remount|crash|fresh|second`) are created in M1/M2.

Workspace layout:
- `crates/pdmount-core`: config, paths, device identity; the engine from M1.
- `crates/pdmount-backend`: `Backend` trait, `fake`, `proton` adapter.
- `crates/pdmount`: the CLI binary.

Never run `login` against the user's account without being asked; it creates a real session.
Commands that write to the keyring (`login`, `logout`) or open a browser can open desktop dialogs. Tell the user before
running them, or run them as the sandbox user once it exists.
For smoke tests, use a throwaway `--profile` and run `logout` afterwards.
