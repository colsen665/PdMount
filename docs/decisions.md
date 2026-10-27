# Decisions

Rules for this log:

- It is append-only, and IDs are never reused.
- To change a decision, add a new entry and set the old entry's status to `superseded by Dnnn`. Don't edit the old decision text.
- **Decided by:**
  - `user`: fixed; only the user changes it.
  - `claude`: a proposal until the user accepts or rejects it. On acceptance, set `Status: accepted` and add `Accepted by user <date>`
    to Refs.
- **Template** (copy for new entries):

  ```markdown
  ## Dnnn — <title>
  - **Status:** proposed | accepted | rejected | superseded by Dnnn
  - **Decided by:** user | claude
  - **Date:** YYYY-MM-DD
  - **Context:** why a decision was needed.
  - **Decision:** what was decided.
  - **Alternatives:** options not chosen, one line each with the reason.
  - **Consequences:** what follows, including costs and limits.
  - **Refs:** DESIGN.md §…, TODO.md Q…, commits.
  ```

  The log fits in a single file while it stays under about 50 entries. Beyond that, split it into `docs/adr/NNNN-title.md`
  files using the same template.

## Index

| ID | Title | Status | Decided by | Date |
|---|---|---|---|---|
| D001 | Standalone crate, not a pdcli fork | accepted | user | 2026-10-19 |
| D002 | Community SDK via patched fork + upstream PRs | accepted | user | 2026-10-19 |
| D003 | Scope: My Files only | accepted | user | 2026-10-19 |
| D004 | Rust with fuser | accepted | user | 2026-10-19 |
| D005 | rsync and git scenarios are the milestone functional tests | accepted | user | 2026-10-19 |
| D006 | Low latency is a hard requirement | accepted | user | 2026-10-19 |
| D007 | Multi-device use, conflicts expected, zero data loss | accepted | user | 2026-10-19 |
| D008 | Per-path ro/rw access control, changeable and listable at runtime | accepted | user | 2026-10-19 |
| D009 | CLI convention: path is the last argument | accepted | user | 2026-10-19 |
| D010 | Web auth both opens the browser and prints the link | accepted | user | 2026-10-19 |
| D011 | Access rules bound to the node, not the path string | accepted | claude | 2026-10-19 |
| D012 | fsync default `local` (durable locally, upload later) | accepted | claude | 2026-10-19 |
| D013 | Directory mtime local-only until a folder update API exists | accepted | claude | 2026-10-19 |
| D014 | Sync fuser trait with tokio dispatch, not the async trait | accepted | claude | 2026-10-19 |
| D015 | Conflict policy: keep both, first committer keeps the name | accepted | claude | 2026-10-19 |
| D016 | Identical content (SHA-1) is never a conflict | accepted | claude | 2026-10-19 |
| D017 | Kernel writeback cache off in v1 | accepted | claude | 2026-10-19 |
| D018 | `.pdignore` default empty; ignored means local-only | accepted | claude | 2026-10-19 |
| D019 | Denied writes return EROFS | accepted | claude | 2026-10-19 |
| D020 | No AutoUnmount without `user_allow_other` | accepted | claude | 2026-10-19 |
| D021 | Handoff docs: AGENTS.md (CLAUDE.md refers to it), TODO.md, this log | accepted | user | 2026-10-19 |
| D022 | SDK fork hosted in a local bare repo | accepted | user | 2026-10-19 |
| D023 | GitHub use postponed until further notice | accepted | user | 2026-10-19 |
| D024 | Keyring calls time out after 20 s and fall back to 0600 files | superseded by D029 | claude | 2026-10-19 |
| D025 | Startup revalidation of listed directories until the event engine exists | superseded by D034 | claude | 2026-10-19 |
| D026 | The mount daemon re-executes itself instead of forking | accepted | claude | 2026-10-19 |
| D027 | Speculative API work is paced and yields to user requests | accepted | claude | 2026-10-19 |
| D028 | Real-account tests write only under `pdmount-test/` | accepted | user | 2026-10-19 |
| D029 | Secrets in files only with `--allow-file-secrets` or `--force-file-secrets` | accepted | user | 2026-10-20 |
| D030 | Stress and conformance tests only on the fake; a load budget for real-account runs | accepted | user | 2026-10-20 |
| D031 | Permission checks in the daemon, not `default_permissions` | accepted | claude | 2026-10-20 |
| D032 | Replies wait for the store commit, so a daemon crash loses nothing acknowledged | accepted | user | 2026-10-20 |
| D033 | Follow the official SDK's operational requirements: 5 block downloads, event-based sync before real-account runs | accepted | user | 2026-10-20 |
| D034 | Events are applied by listing the affected listed directories again | accepted | claude | 2026-10-20 |
| D035 | The Proton adapter fetches event nodes fresh, bypassing the SDK's entity cache | accepted | claude | 2026-10-20 |
| D036 | Agents may empty the real account's trash when storage runs low during real-account tests | accepted | user | 2026-10-21 |
| D037 | Visibility SLO for remote changes: `sync.poll_interval` + 10 s | accepted | user | 2026-10-22 |
| D038 | Fork patch P14: rename/move with a caller-supplied original name hash | accepted | user | 2026-10-22 |
| D039 | M3 is implemented task by task from `docs/plans/m3.md`, one commit per task | accepted | user | 2026-10-22 |
| D040 | Conflict engine rules: layers, lazy name clashes, minimal-chain resurrection, listing-time rename/rename | accepted | claude | 2026-10-22 |
| D041 | Connectivity state machine: one shared failure count, a probe, fast-failing reads, offline mount | accepted | claude | 2026-10-22 |
| D042 | `needs_login` keeps the daemon up; the daemon runs the web login; `notify-send` notifications | accepted | claude | 2026-10-22 |
| D043 | No revision post-check: a version restore that lands during our upload is overwritten | accepted | user | 2026-10-22 |
| D044 | Fork patch P15: read the trash time of v2 link details | accepted | user | 2026-10-22 |
| D045 | Conflicts are resolved through the namespace operations; xattrs come from RAM | accepted | claude | 2026-10-22 |
| D046 | Fixes from the two-device property test: trashed folders, freed clash names, renames into deleted folders | accepted | claude | 2026-10-22 |
| D047 | Two-device scenarios: the marker SLO scales with the number of change pages; catch-up barriers; the fake daemon ends `needs_login` | accepted | claude | 2026-10-22 |
| D048 | The M3 soak runs 3 h instead of 24 h; the risk is accepted | accepted | user | 2026-10-23 |
| D049 | RAM cache at working-set size; block memory reserved at startup in a pool, used by the RAM cache, `seed` and reads | accepted | user | 2026-10-24 |
| D050 | The visibility SLO (D037) is timed from the writer's commit, not its local write | accepted | user | 2026-10-25 |
| D051 | M3 exit: the M scenarios run on the real account only where no fault injection is needed | accepted | user | 2026-10-25 |
| D052 | A folder that loses its name to a remote rename keeps only its pending items | accepted | claude | 2026-10-25 |
| D053 | keep-remote is refused for a local-only conflict copy | accepted | user | 2026-10-26 |
| D054 | A listing request is answered by a listing that started after it | accepted | user | 2026-10-26 |
| D055 | A yielded name is recorded before the listing; open conflicts with a missing other copy relist their folder at start | accepted | user | 2026-10-26 |

---

## D001 — Standalone crate, not a pdcli fork
- **Status:** accepted
- **Decided by:** user
- **Date:** 2026-10-19
- **Context:** pdcli 0.4 already mounts Proton Drive, but its write sync breaks rsync and git (verified by the user). It also has
  structural latency problems.
- **Decision:** Build a new, mount-only workspace. Borrow small pdcli modules (credentials, pdignore matcher, service unit) under
  MIT with attribution.
- **Alternatives:** Fork pdcli's mount: it would inherit the synchronous trait, the global mutex and the journal semantics. Contribute upstream: this would depend on the
  maintainer accepting a redesign.
- **Consequences:** More initial work. Both tools can run side by side (separate config, keyring service and session).
- **Refs:** docs/pdcli-evaluation.md; DESIGN.md §2.1, §3.1.

## D002 — Community SDK via patched fork + upstream PRs
- **Status:** accepted
- **Decided by:** user
- **Date:** 2026-10-19
- **Context:** The SDK has defects that block a mount. Files over 40 MiB are truncated; the reported size and mtime are wrong; tokens are not refreshed on 401; errors are untyped.
- **Decision:** Pin a fork with `[patch.crates-io]`, put one fix per upstream PR, and drop each patch once it is merged.
- **Alternatives:** Work around the defects inside PdMount: duplicated logic. Write our own API client: the most work.
- **Consequences:** Fork maintenance. PdMount code touches only the `Backend` trait.
- **Refs:** docs/sdk-patches.md; DESIGN.md §2.2, §3.2; TODO.md Q4. PR submission deferred by D023.

## D003 — Scope: My Files only
- **Status:** accepted
- **Decided by:** user
- **Date:** 2026-10-19
- **Context:** Proton Drive also has Shared-with-me, Computers, Photos and Trash.
- **Decision:** v1 mounts My Files only.
- **Alternatives:** Mount the other trees too; that adds work without serving a requirement.
- **Consequences:** The other trees are non-goals for v1.
- **Refs:** DESIGN.md §1.2.

## D004 — Rust with fuser
- **Status:** accepted
- **Decided by:** user
- **Date:** 2026-10-19
- **Context:** Language choice.
- **Decision:** Rust (edition 2024) with fuser 0.18 and the pure-Rust mount through fusermount3.
- **Alternatives:** Go with go-fuse and go-proton-api; not chosen.
- **Consequences:** Depends on the community Rust SDK (D002). Building needs Go, clang and protoc for `gopenpgp-sys`.
- **Refs:** DESIGN.md §4.

## D005 — rsync and git scenarios are the milestone functional tests
- **Status:** accepted
- **Decided by:** user
- **Date:** 2026-10-19
- **Context:** pdcli's failures showed up in rsync and git workflows.
- **Decision:** Every milestone's exit criteria are scripted scenarios (R*, G*, plus N*, A* and M*), run in the `live`, `remount`, `crash`,
  `fresh` and `second` modes. On a fresh client or second device, an empty `rsync -ani` result is the acceptance test that size and mtime
  are read correctly from the API.
- **Alternatives:** Only generic POSIX suites; they miss the patterns rsync and git actually use.
- **Consequences:** The scenario scripts are deliverables in their own right.
- **Refs:** DESIGN.md §15.2, §15.3, §16.

## D006 — Low latency is a hard requirement
- **Status:** accepted
- **Decided by:** user
- **Date:** 2026-10-19
- **Context:** Earlier attempts took 1–2 s per operation.
- **Decision:** An explicit mechanism, not best effort: local-first metadata, write-back, prefetch and pins, kernel caching with
  invalidation, measured SLOs and a network guard on the hot path.
- **Alternatives:** None considered.
- **Consequences:** The design is local-first, so a durable journal and conflict handling are required (D007).
- **Refs:** DESIGN.md §5.

## D007 — Multi-device use, conflicts expected, zero data loss
- **Status:** accepted
- **Decided by:** user
- **Date:** 2026-10-19
- **Context:** Several devices, and the official apps, edit the same tree at the same time.
- **Decision:** Conflicts are normal operation. Every version that was acknowledged to an application must remain reachable.
- **Alternatives:** None considered.
- **Consequences:** Base-revision tracking, three-layer detection and conflict copies (D015, D016).
- **Refs:** DESIGN.md §10.

## D008 — Per-path ro/rw access control, changeable and listable at runtime
- **Status:** accepted
- **Decided by:** user
- **Date:** 2026-10-19
- **Context:** Some subtrees must be protected against local writes.
- **Decision:** A rule sets a path and its sub-entries to Read or Read+Write. It can be set, unset and listed with the CLI while
  the filesystem is mounted.
- **Alternatives:** Only a mount-wide `-o ro`.
- **Consequences:** Rule store, enforcement in handlers, kernel invalidation on change (see D011, D019).
- **Refs:** DESIGN.md §11.

## D009 — CLI convention: path is the last argument
- **Status:** accepted
- **Decided by:** user
- **Date:** 2026-10-19
- **Context:** The draft had `access set <path> ro|rw`.
- **Decision:** Options first, then the verb or mode, then at most one positional path, which comes last. Example: `access set ro|rw <path>`.
- **Alternatives:** —
- **Consequences:** Applied to every subcommand (DESIGN.md §14.2), e.g. `conflicts resolve keep-both <id|path>`.
- **Refs:** DESIGN.md §14.1.

## D010 — Web auth both opens the browser and prints the link
- **Status:** accepted
- **Decided by:** user
- **Date:** 2026-10-19
- **Context:** Login is the browser session fork. Headless and SSH sessions cannot open a browser.
- **Decision:** The link is always printed. The browser opens according to `--browser`/`--no-browser` or `auth.browser = auto|always|never`.
  Under systemd, the link is shown in `status` and in a desktop notification.
- **Alternatives:** —
- **Consequences:** The M0 exit criterion tests both paths.
- **Refs:** DESIGN.md §12.2.

## D011 — Access rules bound to the node, not the path string
- **Status:** accepted
- **Decided by:** claude
- **Date:** 2026-10-19
- **Context:** D008 does not say what happens to a rule when its entry is renamed, moved or deleted.
- **Decision:** A rule attaches to the node (`link_id`, or `ino` before upload). It follows renames and moves, including remote ones.
  When the node is deleted, the rule becomes `orphaned` and is never applied to a new entry with the same name.
- **Alternatives:** Path-string rules: they survive delete and recreate, but a remote rename silently unprotects the subtree.
- **Consequences:** `list` shows both the current path and the path recorded when the rule was set.
- **Refs:** DESIGN.md §11.1. Accepted by user 2026-10-20.

## D012 — fsync default `local`
- **Status:** accepted
- **Decided by:** claude
- **Date:** 2026-10-19
- **Context:** git fsyncs refs and packs on every commit. Blocking each fsync on an upload would cost several network RTTs per commit.
- **Decision:** By default, fsync means the data is durable on the local disk and guaranteed to be uploaded later. `fsync.mode = remote`, per glob, makes fsync block until the upload finishes.
  `pdmount sync <path>` is an explicit barrier that waits until everything under the path is uploaded.
- **Alternatives:** Always remote: correct "on server" semantics, but too slow for git.
- **Consequences:** A successful fsync does not mean the data is on the server.
- **Refs:** DESIGN.md §5.6. Accepted by user 2026-10-20.

## D013 — Directory mtime local-only until a folder update API exists
- **Status:** accepted
- **Decided by:** claude
- **Date:** 2026-10-19
- **Context:** The folder XAttr can be set at creation (P5). No update API is known.
- **Decision:** Directory mtime and mode are stored locally. Only the creation-time mtime is sent remotely.
- **Alternatives:** Re-create folders to change their mtime: destructive and expensive.
- **Consequences:** Scenario R1 uses `--omit-dir-times` in `fresh` and `second` modes.
- **Refs:** DESIGN.md §8.6; TODO.md Q2. Accepted by user 2026-10-20.

## D014 — Sync fuser trait with tokio dispatch
- **Status:** accepted
- **Decided by:** claude
- **Date:** 2026-10-19
- **Context:** fuser 0.18's `AsyncFilesystem` covers only lookup, getattr, read and readdir (`fuser/experimental.rs:220-257`).
- **Decision:** Use the synchronous trait. Hot operations are answered in RAM. Slow operations move the `Reply` into a tokio task with a deadline. Every callback runs inside
  `catch_unwind`.
- **Alternatives:** The async trait: it cannot carry a read-write filesystem.
- **Consequences:** Deadlines are mandatory, because FUSE_INTERRUPT is unsupported.
- **Refs:** DESIGN.md §4. Accepted by user 2026-10-20.

## D015 — Conflict policy: keep both, first committer keeps the name
- **Status:** accepted
- **Decided by:** claude
- **Date:** 2026-10-19
- **Context:** D007 requires zero data loss.
- **Decision:** The server version keeps the name. The other version becomes `<stem> (conflict <device> <UTC time>)<.ext>`.
  A remote delete combined with a local edit resurrects the file. A local delete combined with a remote edit does not trash the file.
- **Alternatives:** Last writer wins: loses data. Three-way merge: not generally possible for binary files.
- **Consequences:** Conflict copies need user attention (`conflicts` CLI, notifications).
- **Refs:** DESIGN.md §10.5–§10.8. Accepted by user 2026-10-20.

## D016 — Identical content (SHA-1) is never a conflict
- **Status:** accepted
- **Decided by:** claude
- **Date:** 2026-10-19
- **Context:** git objects and packs are content-addressed, and rsync re-runs write identical bytes.
- **Decision:** If a snapshot's SHA-1 equals the remote active revision's SHA-1, adopt the remote revision and skip the upload.
- **Alternatives:** Always create conflict copies: noisy, and it breaks `.git`.
- **Consequences:** Needs the decrypted SHA-1 from P4.
- **Refs:** DESIGN.md §10.3. Accepted by user 2026-10-20.

## D017 — Kernel writeback cache off in v1
- **Status:** accepted
- **Decided by:** claude
- **Date:** 2026-10-19
- **Context:** With writeback cache the kernel owns mtime and size and holds dirty pages.
- **Decision:** Off in v1. M4 runs an experiment behind `kernel.writeback_cache`, and it becomes the default only if the full scenario suite passes.
- **Alternatives:** On from the start: risks `rsync -t` and invalidation correctness.
- **Consequences:** More write requests reach the daemon; acceptable because throughput is limited by uploads.
- **Refs:** DESIGN.md §5.4.1. Accepted by user 2026-10-20.

## D018 — `.pdignore` default empty; ignored means local-only
- **Status:** accepted
- **Decided by:** claude
- **Date:** 2026-10-19
- **Context:** pdcli's default list includes `.git/` and `*.tmp`, and its ignore handling trashes originals when files are saved atomically.
- **Decision:** The default list is empty. Ignored files persist locally and are never uploaded. A rule only blocks creating new remote nodes.
- **Alternatives:** Ship a default list: it breaks git.
- **Consequences:** Users opt in to ignore rules.
- **Refs:** DESIGN.md §7.7. Accepted by user 2026-10-20.

## D019 — Denied writes return EROFS
- **Status:** accepted
- **Decided by:** claude
- **Date:** 2026-10-19
- **Context:** D008 needs an error code for writes in a `ro` subtree.
- **Decision:** EROFS for all denied writes. Write bits are cleared in the reported mode.
- **Alternatives:** EACCES/EPERM: these read as permission problems, which misleads users.
- **Consequences:** rsync and git report "Read-only file system".
- **Refs:** DESIGN.md §11.2. Accepted by user 2026-10-20.

## D020 — No AutoUnmount without `user_allow_other`
- **Status:** accepted
- **Decided by:** claude
- **Date:** 2026-10-19
- **Context:** fuser refuses AutoUnmount without `allow_other`/`allow_root` (`fuser/session.rs:163-171`), and this machine's
  `/etc/fuse.conf` lacks `user_allow_other`.
- **Decision:** Don't depend on AutoUnmount. Use systemd `ExecStopPost=fusermount3 -uz` and `unmount --force` instead. `doctor` explains the fuse.conf option.
- **Alternatives:** Require the user to edit `/etc/fuse.conf`.
- **Consequences:** After `kill -9`, the mount must be cleaned up explicitly (tested in M1).
- **Refs:** DESIGN.md §14.4. Accepted by user 2026-10-20.

## D021 — Handoff docs: AGENTS.md, TODO.md, this log
- **Status:** accepted
- **Decided by:** user
- **Date:** 2026-10-19
- **Context:** Sessions need a reliable entry point and must be able to tell fixed decisions from proposals.
- **Decision:** Agent instructions live in `AGENTS.md`, and `CLAUDE.md` only refers to it. Status and next steps go in `TODO.md`. Decisions go in this log
  (format suggested by Claude).
- **Alternatives:** Everything in CLAUDE.md, or in user-local memory: tool-specific, and not versioned with the repo.
- **Consequences:** These files are updated as part of every session (rules in AGENTS.md).
- **Refs:** AGENTS.md; TODO.md.

## D022 — SDK fork hosted in a local bare repo
- **Status:** accepted
- **Decided by:** user
- **Date:** 2026-10-19
- **Context:** D002 requires a fork of the SDK. Its hosting was open (TODO.md Q4).
- **Decision:** The fork is the local bare repo `<fork>`. The path follows the existing `<name>.git`
  pattern and was chosen by claude; the user only specified "local bare repo".
  - Remote `upstream` = `https://github.com/tirbofish/proton-sdk-rs2`.
  - The tag `base/0.4.0` = `409ca119` (upstream `main`, the same commit as the published 0.4.0 crates).
  - Integration branch `pdmount`, which PdMount pins.
  - One branch per patch.
- **Alternatives:** A GitHub fork. It would allow opening upstream PRs directly; that is left open as Q8.
- **Consequences:**
  - `[patch.crates-io]` uses `git = "file://<local bare repo>"`. This was verified with `cargo generate-lockfile` on 2026-10-19.
  - CI cannot fetch a local repo. CI needs either a mirror or vendored sources (see M0.7).
  - Submitting patches upstream needs a separate route (Q8).
- **Refs:** docs/sdk-patches.md "Fork repository"; TODO.md Q4, Q8. The CI consequence was revised by D023: CI runs locally and reads the fork through `file://`, so no mirror is needed.

## D023 — GitHub use postponed until further notice
- **Status:** accepted
- **Decided by:** user
- **Date:** 2026-10-19
- **Context:** Upstream PRs (D002), hosted CI (DESIGN.md §15.1) and a possible GitHub fork (D022, Q8) all assumed GitHub.
- **Decision:** Until the user lifts this:
  - Not allowed: creating GitHub repos or forks, opening PRs or issues, using GitHub Actions, or pushing to GitHub.
  - Still allowed: read-only `git fetch upstream` from the public `tirbofish/proton-sdk-rs2`, used only when rebasing the fork.
- **Alternatives:** —
- **Consequences:**
  - Fork patches are kept as upstream-ready branches but not submitted (Q8 deferred).
  - CI runs locally (`scripts/ci.sh`, TODO.md M0.7).
  - The fork stays local only (D022).
- **Refs:** docs/sdk-patches.md; TODO.md M0.7, Q8; DESIGN.md §15.1.

## D024 — Keyring calls time out after 20 s and fall back to 0600 files
- **Status:** superseded by D029
- **Decided by:** claude
- **Date:** 2026-10-19
- **Context:** During the M0 sign-in smoke test, `set_password` on the Secret Service never returned (probably a locked
  collection waiting for an unlock prompt), so `pdmount login` hung before showing the link. A daemon under systemd would
  hang the same way.
- **Decision:** Run each keyring call on a helper thread with a 20 s timeout. On timeout, use the mode-0600 files in the
  profile data directory (the existing no-keyring fallback) for the rest of the process, and log a warning. A later
  process tries the keyring again and moves the secrets into it once it answers.
- **Alternatives:** Block until the user unlocks: hangs forever without a desktop. Files only: weaker at rest than an
  unlocked keyring.
- **Consequences:** With a locked keyring, secrets sit in 0600 files (full-disk encryption recommended, §17). If
  the keyring keeps not answering, each command waits up to 20 s once.
- **Refs:** DESIGN.md §12.1; crates/pdmount/src/credentials.rs. Cause confirmed by the user (2026-10-19): the keyring opened an
  unlock dialog while the terminal window was out of focus, and it went unnoticed. The user is setting up a sandbox user for
  agent work.
  Reproduced 2026-10-20 as the sandbox user with `scripts/repro/keyring-d024.sh`, against gnome-keyring in a private D-Bus session:
  - Locked collection, with a prompter that never answers (the unnoticed dialog): the bare `keyring` call
    (`dbus-secret-service` 4.1.0 waits up to a year for `Prompt.Completed`) was still blocked when killed after 90 s.
    PdMount fell back to the 0600 file after 20 s and logged the warning.
  - A later process with the keyring unlocked moved the session from the file into the keyring.
  - Locked, with no display (the real `gcr-prompter` cannot start): the prompt is dismissed at once and the fallback is
    immediate, so there is no hang. The hang needs a session where a dialog can appear.
  - `doctor` reports a locked keyring as OK, because its probe reads an entry that does not exist and so needs no unlock.

## D025 — Startup revalidation of listed directories until the event engine exists
- **Status:** superseded by D034 (2026-10-20): mounts resume the event stream; the listed tree is walked only for a refresh
- **Decided by:** claude
- **Date:** 2026-10-19
- **Context:** M1 serves the stored tree immediately after a remount (§5.1), but the event engine that keeps it current is
  M3. Without a catch-up step, files changed by other clients while unmounted would stay invisible, and stale sizes would
  break rsync's quick check.
- **Decision:** After mounting, list every listed directory again in the background (4 at a time, root first), apply the
  differences like any listing and send the resulting kernel invalidations (§9.5). The event cursor is still recorded on
  the first mount (§9.1), so M3 can start polling from it.
- **Alternatives:** Do nothing until M3: stale views after every remount. Poll by re-listing periodically: costs one
  request per listed directory per interval, the problem events solve.
- **Consequences:** One listing request per listed directory per mount; changes made while mounted appear only after the
  next mount until M3. M3's `refresh` handling (§9.4) reuses the same code path.
- **Refs:** DESIGN.md §5.1, §9.4; crates/pdmount-core/src/engine.rs (`revalidate`). Accepted by user 2026-10-20.

## D026 — The mount daemon re-executes itself instead of forking
- **Status:** accepted
- **Decided by:** claude
- **Date:** 2026-10-19
- **Context:** `pdmount mount` must return while the filesystem keeps running (§14.2). A classic `fork` + `setsid` daemon
  needs unsafe code, which the workspace denies, and forking a process that already started threads (tokio) is unsound.
- **Decision:** `pdmount mount` starts `pdmount mount --foreground` with `Command::process_group(0)`, stdin from
  `/dev/null` and output appended to the log file, then waits until the child's control socket answers `ping`. The
  foreground mode is also what a systemd unit runs.
- **Alternatives:** `fork`/`setsid` via `nix`: unsafe, and unsound after threads exist. Always foreground: unusable
  from an interactive shell without `&` and `nohup`.
- **Consequences:** The daemon is not a session leader; it ignores SIGHUP. Start-up errors are shown by reading the new
  part of the log file.
- **Refs:** DESIGN.md §14.4; crates/pdmount/src/daemon.rs. Accepted by user 2026-10-20.

## D027 — Speculative API work is paced and yields to user requests
- **Status:** accepted
- **Decided by:** claude
- **Date:** 2026-10-19
- **Context:** In the first real-account run of M1, a `find` over 60 freshly mounted folders started listing and
  small-file prefetch for each folder (§5.3) at a concurrency of 4. Proton answered HTTP 429. The SDK retries with back-off
  longer than the 20 s metadata deadline, so four user-visible `opendir` calls returned EIO.
- **Decision:** All speculative API work goes through one gate: listing prefetch, small-file prefetch and startup
  revalidation (D025). It runs one request at a time, at most one start every 500 ms, and only while no foreground request
  is in flight (a listing or read that a FUSE request waits on). Readahead is not gated: it downloads from storage with
  a block map that is already known.
- **Alternatives:** Disable prefetch: loses §5.3's benefit for small files. A token bucket shared with foreground work:
  foreground would still queue behind background bursts.
- **Consequences:**
  - Prefetch finishes later. A folder of 1000 small files takes about 8 minutes to prefetch.
  - Revalidation of N listed folders takes about N/2 seconds.
  - Foreground requests are not paced at all. A cold `rsync` of 1000 small files still costs about 2 API calls per file
    (6 minutes in the M1 run). Q7 tracks the rate limits.
- **Refs:** DESIGN.md §5.3; crates/pdmount-core/src/gate.rs; TODO.md Q7. Accepted by user 2026-10-20.

## D028 — Real-account tests write only under `pdmount-test/`
- **Status:** accepted
- **Decided by:** user
- **Date:** 2026-10-19
- **Context:** From M2 on, scenario scripts write to the real account (`<test account>`, profile `default`). The account
  holds the M1 dataset `pdmount-m1/`, which later milestones keep reading.
- **Decision:** Agents and scripts create, change and delete only under `My Files/pdmount-test/`. Everything else is
  read-only for them.
- **Alternatives:** Writes anywhere except `pdmount-m1/` (not chosen).
- **Consequences:** Real-account scenario scripts take their target directory under `pdmount-test/` and clean it up
  there only. The persistable fake covers anything that would need to touch other paths.
- **Refs:** AGENTS.md; DESIGN.md §15.3.

## D029 — Secrets in files only with `--allow-file-secrets` or `--force-file-secrets`
- **Status:** accepted
- **Decided by:** user
- **Date:** 2026-10-20
- **Context:** D024 fell back to 0600 files silently, whenever the keyring did not answer. The D024 reproduction
  (D024 Refs) also showed that any keyring error counted as "no entry". After a cancelled unlock dialog, PdMount reported
  "not logged in" and created a new cache key, which made the existing caches unreadable. The user decided that secrets
  may go to files in the user directory only when a command-line flag says so.
- **Decision:**
  - Three modes, from two mutually exclusive global flags:
    - no flag: keyring only;
    - `--allow-file-secrets`: the keyring, and 0600 files only when it is unavailable;
    - `--force-file-secrets`: files only, without touching the keyring.
  - A keyring answer is one of three: found, missing, or unavailable (no Secret Service, prompt dismissed, timed out,
    any other error). Only "missing" counts as no entry. Without a flag, "unavailable" fails the command and names the
    cause and the two flags.
  - A secrets file, if present, is the newest copy: every successful keyring write deletes it. It is read first, in
    every mode. Outside force mode it is then moved into the keyring and deleted, unless another daemon of the profile is
    running, in which case it is left in place.
  - Creating a new cache key deletes the profile's SDK caches and block cache, which the old key encrypted.
  - Keyring calls wait up to 120 s. After 2 s, a note on the terminal says an unlock dialog may be open behind
    other windows. After the keyring is unavailable once, the process stops calling it, except for one last token save
    when the daemon stops.
  - `login` ends the new server session if it cannot store it. `logout` reports keyring entries it could not delete.
    The daemon stores tokens only when they have changed. `mount` reads the stored session before it starts the
    daemon, so an unlock dialog appears while the terminal is waiting. The daemon is started with the same flag.
  - Tests use force mode, except tests that check keyring behaviour.
- **Alternatives:**
  - D024's silent fallback: puts secrets on disk without the user's consent, and hides cancelled dialogs.
  - A single flag: no way to keep agents and tests off a keyring that works.
  - An environment variable or config option: easier to leave on by accident (the user chose flags only).
- **Consequences:**
  - Without a session bus (the agent user, headless machines), every command that reads secrets needs a flag.
    The systemd unit and the scenario scripts pass one.
  - Switching between modes can create a new cache key, which costs a cache rebuild. In force mode, the session and
    the cache key must both be in files; otherwise a new login is needed.
  - After `logout --force-file-secrets`, keyring entries from earlier runs may remain until a `logout` without a flag.
- **Refs:** DESIGN.md §12.1, §12.4, §13.1, §14.1, §14.2; crates/pdmount/src/credentials.rs; scripts/repro/keyring-d024.sh;
  TODO.md M2.0b. Supersedes D024.

## D030 — Stress and conformance tests only on the fake; a load budget for real-account runs
- **Status:** accepted
- **Decided by:** user
- **Date:** 2026-10-20
- **Context:** The M2 exit check includes a 6 h fsx run, and the task did not say which backend it uses. fsx issues
  thousands of small writes and truncates per minute; against the real account that becomes a stream of revision
  uploads that could get the account banned. The M1 run already saw HTTP 429 at a concurrency of 4 (TODO.md Q7).
- **Decision:**
  - fsx, pjdfstest and the soak test run only over the fake backend. Its fault injection supplies latency and 429s.
  - Real-account scenario runs (only under `pdmount-test/`, D028) run one at a time, are not looped or repeated without
    a reason, and stop at the first HTTP 429 instead of retrying.
  - Datasets for many-small-file scenarios (G2, G3) stay modest until Q7 has measured limits.
  - Agents ask the user before starting any long-running test against the real account.
- **Alternatives:** A short fsx run against the real account: still upload-heavy, and it proves nothing the fake cannot.
- **Consequences:** The real API is exercised only by the scenario scripts. Rate-limit behaviour under load is learned
  from short, deliberate runs (Q7), not from stress tools.
- **Refs:** DESIGN.md §15.1, §15.3, §16; TODO.md M2.8, M2.9, Q7; AGENTS.md.

## D031 — Permission checks in the daemon, not `default_permissions`
- **Status:** accepted
- **Decided by:** claude
- **Date:** 2026-10-20
- **Context:** §11.2 asked for two things at once: denied writes in `ro` subtrees return EROFS (D019), and `getattr`
  clears the write bits there with `default_permissions` enforcing them in the kernel. With `default_permissions`, the
  kernel checks the cleared bits itself and answers EACCES ("Permission denied") before PdMount sees the request, so
  `rsync` and `git` never report "Read-only file system". Found while writing scenario A1.
- **Decision:** Mount without `default_permissions`. The engine checks permissions itself, from the owner bits of
  the stored (unmasked) mode: read for opening to read and for `opendir`; write for opening to write and for
  truncation; write and search on a directory to change its entries; search for lookups; write on a directory moved
  to another parent. Only the mounting user reaches the mount (no `allow_other`, D020), so the owner bits are the ones
  that apply. EROFS for `ro` subtrees comes first, as on a read-only file system. `access(2)` is answered the same way.
- **Alternatives:**
  - Keep `default_permissions` and give up the cleared write bits: tools no longer see read-only before they try.
  - Keep both and accept EACCES: contradicts D019.
- **Consequences:** The daemon carries POSIX permission semantics for the owner. M2.9's pjdfstest subset checks them.
- **Refs:** DESIGN.md §11.2; crates/pdmount-core/src/engine.rs (`ensure_allowed`); D019. Accepted by user 2026-10-20.

## D032 — Replies wait for the store commit, so a daemon crash loses nothing acknowledged
- **Status:** accepted
- **Decided by:** user
- **Date:** 2026-10-20
- **Context:** Scenario G5 in `crash` mode lost git's last checkout steps (the `index.lock` → `index` rename, the HEAD
  update). The daemon had answered those requests, but their store rows were still queued when `kill -9` came, and git
  does not fsync them. §5.6 promised only fsync'd data across a crash, so this was allowed, but a daemon crash (bug, OOM
  kill) is far more likely than a power loss, and it left repositories half switched. TODO.md Q9 offered (a) fsync in the
  scenarios before the kill, or (b) commit the store before replying. The user chose (b), to be measured against the
  §5.7 SLOs.
- **Decision:**
  - A successful reply to a request that changes state (create, mkdir, unlink, rmdir, rename, setattr, open, write,
    flush, release) is sent once the store batch holding its rows has committed with `synchronous=NORMAL` (written to the
    WAL, no fsync). The fuser thread hands the Reply to the store writer and goes on; requests that queued nothing are
    answered at once. A crash of the daemon then loses nothing the kernel was told. A power loss still loses what was
    not fsync'd.
  - The writer never waits for the disk. A sync thread acknowledges durable writes after fsyncing the WAL (what
    `synchronous=FULL` did at commit), and checkpoints when the store is idle for 1 s or after 20 000 writes.
  - Staging removals are batched for 1 s to share one fsync.
- **Alternatives:**
  - (a) fsync before the kill in the scenarios: no hot-path cost, but real daemon crashes keep losing acknowledged
    renames.
  - Blocking the fuser thread until the commit: same guarantee, but 4 threads would serialize on the writer.
  - `synchronous=FULL` per reply: a fsync per namespace operation, several ms on SSD.
- **Consequences:**
  - Measured 2026-10-20 with `scripts/bench/latency.sh` (Python client timing 8 operations per iteration, fake backend
    on tmpfs, daemon state on the btrfs SATA SSD or on tmpfs; 1 client × 2000 iterations and 4 × 1000), p99 in µs,
    baseline (replies not deferred) → D032: btrfs 1 client create 84 → 214–285, other namespace ops 25 → 141–259, write 21 → 54–58; btrfs 4
    clients create 149 → 300–321, others 49 → 251–317, write 39 → 210–218; tmpfs 4 clients create 119 → 303, others
    76 → 262, write 63 → 210. Every case is within §5.7 (namespace < 2 ms, write < 1 ms); the maximum stayed ≤ 1.7 ms.
  - Getting there exposed three older stalls that hit every case, baseline included, and were fixed with it: the
    descriptor table growing under load (§4.4), staging files closed under the staged-map lock while btrfs flushed them,
    and inode reservations running out (now 16 384 at a time). Before D032's sync thread, a reply could also wait behind
    a `synchronous=FULL` batch: btrfs create p99 was 12 ms.
  - Proptests' `kill()` (store flushed first) now models the mount exactly.
  - RQ6 reworded by the user (2026-10-20) to match: acknowledged writes survive a daemon crash; fsync'd writes also
    survive a power loss.
- **Refs:** DESIGN.md §4.3, §4.4, §5.2, §5.6, §7.1; crates/pdmount-core/src/store/mod.rs (`Store::after`, `Syncer`);
  crates/pdmount-fuse/src/lib.rs (`PdFs::committed`); TODO.md Q9.

## D033 — Follow the official SDK's operational requirements: 5 block downloads, event-based sync before real-account runs
- **Status:** accepted
- **Decided by:** user
- **Date:** 2026-10-20
- **Context:** The official SDK's "Operational requirements" (`cs/README.md`) exist to "protect service availability",
  and PdMount must not put it at risk. They apply to clients that call the API without the official SDK, which PdMount
  does through the community SDK: they must match its caching, event-based sync, parallelism limits and exponential
  backoff. A review found:
  - Honest `x-pm-appversion`, official endpoints, the third-party notice, caching and backoff are met (P12's retry with
    `Retry-After` and a 60 s stop after repeated 429s; the journal's per-item backoff; paced background work, D027).
  - Block downloads could run 14 at a time (interactive 8, readahead 4, prefetch 2). The official SDKs limit transfers at
    two levels. C#: 6 files and 6 block transfers in total per direction (`cs/client/cs/src/Proton.Drive.Sdk/
    ProtonDriveClient.cs:28`, `Nodes/TransferQueue.cs:39-41`). JS: downloads 5 files with up to 10 blocks each
    (`download/queue.ts:17`, `download/fileDownloader.ts:22`); uploads 5 files with up to 5 blocks each and 40 MiB of
    expected size in total (`upload/queue.ts:11,20`, `upload/streamUploader.ts:69`).
  - There is no event-based sync yet (M3). Startup revalidation (D025) walks the listed tree on every mount, which the
    requirements forbid when frequent, and the real-account scenario matrix mounts about 60 times.
  - The planned event loop (§9.2: 5 s after activity, 15 s normal, 30 s idle) would poll more often than the official
    clients: 30 s for the own volume in the foreground, 10 min in the background, retries backing off by the interval
    times 1, 1, 2, 3, 5, 8, 13 (`cs/client/js/src/internal/events/eventScheduler.ts:1-2`, `eventManager.ts:4,132`).
- **Decision:**
  - Transfer limits stay slightly below the official SDKs (the user's choice): at most 5 block downloads in flight over
    all priorities, one below C#'s 6, with readahead 2 and prefetch 1 within them so interactive reads keep at least 2.
    Uploads go one file at a time with up to 6 blocks (the community SDK's fixed default, the same as C#); when P7's
    file limit is used (M4), files stay below 5 and blocks below 6.
  - Event-based sync is brought forward from M3 (TODO.md M2.10) and done before the real-account matrix. Once a stored
    cursor resumes the event stream at mount, startup revalidation (D025) runs only for a `refresh` (§9.4).
  - The event loop polls like the official clients: every 30 s, immediately again while `more` is set, retries at 30 s
    times 1, 1, 2, 3, 5, 8, 13 with jitter.
  - No switches or test-only settings that bypass these rules (proposed and dropped: turning revalidation off and
    lowering upload concurrency for test profiles).
- **Alternatives:** Config switches for test runs (rejected by the user: the product would still walk the tree on
  every mount). Keep 14 downloads and wait for 429s: the rules ask for the SDK's limits.
- **Consequences:**
  - Cold reads of many blocks are capped at 5 in flight; large sequential reads get less readahead.
  - Uploads already go out one file at a time: PdMount never sets P7's `max_concurrent_file_transfers`, so the SDK's
    default of 1 applies whatever `upload.concurrency` (4) says. Its 6 block uploads equal C#'s limit rather than staying
    below it; `ProtonDriveClient::new` cannot change that, so lowering it needs the SDK's lower-level constructor (M4,
    with P7).
  - The real-account matrix waits for M2.10. Remote changes become visible while mounted (within about 30 s), which
    M3's exit criterion asks for anyway.
- **Refs:** DESIGN.md §4.4, §5.1, §6.4, §9; crates/pdmount-core/src/cache.rs (`DOWNLOADS`); D025, D027, D030; TODO.md M2.10.

## D034 — Events are applied by listing the affected listed directories again
- **Status:** accepted
- **Decided by:** claude
- **Date:** 2026-10-20
- **Context:** D033 brings event-based sync (§9) forward. Events carry only ids. DESIGN §9.3 planned to fetch each node
  with `get_nodes` and apply it to the tree directly, with its own rules per event kind and inode state. The tree already
  has one reconciliation for remote state, `Tree::apply_listing_at`, which handles moves between directories, pending local
  changes (placement, content, mode and mtime), stale data (`remote_stamp`), own trashes, name collisions and resurrection,
  and which the journal property tests have hardened (four bugs found and fixed there). A second rule set for events would
  have to reproduce all of it.
- **Decision:**
  - Resolve the links of a page with batched `get_nodes`, only for links the tree knows or whose parent is listed.
  - Skip echoes: nodes whose parent, name and active revision equal the tree's remote state. The executor records every
    remote change it makes there, so the client's own changes cost no listing.
  - For the rest, list again each listed directory the node is in now and was in before, a directory a node left after the
    one it entered (so moves keep inodes and listed subtrees).
  - Mounts resume from the stored cursor, so startup revalidation (D025) is gone. A `refresh` lists every listed directory
    in the background while events keep flowing from the fresh cursor; the cursor is stored again only after that pass
    succeeds.
- **Alternatives:**
  - Per-node application (§9.3's plan): fewer API calls when a busy directory changes often, but a second set of rules.
  - Refresh in the foreground of the event loop (the first version): simple, but a refresh of a large tree, paced at two
    listings a second, stopped every remote change from showing for minutes (seen in the scenario matrix: 499
    directories, 4.3 min).
- **Consequences:**
  - A real remote change costs one `get_nodes` per page and one listing per touched directory, at most once per poll
    (30 s) and paced (D027). A directory with many children that changes remotely at every poll costs a full listing each
    time; per-node application can replace this later if M4 measures it as a problem.
  - Tested by event tests on the fake (every kind, moves, echoes, local changes, pages, cursor durability, refresh with
    events during it), by the property tests with another device's changes, and by scenario E1.
  - The property tests found two bugs on the way, both fixed: a staging file left over from an upload revived old content
    after a crash once the remote revision had moved on; and that fix at first dropped staging of files written after an
    upload, because stored rows do not say `Content::Staged` (the generations decide).
- **Refs:** DESIGN.md §5.1, §7.5, §9.1–§9.4, §15.1, §15.3 (E1), §16; crates/pdmount-core/src/events.rs; D025, D027, D033.
  Accepted by user 2026-10-20.

## D035 — The Proton adapter fetches event nodes fresh, bypassing the SDK's entity cache
- **Status:** accepted
  Accepted by user 2026-10-21 (as amended: batched, P13).
- **Decided by:** claude
- **Date:** 2026-10-20
- **Context:** The real-account check of M2.10 showed `get_nodes` returning a file's previous revision a few seconds after
  PdMount committed a new one. The adapter used the SDK's `enumerate_nodes`, which answers from the SDK's entity cache;
  revision commits and other devices' changes do not update that cache (DESIGN F14). Own revisions therefore cost a
  listing each (not recognized as echoes), and, worse, another device's change to a node in that cache would match the
  tree's stale remote state and be skipped as an echo. The fake has no such cache, so the tests could not show it.
- **Decision:** `get_nodes` calls the fork's `get_nodes_uncached` (P13, amended 2026-10-21): it evicts the links' cache
  entries and fetches them again from the API with one `get_details` request per 50 links, one batch after the other, as
  the C# SDK's batch loader does (the JS SDK sends up to 100). The fresh nodes also refresh the cache for later SDK calls.
- **Amendment (2026-10-21):** the first version called `get_node_uncached` once per link: one request per node, 437
  during the calibration slice, most of them for our own creations. A check against the official SDK's operational
  requirements (`cs/README.md:38-47`: caching, event-based sync, parallelism limits, backoff) found no rule broken (the
  calls follow events, run one at a time and go through P12's backoff; evicting an entry an event names is what the JS
  SDK does, which marks it stale, `cs/client/js/src/internal/nodes/events.ts:31-46`), but the requirement to match the
  SDK's safety guarantees covers request volume, and both official SDKs batch these requests. The user asked for the
  batched patch before the real-account matrix.
- **Alternatives:**
  - `get_node_uncached` per link (the first version): correct, but one request per node.
  - `NodeBatchLoader` (50 links per call): it decrypts with the parent folder key from the secret cache (degraded when
    that is missing) and leaves the stale cache entries in place for later SDK calls.
  - Fetch lazily, when a node is next read, as the JS SDK does: a mount must know at once whether a change touches its
    tree.
  - Keep `enumerate_nodes`: misses other devices' changes.
- **Consequences:**
  - One API request per 50 changed nodes that the tree knows or that are in a listed directory, per page (30 s).
  - Not yet measured on the real account: the batched requests have only offline tests in the fork; the real-account
    matrix exercises them.
  - Verified on the real account: an overwrite's event is an echo again (no listing). Event kinds seen there: Create for
    new nodes, UpdateMetadata for a new revision, UpdateContent for a rename and for a trash (DESIGN §9.3).
  - The journal's conflict check (`same_content`) reads fresh state as well.
- **Refs:** DESIGN.md F14, §9.3; docs/sdk-patches.md P13; crates/pdmount-backend/src/proton.rs (`get_nodes`); D033, D034.

## D036 — Agents may empty the real account's trash when storage runs low during real-account tests
- **Status:** accepted
- **Decided by:** user
- **Date:** 2026-10-21
- **Context:** Every real-account scenario run deletes the previous run's area under `pdmount-test/`, and deleted nodes
  go to the trash, which presumably counts against the quota (the dataset about
  2.3 GiB). The full real-account matrix (60 runs) may fill it. Emptying the trash is account-wide, outside what D028
  allows.
- **Decision:** An exception to D028: during real-account tests, agents may empty the account's whole trash when storage
  is low: less than 1 GiB free before a run, or a run failed because the quota was full. Nothing else changes: writes
  stay under `pdmount-test/`, `pdmount-m1/` stays read-only.
- **Alternatives:** Delete from the trash only nodes that came from `pdmount-test/` (safer for other trashed items, but
  needs their origin; not asked for). Stop the matrix and ask the user each time.
- **Consequences:**
  - Anything else in the account's trash is deleted for good as well.
  - `pdmount trash empty --yes` (DESIGN.md §14.2) calls the SDK's `empty_trash`; it is refused while the profile is
    mounted. `tests/scenarios/run-all.sh` checks `pdmount status` before each real-account run and, below
    `TRASH_MIN_FREE` (1 GiB), unmounts and empties the trash. A run that fails on the quota stops the matrix
    (`STOP_ON=fail`); the agent empties the trash by hand then.
  - Each emptying is printed in the matrix output, with the storage before and after.
- **Refs:** D028, D030; TODO.md (M2.8 real-account matrix).

## D037 — Visibility SLO for remote changes: `sync.poll_interval` + 10 s
- **Status:** accepted
- **Decided by:** user
- **Date:** 2026-10-22
- **Context:** DESIGN.md §16 asked for remote changes to be visible within 30 s for M3. D033 fixes the event poll
  interval at a minimum of 30 s, as the official clients do, so the worst case is 30 s plus the `get_nodes` fetch, the
  listings and the D027 pacing: a hard 30 s bound cannot be met without polling faster than the official clients.
- **Decision:** The M3 exit criterion becomes "remote changes visible within `sync.poll_interval` + 10 s" (40 s at the
  default). D033 stays unchanged.
- **Alternatives:** A median of 30 s with a maximum of 45 s (needs many samples per run). A hard 30 s (needs polling
  below 30 s, against D033).
- **Consequences:** Every `second`-mode scenario run measures the delay with a marker file and fails above the bound
  (docs/plans/m3.md M3.18). E1's limit (75 s) is tightened to 40 s in M3.18. A conflicting file that stays open for
  writing surfaces only at close or at `upload.max_dirty_age`; the SLO covers visibility, not that case.
- **Refs:** DESIGN.md §9.2, §16; D033; docs/plans/m3.md M3.18.

## D038 — Fork patch P14: rename/move with a caller-supplied original name hash
- **Status:** accepted
- **Decided by:** user
- **Date:** 2026-10-22
- **Context:** Rename/rename detection (DESIGN.md §10.5) relies on the server's `OriginalHash` precondition. The SDK's
  `rename_node`/`move_node` compute that hash from the SDK's entity cache, which is often stale (F14, D035), and the
  Proton adapter cannot pass the hash it knows: `convert_node` leaves `name_hash` empty and `rename` ignores `expected`.
  So the precondition protects nothing on the real account, and a stale cache can even reject our own renames.
- **Decision:** A new fork patch P14 lets the caller supply the original name hash to `rename_node`/`move_node` and
  exposes a decoded node's name hash. The adapter fills `RemoteState.name_hash` from fresh node fetches and passes it.
  Each rename also gets a fresh pre-check (`get_nodes`) before it is sent.
- **Alternatives:** A pre-check only, without an SDK change: leaves a race window, as with trash, and keeps the stale
  cache hash in the request.
- **Consequences:** One more fork patch to carry and upstream later (D023). One extra `get_nodes` per rename or move.
  Whether the server enforces `OriginalHash` at all is verified in M3.1 (TODO.md Q1, Q6); if it does not, only the
  pre-check remains and DESIGN.md §17 gets a "rename race" risk.
- **Refs:** DESIGN.md §10.4, §10.5; docs/sdk-patches.md (P14, added in M3.1); TODO.md Q1, Q6; docs/plans/m3.md M3.1,
  M3.11, M3.12.

## D039 — M3 is implemented task by task from `docs/plans/m3.md`, one commit per task
- **Status:** accepted
- **Decided by:** user
- **Date:** 2026-10-22
- **Context:** The user plans M3 with Opus, has Sonnet implement it over several sessions, and reviews the result with
  Opus. The plan must therefore be detailed and self-contained, and the review needs small, separable changes.
- **Decision:**
  - The M3 task specifications live in `docs/plans/m3.md`; status and checkboxes stay in TODO.md.
  - Each implementing session does one task. It commits after `scripts/ci.sh` passes, one commit per task, message
    `M3.n: <title>`, never pushes. This is an exception to AGENTS.md's "commit only when the user asks" for M3.
  - Deviations from the plan are recorded in the commit body and in TODO.md Handoff notes.
  - M3.1 (the P14 patch and the Q1/Q6 check on the real API) and M3.21 (exit check) are done by Opus; real-account
    runs stay user-gated (D030).
  - The M3.0 commit is tagged `m3-start`, so the final review can diff `m3-start..HEAD`.
- **Alternatives:** Leave changes uncommitted until the user reviews them (one large diff to review).
- **Consequences:** The plan's decisions D040–D043 are created as `proposed` by the tasks that implement them, for the
  user's review.
- **Refs:** docs/plans/m3.md; AGENTS.md (Git); TODO.md (M3 tasks).

## D040 — Conflict engine rules: layers, lazy name clashes, minimal-chain resurrection, listing-time rename/rename
- **Status:** accepted
  Accepted by user 2026-10-26.
- **Decided by:** claude
- **Date:** 2026-10-22
- **Context:** M3 replaces the M2 stopgaps (a remote delete resurrected the whole subtree, rename conflicts retried until
  local intent won, a remote node on a locally used name was hidden for good) with the policy of DESIGN.md §10.5. The
  plan (docs/plans/m3.md) fixes the rules; M3.8 implements the listing side, M3.9–M3.11 the upload and namespace side
  (this entry is extended by them).
- **Decision (listing side, M3.8):**
  - **Detection compares revisions and placements, never event kinds.** The listing is the one place remote changes
    meet local state (`Tree::apply_listing_at`), so events, refreshes and plain listings behave alike.
  - **Layer 1:** a listing that shows another revision for a file with unuploaded content sets `remote_newer`
    (persisted) and sends a notice with id 0 (not registered yet; the upload registers the conflict).
  - **Remote delete, local change:** the listing only reports the node as `vanished`. The engine asks the server
    (`get_nodes` for the node and its remote parents up to the root: a child of a trashed folder is not reported trashed
    itself) and resurrects the *minimal chain* (pending items plus the folders that lead to them) below the highest removed
    ancestor that holds the node locally. A node that only moved follows at the listing of its new folder. A clean local
    rename loses against the remote delete (`RemoteDeleteLocalRename`).
  - **Local delete, remote edit:** a tombstone whose remote file shows another revision is unbound; the link returns as a
    new inode (`LocalDeleteRemoteEdit`, state open). The tombstone is not trashed.
  - **Rename/rename:** the first committer wins. A node with a pending local rename whose remote placement changed too
    follows the remote (`RenameRename`); the executor never gets to retry the local name.
  - **Name clashes are lazy.** A remote node whose name a local node holds is not shown, and the local node is reported as
    a clash, so identical content never yields (D016). The exception is a `.pdignore`d (`local_only`) occupant, which
    gives way at once (`NameTaken`). A remote rename onto a used name detaches a clean node (it returns when the name is
    free) and turns a node with local changes into a new local node.
  - Conflict kinds and their initial states are those of `conflicts.rs` (`ConflictKind::initial_state`).
- **Decision (upload side, M3.9):**
  - **The pre-check runs before the snapshot** (one `get_nodes`, no materialization in the common case). The base is the
    staging's `base_rev`, else the node's remote revision, because a listing may have advanced the node's remote state
    while the staging still builds on the old revision.
  - **A metadata-only race is not a conflict:** a node whose only pending changes are mode or mtime adopts another
    client's new content (`readopt_remote_content`) and uploads its metadata revision on top. This is what makes official-app
    edits (M7) safe: they carry no mode and often no SHA-1.
  - **Identical content never conflicts** (D016), also when the remote revision claims no SHA-1: files of at most 16 MiB
    (`IDENTICAL_PROBE_MAX`) with the same size are compared by reading the remote revision. Above that limit without a
    claimed SHA-1 a conflict copy is made.
  - **Edit/edit** (`split_edit_conflict`): the dirty inode keeps its inode number, open descriptors and content under the
    conflict name and uploads as a new file; the server's version becomes a new inode at the original name. The staging's
    `base_rev` is cleared.
  - **A known conflict is settled promptly:** `remote_newer` bypasses the upload debounce (not an open writer).
  - **Layer 3:** `RevisionConflict` with the node still at the base revision is another client's open draft (retry); any
    other fresh revision takes the same decision as the pre-check. `NotFound` means the file is gone.
  - **Commit guard:** an upload whose node is unbound meanwhile is not recorded and plans again (`UploadError::Replan`),
    so the content survives as a new file.
  - **Emptied trash:** content that exists only in a trashed revision that is gone parks with "content only in the emptied
    trash" instead of retrying.
- **Decision (create/create, M3.10):**
  - **One resolver for files and folders** (`name_clash` in `journal.rs`), called when `upload_new` or `create_folder`
    answers `AlreadyExists`. It replaces the crash-adoption check (same SHA-1 only), which is the "identical file" row
    now and therefore also covers a holder that claims no SHA-1.
  - **Identical files bind, they do not yield** (D016): the node takes the remote file's link and the staging builds on its
    revision; the next plan finds nothing to upload except a differing mode or mtime. Folders always merge.
  - **Everything else yields at the executor:** the local node takes its conflict name before the remote node is shown, so
    the first committer keeps the name (§10.5). The parent is listed in the same step; a failing listing is retried, the
    conflict is recorded either way.
  - **A parent that vanished** during the create (`NotFound`) goes through `resolve_vanished`, like a vanished file.
- **Decision (namespace executor, M3.11):**
  - **Every rename or move looks at the node first** (`get_nodes`, fresh) and carries the hash just read as its
    precondition; a stale hash plan (`expected` of the plan) is only a fallback when the backend reports no hash. Local
    intent no longer wins: a node the remote moved or renamed meanwhile follows the remote placement (`RenameRename`);
    when it cannot follow, the local placement is kept and pushed. Only the placement and hash of the remote state are
    refreshed, never its content fields, so a newer revision is still found by the upload pre-check (same reason as
    `with_content_of`, M3.9).
  - **`InvalidMove`** keeps the server's placement (`MoveCycle`) and parks when the inode cannot follow. **`AlreadyExists`**
    on a rename makes our node yield (`NameTaken`), the same helper as create/create (`yield_to_remote`).
  - **Trash looks first, per batch** (one `get_nodes`): a file with another active revision, or marked `remote_newer`,
    is kept (`LocalDeleteRemoteEdit`); a folder is listed uncached and kept when it holds a child unknown to the tree or
    a child file that changed (`LocalDeleteRemoteChildren`, detail = the count); a child that is attached elsewhere
    (moved out locally, not yet remotely) makes the folder wait with a transient error. Child folders that are
    tombstones count as unchanged (their own entries look at them): a remote child added below a deleted grandchild folder
    can still go to the trash with it, recoverably.
  - **Kept nodes come back through a listing** the executor awaits (not spawned) so the conflict record can name the new
    inode (`other_ino`).
- **Alternatives:** Resurrect whole subtrees (M2): uploads clean files again and may recreate folders the user deleted
  elsewhere. Resolve at upload time only: leaves the tree showing a state the server no longer has until the upload.
- **Consequences:** A listing may now start a `get_nodes` round (`resolve_vanished`) after applying. Nodes flagged
  `vanished` keep their old remote state until it is resolved, so a failed resolution is retried by the next listing.
- **Refs:** DESIGN.md §9.3, §10.5; docs/plans/m3.md M3.8; `vfs.rs` `apply_listing_at`, `engine.rs` `resolve_vanished`.

## D043 — No revision post-check: a version restore that lands during our upload is overwritten
- **Status:** accepted
- **Decided by:** user
- **Date:** 2026-10-22
- **Context:** The M3.1 probe (docs/plans/m3.md, "M3.1 outcomes") found that the server checks `CurrentRevisionID` only
  when a revision draft is created, not at commit. Revision drafts are exclusive per file: while one client's draft is
  open, another client's draft creation is refused (HTTP 409, code 2500, `Details.ConflictDraftClientUID`), so two
  clients' uploads cannot interleave. A version restore by another client (an asynchronous operation, answered with code
  1002) did land while our draft was open, and our commit then replaced it; the restored revision stayed in the history.
  The plan's branch B for this case was a post-check (M3.13): list the file's revisions after each commit and keep any
  foreign revision between the base and ours as a conflict copy.
- **Decision:** No post-check; M3.13 is skipped. The restore race is recorded as a DESIGN.md §17 risk.
- **Alternatives:** M3.13 as planned: one more API call (the revision list) per upload, to catch a race that needs a
  restore of the same file during the seconds of an upload.
- **Consequences:** A restore made on another device during our upload of the same file is silently superseded by our
  revision; it can be restored again from the history. Layer 3 of §10.4 is the draft refusal (`RevisionConflict`).
- **Refs:** DESIGN.md §10.4, §17; TODO.md Q1, Q6; docs/plans/m3.md (M3.1 outcomes, M3.13).

## D044 — Fork patch P15: read the trash time of v2 link details
- **Status:** accepted
- **Decided by:** user
- **Date:** 2026-10-22
- **Context:** The M3.1 probe found that `get_nodes` reports a just-trashed node as not trashed, also a minute later.
  The SDK fills its link DTO from `POST v2/volumes/{id}/links`, whose `LinkDto` names the trash timestamp `TrashTime`
  (`cs/client/js/src/internal/apiService/driveTypes.ts`); the SDK reads `Trashed`, the v1 name, so `trash_time` is always
  empty. Consequence in PdMount: an event for a node trashed on another device looked like an echo of a known node
  (`Tree::knows_remote`) and was ignored.
- **Decision:** Fork patch P15 reads `TrashTime` and keeps `Trashed` as an alias. Pinned in PdMount.
- **Alternatives:** Work around it in the adapter (e.g. treat `State == 2` as trashed): the SDK does not expose the
  state, and the fix belongs in the DTO.
- **Consequences:** Behaviour change for SDK callers: trashed nodes now report their trash time (docs/sdk-patches.md,
  Behaviour changes). Remote trashes reach PdMount through events as designed (§9.3). Children of a trashed folder are
  still reported as not trashed, with the trashed folder as parent; detecting them needs the parent chain (M3.8
  `resolve_vanished`).
- **Refs:** DESIGN.md F16, §9.3; docs/sdk-patches.md P15; docs/plans/m3.md (M3.1 outcomes).

## D045 — Conflicts are resolved through the namespace operations; xattrs come from RAM
- **Status:** accepted
  Accepted by user 2026-10-26.
- **Decided by:** claude
- **Date:** 2026-10-22
- **Context:** M3.14 lets the user settle the conflicts of §10.5 (`conflicts resolve`) and see sync state in the file
  manager (`user.pdmount.*`). Resolving could edit the registry and the tree directly, or reuse what the kernel's own
  operations do.
- **Decision:**
  - `resolve keep-local` renames the conflict copy over the other copy, `keep-remote` removes the conflict copy once it
    is uploaded, and the "local delete, remote edit" kind removes the file that came back. They are `Engine::rename`,
    `unlink` and `rmdir` with current paths, so access rules, the journal, the trash and the atomic-save adoption
    (§7.6) apply as for any user action. Only the registry state is set by the resolver (durably).
  - Notices (`rename_rename`, `move_cycle`, …) are dismissed with `keep-both`; `keep-local` and `keep-remote` are
    refused for them, because there is nothing to choose.
  - The xattrs are computed from the tree, the journal and the registry in RAM (hot path): no network, no store read.
    `state` is the first match of `local-only`, `conflict`, `remote-newer`, `parked`, `uploading`, `pending`, `clean`.
  - `status` counts only conflicts in state `open`; notices show in `conflicts list`.
- **Alternatives:** A resolver that edits the tree and the remote state itself (a second path to the same results, with its
  own races); xattrs read from the store (a disk read on the hot path).
- **Consequences:** `keep-remote` waits up to 120 s for the upload of the copy, so the content is in the trash and not
  lost; it fails with a timeout when the upload cannot finish (offline), and nothing has changed then. A notice stays in
  `conflicts list` until it is dismissed.
- **Refs:** DESIGN.md §8.7, §10.8, §14.2; docs/plans/m3.md M3.14; `crates/pdmount-core/src/engine/surface.rs`.

## D041 — Connectivity state machine: one shared failure count, a probe, fast-failing reads, offline mount
- **Status:** accepted
  Accepted by user 2026-10-26.
- **Decided by:** claude
- **Date:** 2026-10-22
- **Context:** §9.6 said the daemon goes offline after 3 connection failures "of the sync engine or the executor", but each
  component saw only its own failures, nothing probed, and a mount without a connection failed. M3.2 made connection
  errors `Offline`, so a state can be built on them.
- **Decision:**
  - One `Health` per engine (`Online`, `Offline`, `NeedsLogin`), fed by a `Monitored` decorator around the backend: every
    call's result is observed, so reads, the journal, the event task and the probe share one count. 3 `Offline` results in
    a row mean `Offline`; any other answer from the server (errors included) means `Online`; `Unauthorized` means
    `NeedsLogin`, which only a login ends (M3.16). While `NeedsLogin`, the decorator answers `Unauthorized` without a request.
  - While not `Online`, a task probes `storage_info` every `sync.probe_interval` (30 s, at least 10 s), in a background
    turn; a rejected session is not probed, except by `Health::kick` after a login (M3.16), whose first answer ends `NeedsLogin`.
  - The journal and the event task wait for `Online`. On the transition the journal retries every entry that is not parked
    at once and the event task polls at once. `Offline` and `Unauthorized` failures do not count as attempts of an entry.
  - FUSE slow-path deadlines drop to `timeouts.offline_read` (2 s) when the state is not `Online`, so uncached reads and the
    first listing of a directory fail with EIO instead of waiting 20 to 60 s. Local `fsync` keeps its deadline.
  - A mount without a connection works when the profile was mounted online before: the volume and root are kept in
    `proton-root.json`, the session is built from the stored tokens, and the daemon starts `Offline`.
- **Alternatives:** A failure count per component (disagree, slow to notice); probing with a Drive listing (heavier than the
  quota request); failing reads only by timeout (a user waits 20 s per `ls`).
- **Consequences:** Reads of cached data and all writes work offline; the queue is bounded only by disk. A slow link that
  times out is `Transient`, not `Offline`, so it never switches the state. A session mounted offline has not been checked:
  if its access token has expired by the time the connection returns, the first request answers 401, which M3.16's
  reactive refresh handles; until then it would show as `NeedsLogin`.
- **Refs:** DESIGN.md §9.2, §9.6, §13.1, Appendix B; docs/plans/m3.md M3.15; `crates/pdmount-core/src/health.rs`.

## D042 — `needs_login` keeps the daemon up; the daemon runs the web login; `notify-send` notifications
- **Status:** accepted
  Accepted by user 2026-10-26.
- **Decided by:** claude
- **Date:** 2026-10-22
- **Context:** A rejected session (expired refresh token, revoked session) used to end the daemon or, at mount, refuse to
  start, and an expired access token was never refreshed: the drive client sends the token but does not renew it. Work
  queued offline (D041) must not be lost to a login prompt, and a service has no terminal to run `pdmount login` in.
- **Decision:**
  - **Reactive refresh in the adapter.** Every `Backend` call runs through `with_reauth`: on `Unauthorized` (HTTP 401, the
    body `Code` 401 of M3.2, a block transfer's 401) it asks the credential for a new token once (single-flight, keyed by the
    rejected token) and repeats the call. A second rejection is `Unauthorized`; a failed refresh is classified like any SDK
    error (refresh token expired: `Unauthorized`, anything else `Transient`). Uploads reopen their source on the repeat.
  - **Rotation stored at once.** The daemon subscribes to the credential's token and expiry channels: each rotation is
    stored in a task of its own (a keyring prompt can take 120 s); an expired refresh token sets `NeedsLogin`. Every 30
    minutes while online it also checks the session. The 60 s timer stays as a fallback.
  - **`needs_login` keeps the mount.** A mount whose profile has local state and a known root starts with the stored
    (rejected) credentials in `NeedsLogin`; local reads of cached data and all writes work, the queue waits (D041). Without
    local state it fails as before.
  - **The daemon runs the login.** When the state becomes `NeedsLogin`, and on `pdmount login` while a daemon answers, it
    starts the web login once at a time: link and code go to the log (WARN), `pdmount status` and a desktop notification.
    The link lasts 10 minutes; `pdmount login` starts a new one. The new session replaces the backend's connection
    (`replace_session`) only for the account that is mounted; another account is ended again and reported. `pdmount login`
    with a daemon prints the link and polls `status` until the daemon's login is over.
  - **`mount` without a session** runs the foreground login first; `--no-login` fails instead.
  - **Notifications** are `notify-send --app-name=PdMount`, spawned without waiting, only with a session bus and the tool on
    `PATH`; at most one per 30 s, conflict notices in between are announced together ("N new conflicts").
- **Alternatives:** Exiting the daemon on a rejected session (loses the offline queue's context, forces a restart);
  the login only through the CLI (useless for services); a notification library over D-Bus (a dependency for one message).
- **Consequences:** A session is stored under the same D029 modes as `pdmount login`. A second 401 right after a refresh is
  taken as a rejected session, so a server that revokes tokens without a refresh token error ends in `needs_login` too. A
  login that is never completed leaves the mount in `needs_login` with no further notifications until `pdmount login`.
- **Refs:** DESIGN.md §12.2, §12.3, §13.1, §14.2; docs/plans/m3.md M3.16; `crates/pdmount-backend/src/proton.rs`
  (`with_reauth`), `crates/pdmount/src/{auth,notify,daemon,session}.rs`.

## D046 — Fixes from the two-device property test: trashed folders, freed clash names, renames into deleted folders
- **Status:** accepted
  Accepted by user 2026-10-26. Superseded in part by D054 (the second bullet and the generation alternative).
- **Decided by:** claude
- **Date:** 2026-10-22
- **Context:** `two_devices_and_an_app_converge` (M3.17, DESIGN.md §15.1) found five faults of the M3 engine within 2000
  cases. All five are in paths the scenario tests do not reach because they need a second device and a particular order.
- **Decision:**
  - **A trashed folder is not a failed event page.** The fake (and the server) answer a listing of a trashed folder with
    NotFound. `apply_events` treated `Backend(NotFound)` as an error, so the page failed, the cursor stayed, and the same
    page failed again for good. It now skips that directory; the pass after the listings (§10.5) settles what it held.
  - **A shared listing is not fresh.** `yield_to_remote` frees a name and lists the folder to show the remote node. The
    listing that found the clash wakes this upload while it is still in flight, so the new request joined it and got a
    result fetched before the name was free. It now lists again (at most three times) until the remote node shows.
  - **A hidden remote node comes back when its occupant leaves.** A local node that holds the name of a remote one keeps it
    out of the tree (D040). When that node is unlinked or renamed away before it uploads, nothing listed the folder again.
    `Node::clashed` (memory only; after a restart it is set for every created, not yet uploaded node) makes
    `remove_entry` and `rename` queue a listing of the folder (`Tree::take_relist`, `Journal::list_later`, run by
    `run_journal`).
  - **A rename into a deleted folder loses, not the node.** `resurrect_minimal` removed every clean node below the deleted
    folder, including a node that was only renamed into it and still lives at its old place on the drive; the local tree
    then lacked a node the drive had. Such nodes (remote parent outside the removed folder, placement pending) first go
    back to their remote placement. A `move_node` step whose call answers NotFound settles the destination folder when the
    moved node is still live, instead of the node.
  - **No hot replan loop.** The step above looped without delay: `vanished` returned `Replan`, the same step was planned
    again at once, with no state change. After 16 `Replan`s in a row an entry backs off like a failing step
    (1 s doubling up to 300 s; `Journal::replanned`; a flat second until the M3.21 review).
- **Alternatives:** Persisting hidden clashes (a table for a rare case; a listing after a restart finds them anyway);
  listing the folder after every unlink of a created file (an API call per temp file of rsync or git, too many); making the
  single-flight listing take a generation (every caller would need one; only this caller needs a fresh result).
- **Consequences:** An unlink of a clashed node costs one extra listing, in the background. After a restart, unlinking a
  created, not yet uploaded node in a listed folder also costs one listing. The replan guard only slows a wrong state down;
  it does not make a wrong state right.
- **Refs:** `crates/pdmount-core/src/{events,journal,vfs,engine}.rs`; `engine/proptests.rs` (`concurrent_regression_cases`).

## D047 — Two-device scenarios: the marker SLO scales with the number of change pages; catch-up barriers; the fake daemon ends `needs_login`
- **Status:** accepted
  Accepted by user 2026-10-26.
- **Decided by:** claude
- **Date:** 2026-10-22
- **Context:** M3.18 runs the existing scenarios with a second device (`second` mode) and adds the M scenarios. D037 sets
  the visibility SLO at `poll_interval` + 10 s (40 s) for a remote change. Measured on the fake with two daemons: a single
  change shows within 26–36 s, but a scenario that makes hundreds of changes (a git repository, 200 rsync'd files) needs more
  than one event page of 50, and the daemon applies a page in about 12 s because listings and lookups are paced (D027); the
  fake's trash purges (events in pages of 50) and the previous scenario's backlog add to it. Separately, nothing ended
  `needs_login` on the fake: no sign-in exists, and `fault clear` did not kick the probe (M3.16 left the kick to the login).
- **Decision:**
  - The marker check keeps D037's 40 s for the first page of changes and adds 15 s per further page of 50 changes the
    scenario made (counted in the fake's change log; the real account gets no allowance until M3.20 has data).
  - Before a `second` run (and before E1) the devices wait for each other's earlier events with a marker each way, so the
    check times the scenario's own changes and A has applied what checks on B wrote (`git status` refreshes the index; A then
    deleting that file would revive it, M4).
  - A fake-backend daemon in `needs_login` kicks the probe every 2 s (`watch_fake_session`), standing in for the user
    signing in again; the first answer after `fake fault clear` ends the state.
  - E1's limit goes from 75 s to D037's 40 s.
- **Alternatives:** A flat 40 s (fails on big scenarios for reasons of pacing, not of the sync); a flat 120 s (hides a
  regression of the poll loop); no check outside the dedicated visibility tests (the plan wants it in every `second` run).
- **Consequences:** `second` runs take as long as the daemons need to digest the changes: the CI set (r1, r3, g3 with 30
  commits, a1) takes about 5 minutes. On the real account the check may need an allowance too; M3.20 decides.
- **Refs:** DESIGN.md §15.1, §15.3; `tests/scenarios/lib.sh` (`second_marker`, `send_marker`, `event_barrier`),
  `crates/pdmount/src/daemon.rs` (`watch_fake_session`).

## D048 — The M3 soak runs 3 h instead of 24 h; the risk is accepted
- **Status:** accepted
- **Decided by:** user
- **Date:** 2026-10-23
- **Context:** The M3 exit criteria (DESIGN.md §16) and task M3.19 asked for a 24 h soak on the fake. The user cut it to
  3 h and accepted the risk.
- **Decision:**
  - The soak runs 3 h (`SOAK_DURATION=10800`), still on the fake only (D030).
  - Its loops are rescheduled so that each fault recurs within 3 h: network flaps every 40 ± 10 min (was 2 h ± 20 min),
    token expiry every 60 min from minute 30 (was every 6 h, which a 3 h run would never reach).
  - The RSS check compares the last third of the run with the window from 1/6 to 1/3 of the run (at 3 h: the last hour
    against minutes 30–60), instead of fixed hours.
- **Alternatives:** The 24 h run as planned: not wanted by the user.
- **Consequences:** Faults that take many hours to show can pass the M3 exit unseen: slow memory or disk growth (an RSS
  increase below 25 % over the run, store or cache growth), rare races that need more loop iterations, and timers longer
  than the run. A longer run needs only a larger `SOAK_DURATION`.
- **Refs:** DESIGN.md §15.1, §16, §17; docs/plans/m3.md (M3.19); TODO.md M3.19.

## D049 — RAM cache at working-set size; block memory reserved at startup in a pool, used by the RAM cache, `seed` and reads
- **Status:** accepted
- **Decided by:** user (the three points); the sizes, classes and fallback rules are Claude's (docs/plans/m3.md M3.19.1–M3.19.3)
- **Date:** 2026-10-24
- **Context:** The M3.19 soak failed its RSS check (c). heaptrack found no leak and little live-heap growth. The growth
  is freed memory that glibc malloc keeps: with `glibc.malloc.mmap_threshold=1048576:glibc.malloc.arena_max=4`, RSS
  halved and device B stayed flat.
  - The churn comes from 4 MiB block buffers: one per `BlockCache::seed` (even for tiny files), two per disk-cache hit,
    and one per assembled reply.
  - The 256 MiB RAM cache also duplicated what the kernel page cache already holds.
- **Decision:**
  1. The RAM cache is sized to the working set of blocks being read (default 32 MiB, was 256 MiB). Long-term caching
     is left to the kernel page cache. This is also gentler on workstations with less memory.
  2. RAM-cache block memory is reserved at startup in a pool of fixed size classes and stays resident. The RAM cache and
     the `seed` buffer use it, and after startup no block-sized allocation is made for them. How many allocations happen
     at startup does not matter; that they all happen there, deterministically, does.
  3. Reads use the pool too, where possible: disk hits are read and decrypted in place in a slot, downloads are copied
     into a slot, and replies are sent from the slot without assembling.
- **Alternatives:**
  - Tune glibc malloc only (`mallopt` at startup): it halves RSS, but the allocations stay, and every reused block
    costs page faults.
  - jemalloc or mimalloc: a new dependency, and it does not bound the cache's memory.
  - Change check (c) to measure live heap: it would hide the problem instead of fixing it.
  - Keep 256 MiB: duplicates the page cache and costs fixed memory.
- **Consequences:**
  - The daemon's RSS includes `ram_bytes` from the start, and a smaller default lowers it.
  - A hot-path read whose class has no free slot falls back to a heap allocation, counted in `cache_pool_fallbacks`.
  - Proton downloads still allocate inside the SDK; avoiding that needs a fork patch (M4 option).
  - Reads of dirty inodes from staging are unchanged.
- **Refs:** DESIGN.md §3.4, §6.2, App. B (updated by the tasks); docs/plans/m3.md (M3.19 outcome, M3.19.1–M3.19.3);
  TODO.md M3.19.

## D050 — The visibility SLO (D037) is timed from the writer's commit, not its local write
- **Status:** accepted
- **Decided by:** user
- **Date:** 2026-10-25
- **Context:** M3.20's first real-account run (m1, live) failed the 40 s marker check: B's marker showed on A after about
  39 s. The time splits into B's upload debounce and upload (about 6 s), A's wait for the next poll (30 s after the end of
  the previous poll, as the official event manager does, `cs/client/js/src/internal/events/eventManager.ts:114-123`) and
  A's paced poll, `get_nodes` and listing (about 9 s). D037 budgeted only the receiving side; `send_marker` started the
  clock at the local write, although docs/plans/m3.md M3.18 already said to wait for the writer's queue first.
- **Decision:** The marker clock starts when the writer's queue is empty (`Queue: 0 pending`; debounced files count as
  pending), i.e. when the change is committed on the server. The bound stays `sync.poll_interval` + 10 s.
- **Alternatives:** Raise the bound to `poll_interval` + 20 s, timed from the local write. Both (+15 s, from the commit).
  Poll at a fixed rate instead of a fixed delay (more often than the official clients; against D033).
- **Consequences:** The upload debounce is outside the SLO. A slow poll on the receiver (listings paced by D027) can still
  exceed it; that would show in the M3.20 runs.
- **Refs:** D037, D033, D027; DESIGN.md §15.3; `tests/scenarios/lib.sh` `send_marker`.

## D051 — M3 exit: the M scenarios run on the real account only where no fault injection is needed
- **Status:** accepted
- **Decided by:** user (confirmed at M3.20)
- **Date:** 2026-10-25
- **Context:** DESIGN.md §16 asked for "M1–M7 … on two real profiles", while §15.3 marks only some M cases as real. M2
  needs a pause at the commit, M4 and M5 an offline device, and the folder and app parts of M7 an offline device and an
  official-app writer; a daemon on the real account cannot be given those.
- **Decision:** The M3 exit runs M1–M7 on the fake (M2 as the engine test `m2_edit_edit_interleaved_at_commit`) and M1,
  M3, M6 and the file part of M7 on two real profiles of one account.
- **Alternatives:** Fault injection against the real account (a proxy that drops connections): more machinery and load on
  the service for cases the fake already models.
- **Consequences:** The real-account evidence for M2, M4, M5 and the M7 folder and app parts is indirect: the server
  behaviour they depend on was verified in M3.1 (§10.4).
- **Refs:** DESIGN.md §15.3, §16; docs/plans/m3.md M3.20; TODO.md M3.20.

## D052 — A folder that loses its name to a remote rename keeps only its pending items
- **Status:** accepted
  Accepted by user 2026-10-26.
- **Decided by:** claude
- **Date:** 2026-10-25
- **Context:** The M3.21 run of `two_devices_and_an_app_converge` at 10 000 cases found a divergence: device B had a
  pending file in folder `d` and a pending file `a`; device A renamed `d` to `a`. B's listing applied the rename onto its
  used name (`Tree::lose_name`, D040): `d` has pending items, so it became a new local folder, and its whole subtree went
  with it, clean child `b` included. `b` stayed bound to its link, whose remote parent is the renamed folder, and nothing
  moved it back, so B showed `/d/b` while the drive had `/a/b`. Replay: `concurrent_regression_cases`.
- **Decision:** Such a folder goes through the same minimal re-creation as a folder trashed remotely
  (`Tree::keep_pending`, shared with `resurrect_minimal`): pending items and the folders that lead to them become nodes to
  create again, and the clean rest is removed locally; it is still in the remote folder and returns with it once the name
  is free. Unlike the trash case, a node moved into the folder from outside keeps its link and its pending move, which
  now goes to the new folder; its subtree stays as it is (still bound, in the moved node's remote folder). Renames inside
  the folder that are the only pending change lose, as for a clean folder, and each is recorded as `RenameRename`.
- **Alternatives:** Keep the whole subtree and move the clean children on the drive into the new folder (takes the other
  device's files out of the folder it renamed); move the clean children back under the remote folder's later inode (the
  folder is not in the tree until the name is free).
- **Consequences:** Clean files of such a folder disappear locally until the name is free (B's own pending `a` yields when
  it uploads, §10.6), then return under the folder's new name. A pending content edit of a clean-bound file is uploaded as
  a new file in the new folder, as before.
- **Refs:** DESIGN.md §9.3; `crates/pdmount-core/src/vfs.rs` (`lose_name`, `keep_pending`); D040, D046.

## D053 — keep-remote is refused for a local-only conflict copy
- **Status:** accepted
- **Decided by:** user
- **Date:** 2026-10-26
- **Context:** The pre-M4 review (B9) found that `conflicts resolve keep-remote` on a `name_taken` conflict whose copy is
  local-only (`.pdignore`, §7.7) waited for an upload that never happens, and timed out after 120 s every time.
  keep-remote removes the copy only once it is uploaded, so the removed content stays recoverable from the trash; a
  local-only copy never gets there.
- **Decision:** keep-remote is refused at once for a local-only copy, with a message that names the cause and the two ways
  out: delete the copy by hand, or use keep-both.
- **Alternatives:** Remove the copy directly: the user asked to drop it, but its content would be gone for good, with no
  copy in the trash.
- **Consequences:** Dropping a local-only copy is the user's own `rm`; keep-remote keeps its guarantee that nothing it
  removes is lost.
- **Refs:** DESIGN.md §10.8; `crates/pdmount-core/src/engine/surface.rs` (`resolve_keep_remote`);
  `target/pre-m4-review/report.md` B9.

## D054 — A listing request is answered by a listing that started after it
- **Status:** accepted
- **Decided by:** user
- **Date:** 2026-10-26
- **Context:** Concurrent listings of a folder share one request (`SingleFlight`, §6.4). Joining a listing that is
  already in flight returns data fetched before the call. D046 worked around this for `yield_to_remote` alone, with up
  to three listings. The pre-M4 review (B2) found the same problem in the event stream: `apply_events` can join a
  listing started before the event, apply it, and store the cursor past the event, so the change stays invisible until
  the folder is listed again. `revalidate`, `keep_remote`, the relist after a freed clash name, the merged-folder relist
  and the relist in `resolve_vanished` all need a listing that started after they asked, too.
- **Decision:**
  - Each listing takes a number when it starts (`listing_epoch`, an engine counter). `Engine::list(dir)` notes the
    counter before it joins. If the listing it joined started earlier, it waits for that listing and lists again. At most
    two rounds: the second listing has started after the call.
  - `ensure_listed` keeps joining any listing (it only needs the folder listed once). It is the only caller that does.
  - `yield_to_remote` lists once.
  - Unchanged: concurrent fresh requests still share one listing; a request costs one extra listing only when it joined
    one that started before it.
- **Alternatives:**
  - A fresh/any flag per caller: every caller has to choose, and all but one choose fresh.
  - A generation argument: callers would have to know which generation they need.
  - No sharing for event and refresh listings: each event page of a busy folder would list it again even when a fresh
    listing is already running (D027 pacing, 429 risk).
- **Consequences:**
  - An event for a folder whose listing is in flight costs one more listing of that folder (the joined one, then its
    own).
  - The cursor moves past an event only after a listing that started after the event was applied.
  - Supersedes D046's second bullet and its rejected alternative about a generation.
- **Refs:** DESIGN.md §9.3; `crates/pdmount-core/src/{engine,events,journal,singleflight}.rs`; D046;
  `target/pre-m4-review/report.md` B2.

## D055 — A yielded name is recorded before the listing; open conflicts with a missing other copy relist their folder at start
- **Status:** accepted
- **Decided by:** user
- **Date:** 2026-10-26
- **Context:** The 10 000-case run of `two_devices_and_an_app_converge` during fix group G3 failed: device B did not
  show the app's `/b` although the drive had it. It reproduces on the code before G3 (the M3.21 run's lost failure was
  probably this). B's own `/b` yielded its name (`yield_to_remote`, §10.6); the rename was persisted, then the listing
  that shows the remote node ran. A restart aborted the step during that listing, before the conflict was recorded.
  The event that announced the app's `/b` had been applied while the name was taken, and a mount with a cursor does not
  list folders again at start, so nothing showed `/b` until a refresh. A failed listing after a yield was not retried
  either.
- **Decision:**
  - `yield_to_remote` records the conflict right after the rename and before the listing; once the listing shows the
    remote node, the record names it as the other copy (`Conflicts::set_other_ino`).
  - At start (`Engine::start_background`), every open `CreateCreate` or `NameTaken` conflict whose other copy is not in
    the tree (`other_copy_in` finds neither `other_ino` nor the link) has its folder listed again, if that folder is
    listed (`Journal::list_later`).
  - A listing after a yield that fails goes to the relist queue.
- **Alternatives:**
  - Persisting the pending relists in a table: a second durable trace of what the open conflict already says.
  - Listing every listed folder at start: the D027 budget for one rare case.
- **Consequences:**
  - A start costs one listing per folder holding such a conflict; once a listing has shown the other copy, the record
    names it and the folder is not listed again.
  - A crash before the group commit still loses the record (§17), and with it this trace.
  - Regression case in `concurrent_regression_cases`.
- **Refs:** DESIGN.md §10.6; `crates/pdmount-core/src/{journal,engine,engine/surface,conflicts}.rs`;
  `target/g3/proptests.out` (the failing run).
