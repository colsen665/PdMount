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
| D003 | Scope: My Files only | accepted (v1 scope; extended by D078) | user | 2026-10-19 |
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
| D020 | No AutoUnmount without `user_allow_other` | accepted (systemd part superseded by D059) | claude | 2026-10-19 |
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
| D042 | `needs_login` keeps the daemon up; the daemon runs the web login; `notify-send` notifications | accepted (link-in-log part superseded by D071) | claude | 2026-10-22 |
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
| D056 | M4 is implemented from `docs/plans/m4.md`, one commit per task, several tasks per session allowed | accepted | user | 2026-10-27 |
| D057 | How the M4 exit criteria are measured: fake network profile, benchmark definitions, SLOs in the daemon | accepted | claude | 2026-10-28 |
| D058 | Upload parallelism with P7: 4 files, 5 blocks over all of them; fork patch P18 | accepted | claude | 2026-10-28 |
| D059 | An XDG autostart entry instead of a systemd user unit | accepted (no-services part superseded by D074) | user | 2026-10-27 |
| D060 | Pins: model, fetching, eviction and the cap | accepted | claude | 2026-10-30 |
| D061 | Queue tooling: inode ids, user parks in RAM, `drop` = back to the remote state | accepted | claude | 2026-10-30 |
| D062 | Shutdown waits for running steps only; opens are not refused | accepted | claude | 2026-10-31 |
| D063 | Staging files are opened on demand through a descriptor LRU | accepted | claude | 2026-10-28 |
| D064 | The kernel writeback cache stays off by default, behind `kernel.writeback_cache` | accepted (option part superseded by D083) | user | 2026-10-30 |
| D065 | The untar exit criterion is ≤ 15× local (was 3×; amends §16 and D057) | accepted | user | 2026-10-29 |
| D066 | The `local` fsync SLO is p99 < 30 ms (was 10 ms; amends §5.7 and D057) | accepted | user | 2026-10-29 |
| D067 | A staging file without a base has no sidecar; `local` fsync keeps flushing data and store in turn | accepted | claude | 2026-10-29 |
| D068 | `FUSE_HANDLE_KILLPRIV_V2` is not requested; the kernel keeps asking for `security.capability` | accepted | claude | 2026-10-30 |
| D069 | The relist queue is kept in the store until each listing succeeds | accepted | claude | 2026-10-29 |
| D070 | Security threat model and review method (first review, M4.0.1) | accepted | claude | 2026-10-27 |
| D071 | The sign-in link is never logged or put in a notification (supersedes part of D042) | accepted | claude | 2026-10-27 |
| D072 | Directory pages stay in the kernel cache while the entries are unchanged | accepted | claude | 2026-10-30 |
| D073 | Milestones: M5 user services and flush, M6 conflict notifications, M7 upstream and crypto (was M5) | accepted | user | 2026-10-30 |
| D074 | OpenRC and systemd user services, next to the autostart entry (supersedes part of D059) | accepted | user | 2026-10-30 |
| D075 | SIGTERM flushes the queue for up to `shutdown_grace`, 90 s by default; warning at the next start (widens D062) | accepted | user | 2026-10-30 |
| D076 | A pending marker in `.pdmount/` tells other devices that changes are on their way | accepted | user | 2026-10-30 |
| D077 | Conflict notifications name the file, offer actions, survive a missing desktop, and use D-Bus directly | accepted | user | 2026-10-30 |
| D078 | M5 also maps Proton Drive roots other than My Files (extends D003) | accepted | user | 2026-10-30 |
| D079 | Further roots: one mountpoint per root in one daemon, read-only at first | accepted (engine unit and read-only part amended by D080) | user | 2026-10-30 |
| D080 | Computers and writable shares are read-write through the My Files engine, one engine per tree; Photos stays read-only | accepted | user | 2026-10-30 |
| D081 | Browsing many images: folders in stages, Proton's thumbnails in the local cache, and a bulk-read guard on all trees | accepted | user | 2026-10-30 |
| D082 | `kernel.writeback_cache` applies to the My Files mount only | rejected | claude | 2026-10-30 |
| D083 | The writeback cache is never requested; `kernel.writeback_cache` is removed (supersedes part of D064) | accepted | user | 2026-10-30 |
| D084 | `scripts/ci.sh --quick` is the default before a commit; named tasks and every push need the full run (amends D056) | accepted | user | 2026-10-31 |
| D085 | Account key passphrases live in their own uncapped store; keys that cannot be unlocked mean `needs_login` | accepted (migration part superseded by D088) | claude | 2026-10-31 |
| D086 | The §5.7 SLO row is the median of 5 rounds on a quiet disk; `fsync` p99 < 40 ms (amends D057, D066) | accepted | user | 2026-10-31 |
| D087 | A WAL fsync and a staging fsync take turns, so commits do not stall behind them | accepted | claude | 2026-11-01 |
| D088 | No migration of passphrases stored before D085; such a profile logs in once (amends D085) | accepted | user | 2026-11-01 |

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
- **Refs:** DESIGN.md §5.4.1; outcome of the experiment in D064. Accepted by user 2026-10-20.

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
- **Status:** accepted (systemd part superseded by D059)
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
- **Amendment (2026-10-28, user):** the per-session handoff history moved from TODO.md to `docs/handoff-log.md`, which is
  not read at session start; TODO.md keeps the current state, standing notes and known gaps (it had grown to about
  27k tokens, read in full at every session start).
- **Refs:** AGENTS.md; TODO.md; docs/handoff-log.md.

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
    with P7). Done in M4.7 (D058): 4 files and 5 blocks, through fork patch P18.
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
- **Status:** accepted (link-in-log part superseded by D071)
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

## D056 — M4 is implemented from `docs/plans/m4.md`, one commit per task, several tasks per session allowed
- **Status:** accepted
- **Decided by:** user
- **Date:** 2026-10-27
- **Context:** M4 is planned like M3 (D039): Opus plans and triages, Sonnet implements, Opus reviews. D039 covered M3
  only. The user asked to allow several tasks in one session where the context suits, with the batches recommended up
  front.
- **Decision:**
  - The M4 task specifications live in `docs/plans/m4.md`; status and checkboxes stay in TODO.md.
  - A session does one task or a batch from the plan's "Sessions" table. Each task gets its own commit once
    `scripts/ci.sh` passes, message `M4.n: <title>`; nothing is pushed. This is an exception to AGENTS.md's "commit
    only when the user asks" for M4, as D039 was for M3.
  - Deviations from the plan are recorded in the commit body and in TODO.md Handoff notes.
  - The efficiency items of the pre-M4 review (E1–E19) are measured first: benchmarks and the fake's network profile
    come first, then an Opus triage (M4.5) chooses the fixes. One real-account task (M4.15) checks P7 and Q7, gated
    by the user (D030). git.git for the benchmark comes from kernel.org (D023).
  - The M4.0 commit is tagged `m4-start`, so the final review can diff `m4-start..HEAD`.
  - The plan's decision numbers D057, D058 and D060–D070 are reserved for its tasks.
- **Alternatives:** One task per session (D039's rule): more sessions for small tasks. No commits until review: one
  large diff.
- **Consequences:** Commits stay per task, so the review can still take them one at a time.
- **Refs:** docs/plans/m4.md; AGENTS.md (Git); TODO.md (M4 tasks); D039.

## D057 — How the M4 exit criteria are measured: fake network profile, benchmark definitions, SLOs in the daemon
- **Status:** accepted
- **Decided by:** claude
- **Date:** 2026-10-28
- **Context:** DESIGN.md §16 states the M4 exit criteria in one line each ("`git status` on warm git.git ≤ 2× local
  btrfs", "untar of 5k small files ≤ 3× local", "cached sequential read ≥ 500 MB/s", "`cache gc` holds the cap ±5 %",
  "§5.7 SLOs met", all "over the fake backend with an 80 ms RTT / 20 MB/s profile"). Each needs a dataset, a
  procedure and a number, fixed before the baseline (M4.5), so that later fixes are measured the same way.
- **Decision:**
  - **Network:** the fake's network profile (DESIGN.md §15.1, M4.2) with rtt 80 ms and 20 000 000 B/s each way: one
    RTT per call, one per 150 children or ids, three per upload, transfers on one shared pipe per direction per
    process.
  - **Harness:** `scripts/bench/m4.sh [git|untar|read|slo|gc|all]`: release build with `--features fake-backend`,
    profile `bench`, state and cache in `BENCH_ROOT` (default `~/.cache/pdmount-bench`, the disk: §5.7 is defined
    on SSD), the fake drive in tmpfs (a real backend writes nothing locally), local comparisons in
    `$BENCH_ROOT/local` on the same filesystem. The profile has the default config except `cache.max_bytes = 4GiB`.
    Results go to `target/bench/m4-<date>/report.md` (row, mount, local, ratio or value, limit, PASS/FAIL/SKIP) with
    raw logs; the exit status is 1 if a row fails. Not part of `scripts/ci.sh`.
  - **git:** a mirror of `https://git.kernel.org/pub/scm/git/git.git` (cloned once, its HEAD recorded); local and
    mount each `git clone --no-hardlinks --branch v2.50.0`; the mount's queue drains; `sync -f` after the local clone
    (its writeback slowed the local side's first runs); `git status` twice untimed per side, then 11 timed pairs, local then mount, so a slow spell of the disk hits both sides (M4.6.3; was 5 runs per
    side, one side after the other). `git.log` notes whether the first timed run rewrote the mount's `.git/index`
    (its mtime and `queue_pending` before and after); `git-stats.json` holds the daemon's stats over the timed runs.
    Pass: median(mount) / median(local) ≤ 2.0.
  - **untar:** `scripts/testdata/make-untar-dataset.sh`, a deterministic tar of 5000 files of 0–16 KiB in 100
    directories on 2 levels, mode 0644, fixed mtimes. Three `tar -xf` per side into fresh directories, timed until
    `tar` exits (time to acknowledgement); the mount's queue drains between runs. Pass: median ratio ≤ 15.0 (3.0
    until M4.5, D065).
  - **read:** a deterministic 1 GiB file, read once through the mount (download), then a remount (daemon RAM cold,
    disk cache warm), the page cache dropped (`POSIX_FADV_DONTNEED`), and a sequential read in 1 MiB reads. Pass:
    ≥ 500 MB/s (10^6 bytes).
  - **gc:** `cache.max_bytes = 512MiB`, 6 files of 256 MiB read through the mount while `du -sb` of the block cache
    is sampled every second, then `pdmount cache gc` and one more sample. Pass: every sample ≤ 1.05 × the cap, the
    last ≤ the cap. SKIP while `cache gc` does not exist (M4.9); the samples are still taken.
  - **slo:** `pdmount stats --reset`; latency.sh's workload (4 clients × 1000 iterations of create, write 4 KiB,
    every tenth an fsync, close, chmod, rename, mkdir, rmdir, every second an unlink), the read row's sequential
    read and 5 runs of the git row's `git status`; then `pdmount stats --json | scripts/bench/slo.py`. Pass: p99 per
    histogram key below the §5.7 limit (`getattr`, `lookup`, `readdir`, `readdirplus` 1 ms; `create`, `mkdir`,
    `unlink`, `rmdir`, `rename`, `setattr` 2 ms; `write` 1 ms; `read_ram` 1 ms; `read_disk` 5 ms; `fsync` 30 ms, 10 ms until M4.5, D066) and
    `hot_path_network_calls == 0`. A key without samples fails, except `readdir`/`readdirplus` (the kernel chooses)
    and `read_ram`. Latencies are the daemon's own (§5.8: callback entry to reply, commit waits included).
  - **Network counts:** the "first byte" and "first listing" rows of §5.7 are counts of backend calls, not times;
    engine tests check them with the per-method backend counts
    (`first_bytes_and_listings_cost_the_slo_round_trips`).
- **Alternatives:**
  - Client-side timing (`latency.sh`, `read.sh`): includes the kernel and the client, and §5.7 is defined in the
    daemon.
  - A git.git tree as files instead of a clone: `git status` would not see a repository written the way git writes
    one.
  - A real network shaper (`tc netem`): needs root, and would shape the whole machine.
- **Consequences:** M4.5 takes its baseline with this harness, and the M4.16 exit check reruns it. The git and untar
  rows, and so the slo row, need M4.4: pending files hold a descriptor each until then, and the first run (2026-10-28)
  hit EMFILE during the checkout of git.git (about 4700 files). The gc row passes only after M4.9.
- **Refs:** DESIGN.md §5.7, §5.8, §15.1, §16; docs/plans/m4.md M4.2, M4.3; D056. Limits amended in M4.5 by D065
  (untar) and D066 (fsync); M4.6.3 changes the git row's method (its ratio ranged 0.42–2.68 over the three M4.5
  runs). Accepted by user 2026-11-01.

## D058 — Upload parallelism with P7: 4 files, 5 blocks over all of them; fork patch P18
- **Status:** accepted
- **Decided by:** claude
- **Date:** 2026-10-28
- **Context:** D033 keeps transfers slightly below the official SDKs. Uploads went one file at a time, since PdMount never
  set P7's `max_concurrent_file_transfers`, with the community SDK's fixed 6 blocks: equal to C#'s limit (6 files, 6
  blocks in total), not below it. `upload.concurrency` (4) prepared 4 jobs, but only one reached the network.
- **Decision:**
  - Fork patch P18 adds `ProtonDriveClient::with_block_transfer_parallelism(n)` (docs/sdk-patches.md).
  - Every drive client PdMount builds (`session::TransferLimits`: daemon, re-login, `status`, `trash`, the probe) gets
    `with_max_concurrent_file_transfers(upload.concurrency)` and `with_block_transfer_parallelism(5)`.
  - `upload.concurrency` must be 1–4; above 4 is a config error naming D033. The default stays 4.
- **Alternatives:**
  - Build the client with `from_components`: duplicates the SDK's construction of its API clients and cache.
  - 5 files: equal to JS's limit, not below it.
  - Keep one file at a time: small-file workloads (git, untar) wait on one round trip chain per file.
- **Consequences:** up to 4 uploads in flight, sharing 5 block transfers; the JS SDK's 40 MiB total of expected upload
  size has no counterpart yet. The client's download queue gets the same limits: block downloads now wait for one of 5
  SDK permits, the same number as PdMount's own `DOWNLOADS`. The real-account rate (Q7) is measured in M4.15.
- **Refs:** DESIGN.md §6.4, App. B; docs/sdk-patches.md P7, P18; docs/plans/m4.md M4.7; D033. Accepted by user 2026-10-29.

## D059 — An XDG autostart entry instead of a systemd user unit
- **Status:** accepted (no-services part superseded by D074)
- **Decided by:** user
- **Date:** 2026-10-27
- **Context:** DESIGN.md §14 planned `pdmount service install|uninstall` as a systemd user unit (`ExecStopPost=fusermount3
  -uz`, `Restart=on-failure`; D020). The machine PdMount runs on has no systemd (PID 1 is `init`, no `systemctl`).
- **Decision:**
  - `pdmount autostart enable|disable` writes or removes `$XDG_CONFIG_HOME/autostart/pdmount-<profile>.desktop`, which
    runs `pdmount --profile <p> [<secret-storage flag>] mount` at desktop login. `pdmount autostart` shows the state.
  - No systemd unit. This supersedes the systemd part of D020; its AutoUnmount part stands.
- **Alternatives:**
  - The systemd unit as designed: cannot run or be tested here.
  - An OpenRC user service: supervision and restart, but tied to OpenRC's user services.
  - A systemd unit where systemd runs plus the autostart entry: two integrations to keep.
- **Consequences:**
  - No supervision: a crashed daemon is not restarted, and a stale mount after a crash is still cleared with
    `pdmount unmount --force` (§14.4).
  - At logout the daemon ends with the session's SIGTERM if the session manager sends one; otherwise it keeps running
    until `unmount`.
  - A daemon in `needs_login` shows the sign-in link through its desktop notification (D042).
- **Refs:** DESIGN.md §14.2, §14.4, §16, §17; docs/plans/m4.md M4.13; D020, D042.

## D084 — `scripts/ci.sh --quick` is the default before a commit; named tasks and every push need the full run (amends D056)
- **Status:** accepted
- **Decided by:** user
- **Date:** 2026-10-31
- **Context:** D056 and AGENTS.md ask for `scripts/ci.sh` before each task's commit. The run takes about 35 minutes, nearly all of it scenarios that low-risk tasks cannot affect. The user asked to make the quick run the default and to name the tasks that need the full one.
- **Decision:**
  - Before a commit, `scripts/ci.sh --quick` (fmt, clippy, unit and mounted tests, script self-tests) must pass, plus any scenario the task adds or changes.
  - **The full `scripts/ci.sh` is needed** when a task changes the journal, engine, tree, staging, store (migrations too), cache, events, the FUSE layer, the backend adapters, the daemon's start or stop, or the scenario harness; and at each milestone's exit task. Which tasks that makes is marked `[full]` in TODO.md's checklists; the task's real diff decides if it turns out otherwise.
  - **Nothing is pushed to `master` until a full run has passed on the commit being pushed.** Commits tested only with the quick run stay local until then.
  - A failure in a later full run is pinned down with `git bisect` over the unpushed commits, rerunning only what failed, a few times per step where timing may be involved. The fix is a new commit on top; history is not rewritten.
- **Alternatives:** Full CI per task (D056; safest, 35 minutes each); quick CI everywhere and a full run only at the milestone's end (a failure in lock-ordered code would be far from its cause).
- **Consequences:** failures in low-risk tasks show up later, and every commit still builds and passes the unit and mounted tests, so bisect works. `master` only ever gets commits that passed a full run. Noise makes bisect less reliable for timing failures, hence the reruns.
- **Refs:** D056; AGENTS.md (Git, Commands); TODO.md (task checklists); `scripts/ci.sh`.

## D062 — Shutdown waits for running steps only; opens are not refused
- **Status:** accepted
- **Decided by:** claude
- **Date:** 2026-10-31
- **Context:** §14.4 listed "stop accepting new opens" and a wait for uploads, and neither existed: `Engine::shutdown` aborted its tasks at once, and a running upload died with the process. M4.12 adds the wait. D075 (M5.1) widens it to a flush of what is queued.
- **Decision:**
  - At SIGTERM, SIGINT or `unmount` the journal stops starting steps. `Engine::shutdown` then waits up to `timeouts.shutdown_grace` until no upload holds a permit and no entry has a step running, and goes on as before.
  - New opens are not refused: anything accepted stays pending and resumes at the next mount, so refusing would only turn a harmless write into an error.
  - The wait comes after the unmount and the session's end (at most 10 s), so a stop takes at most 10 s + grace + the commit. `unmount --force` sends SIGKILL after `shutdown_grace` + 30 s instead of 30 s, and `unmount` waits for the larger of that and 60 s for the daemon to go.
- **Alternatives:** Waiting for everything pending (that is D075's flush, M5); aborting at once (works, since the journal retries, but repeats the whole upload).
- **Consequences:** a stop can take as long as the grace longer than before. An upload cut off by the grace is uploaded again at the next mount, and a commit that reached the server is adopted by its SHA-1, as after a crash.
- **Refs:** DESIGN.md §14.4, App. B; docs/plans/m4.md M4.12; D075. Accepted by user 2026-11-01.

## D061 — Queue tooling: inode ids, user parks in RAM, `drop` = back to the remote state
- **Status:** accepted
  Accepted by user 2026-10-31.
- **Decided by:** claude
- **Date:** 2026-10-30
- **Context:** M4.11 adds `pdmount queue` and `pdmount sync` (DESIGN.md §8.2, §5.6). The journal is one entry per inode, not an
  operation log, so an entry has no id of its own, and it stays in RAM. The plan sends `drop` of a node with a remote counterpart
  through `resolve_keep_remote`, but that function uploads the local copy and then trashes the node (D045), the opposite of a drop.
- **Decision:**
  - Entries are named by inode number; exactly one of `--id`, `--all` or a path is given. A deleted file's entry counts at the place
    of its remote file, so `queue list <dir>` and `sync <dir>` include deletes under it and `drop <path>` can undo one.
  - `queue park` lives in RAM like an automatic park: `retry` or a remount resumes it. **Unlike an automatic park, a local change
    does not resume it:** a file that is being edited would otherwise never stay parked.
  - `queue drop --yes` has its own path, following the plan's description and not the function it names. A node with a remote
    counterpart goes back to its known revision, attributes and place (`adopt_remote_placement`, `revert_to_remote`), a tombstone
    is unbound, and the remote folder is listed again. A node never uploaded is removed with its subtree, `.pdignore`d files
    included; `drop` without `--yes` prints all of them. It does not go through `unlink`, so an `ro` rule does not refuse it: content
    accepted before the rule (§11.4) can still be discarded. Conflict records stay; `keep-both` dismisses them.
  - A running step refuses the whole `drop`. Selected entries are held under the journal lock first, and the executor rechecks at
    start, so a step planned before the hold cannot start. A node that cannot go back is reported and stays queued; undos repeat until
    none succeeds, since one can free the name another needs.
  - `sync` selects its entries again each round (50 ms), so new ones count, and forces each once, along with what it waits for: a file
    can wait for a folder or a name holder outside the path. It stops at once when all that is left is parked or held up by a parked entry. `unmount`'s
    drain stays as it was.
- **Alternatives:** Stored parks (a restart that resumes everything is the rule for parks, and a stale park would hide changes);
  ending a user park at the next local change (the plan is silent; see above); refusing `drop` for moved files (leaves no way out when
  a move can never succeed); `drop` through `unlink`/`rmdir` (D045; bottom-up, no invalidations from a CLI request, and refused
  under `ro`).
- **Consequences:** an open writer is not protected from `drop`: its next write stages the remote version again and queues that. The
  drop of a never-uploaded folder that holds `.pdignore`d files loses them, which is why the preview lists them. A folder made
  a moment ago usually has its remote node already (folders do not wait for the debounce), and then `drop` of it only drops
  what is pending inside. A delete dropped alone inside a deleted folder is refused, since the folder's trash would take the file along.
- **Refs:** DESIGN.md §5.6, §7.8, §8.2, §14.2; docs/plans/m4.md M4.11; `crates/pdmount-core/src/engine/queue.rs`.

## D060 — Pins: model, fetching, eviction and the cap
- **Status:** accepted
  Accepted by user 2026-10-30.
- **Decided by:** claude
- **Date:** 2026-10-30
- **Context:** M4.10 adds `pdmount pin` (DESIGN.md §5.3). The plan gives each node a `pinned` flag recomputed on every
  rename and move, and has the event path, the upload path and the unpin path each change the pin state of blocks.
  The tree changes in many places (local renames, listings, conflict splits, resurrection, name yields), and each
  place missed would leave a block wrongly pinned or evictable.
- **Decision:**
  - **Model:** a row in `pins` holds a file, or a folder's whole subtree, by inode. A pin follows its inode through
    renames. A file that replaces a pinned one takes the pin over, so an atomic save keeps it. Deleting the node
    deletes the row.
  - **Reconciling, no `Node.pinned`:** one background task walks the pinned subtrees, and from that alone decides
    which revisions are pinned (the current revision of each clean remote file) and what is missing. Changes only wake
    it. A staged file fetches nothing: its content is local, or in a base the cache protects (§6.2).
  - **Cache:** the disk cache keeps the set of pinned revisions; rows of those revisions, present or arriving, have
    `pin = 1`, are never evicted and do not count against `cache.max_bytes`. A revision let go keeps its `last_access`,
    so an old one goes soon.
  - **Fetching:** paced (D027): one background turn per file or per listing, and a new turn every 16 blocks so a large
    file does not hold up event polling. Prefetch priority inside the download limits (D033). Failures back off like the
    journal without parking; it waits while offline or needing a login.
  - **Space:** `pin` is refused when the known missing bytes exceed free disk space less 1 GiB. The fetcher stops
    below 1 GiB free with a WARN (`waiting-space` in `pin list`, a state the plan did not list).
- **Alternatives:** `Node.pinned` kept up to date in every tree path: faster to read, but nothing needs it on the hot
  path, and every path is a chance to miss one. Fetching only on events: a change missed by a trigger would stay wrong
  until the next mount.
- **Consequences:** Between a change and the walk (0.5 s after a wake, to share one walk) a block has its old state: a
  `cache gc` in that window can evict a just-uploaded pinned block, which is then fetched again. A walk reads the store
  per file whose layout is not in memory, on a blocking thread, only when pins exist.
- **Refs:** DESIGN.md §5.3, §6.2, §13.2, §14.2; docs/plans/m4.md M4.10; `crates/pdmount-core/src/engine/pins.rs`; D027, D033.

## D063 — Staging files are opened on demand through a descriptor LRU
- **Status:** accepted
- **Decided by:** claude
- **Date:** 2026-10-28
- **Context:** every staged file held an open descriptor from its first write until its staging was removed, so the
  number of pending files was bounded by the open-file limit (4096 in the sandbox). The M4.3 benchmark checked out
  git.git (about 4700 files) on the fake mount and hit EMFILE: creates failed, and the fake backend's own file I/O
  failed with an `Other` error that parked 29 entries for good; the 3834 entries behind them never ran.
- **Decision:**
  - `staging.rs` has an `FdCache`: an LRU of `Arc<File>` by inode under a mutex, capacity 1024, or a quarter of the soft
    open-file limit when that is lower (at least 16). Each staging operation takes the descriptor from it and drops
    it at the end; a miss opens the file read-write. Opens and the close of an evicted descriptor run outside the
    mutex.
  - Creating a staging file puts its new descriptor into the LRU; reopening at startup only reads the length; removal
    closes the descriptor before deleting the files. `fsync` uses whichever descriptor is current: it flushes the inode.
  - `staged_open_files` is the LRU's length.
  - A backend's local EMFILE/ENFILE (`BackendError::local_io`) is `Transient`, so it is retried instead of parking.
- **Alternatives:**
  - Raise the limit further: the hard limit bounds it, and the next large tree hits it again.
  - Open and close on every operation: an `open` per 4 KiB write on the hot path, and on btrfs a close after a
    truncation flushes the file.
  - A fixed capacity of 1024 as first planned: under a soft limit of 512 (the M4.4 test) it would run out itself.
- **Consequences:** files beyond the capacity cost an `open` when they are used again; a close of an evicted file can
  run on a FUSE thread, outside every lock. Pending files are bounded by disk space and the queue, not descriptors.
- **Refs:** DESIGN.md §5.8, §7.1; docs/plans/m4.md M4.4; D057. Accepted by user 2026-10-29.

## D064 — The kernel writeback cache stays off by default, behind `kernel.writeback_cache`
- **Status:** accepted (option part superseded by D083)
- **Decided by:** user
- **Date:** 2026-10-30
- **Context:** D017 left the writeback cache off until an M4 experiment; it becomes the default only if everything passes
  and it helps measurably (docs/plans/m4.md M4.8). Measured on the fake, Linux 6.18, frozen binary, on against off:
  - Scenario matrix (4 modes, R/G/N/A/E/M/sha): 120 of 124 against 124 of 124. A6 fails in `live`: a write on a handle
    opened before a switch to `ro` returns success, EROFS arrives at fsync and close, and until a remount the file
    has the kernel's size and reads as zeros past the daemon's end (`first` plus 6 zero bytes; `first` after the remount).
    The other three modes fail on the `ro` rule the aborted `live` run leaves behind.
  - pjdfstest passes with both, with the same counts; `FSX_DURATION=3600 tests/fsx.sh` passes with it on (62 million operations).
  - `m4.sh`: untar 12.52× against 11.09× (12 alternating runs without the network profile: 0.848 against 0.777 s, 5000
    writes instead of 11 433 but 10 101 `setattr` instead of 5101); read 1079 against 1078 MB/s; `git status` 2.72
    against 2.53× in alternating runs; fsync p99 30.2–39.0 against 30.4–52.6 ms, with one default run at 112 ms.
  - A remote edit that changes a file's size stays unseen: the file still had the old size after 60 s, closed or open.
- **Decision:** off by default. The code stays behind `kernel.writeback_cache`, with a WARN at mount, for a mount whose
  files no other device or app changes.
- **Alternatives:**
  - On by default: fails both conditions, and needs an answer to A6 and to remote size changes.
  - Remove the option: the plan keeps the code behind it either way.
- **Consequences:** the M4 exit needs no 6 h fsx with it on. The hazard that dirty pages the daemon has not seen lose the
  conflict check (§5.4.1) comes from reading the kernel; no test demonstrates it.
- **Refs:** DESIGN.md §5.4, §5.4.1; D017; D068; docs/plans/m4.md M4.8.

## D065 — The untar exit criterion is ≤ 15× local
- **Status:** accepted
- **Decided by:** user
- **Date:** 2026-10-29
- **Context:** §16 asked for an untar of 5000 small files at most 3× local btrfs (time to acknowledgement, D057). The
  M4.5 baseline measured 14× on the SSD (median 1.00 s against 0.072 s, three runs: 14.04, 14.01, 15.10). A/B builds
  (`docs/plans/m4.md`, "M4.5 outcomes") showed where it goes: about 0.5–0.6 s is the wait for the store commit before
  each reply (D032), and the daemon's own work per file is about 120 µs (create 55, setattr 28, release 27 µs p50 on
  tmpfs). Replying before the commit and skipping the staging sidecar still took 0.32 s on the SSD (4.5×) and
  0.23 s on tmpfs (3.1×): each file costs about 9 FUSE requests (lookup, create, 2–3 writes, 2 `getxattr` of
  `security.capability`, flush, release, setattr), against about 14 µs per file for local btrfs.
- **Decision:** the criterion is **≤ 15× local**, measured as D057 defines; `scripts/bench/m4.sh` checks 15.0.
- **Alternatives:**
  - Keep 3× and accept the gap: the row would fail at every exit check.
  - ≤ 10×: needs more than the planned fixes (M4.6.1, M4.6.2), and disk noise (single runs from 0.87 to 1.76 s) could
    fail it.
  - Reply before the commit (drop D032): gives up the crash guarantee and still misses 3×.
- **Consequences:** M4.6.1 and M4.6.2 are expected to bring it to about 10–12×. The floor numbers stay in the M4.5
  outcomes for a later look (fewer FUSE requests per file, for example with the writeback cache, M4.8).
- **Refs:** DESIGN.md §16; D032; D057; docs/plans/m4.md "M4.5 outcomes".

## D066 — The `local` fsync SLO is p99 < 30 ms
- **Status:** accepted (limit raised to 40 ms by D086)
- **Decided by:** user
- **Date:** 2026-10-29
- **Context:** §5.7 asked for p99 < 10 ms for `fsync` in `local` mode. The benchmark load (4 clients, an fsync every
  tenth iteration, D057) measured p99 48, 35 and 34 ms (p50 23 ms). The disk is a SATA SSD (btrfs):
  under the same 4-writer load one bare `fdatasync` has p50 3.6 ms and p99 8.9 ms; two or three in a row have p99 22–27 ms.
  `local` fsync needs two flushes, the staging data and the store's WAL (§5.6); today it does three in a row
  (data, sidecar, WAL). Flushing data and WAL in parallel and dropping the sidecar of new files (M4.6.2) measured
  p50 10.7 ms, p99 26 ms.
- **Decision:** the SLO is **p99 < 30 ms**; `scripts/bench/slo.py` checks 30 000 µs.
- **Alternatives:**
  - A limit relative to the bare disk, measured by the harness in the same run: portable, but one more harness step.
  - Keep 10 ms and accept the gap: below what two flushes cost on this disk.
- **Consequences:** the limit is tied to this machine's disk; on NVMe it is loose. M4.6.2 carries the fix.
- **Refs:** DESIGN.md §5.6, §5.7; D032; D057; docs/plans/m4.md "M4.5 outcomes".

## D067 — A staging file without a base has no sidecar; `local` fsync keeps flushing data and store in turn
- **Status:** accepted
- **Decided by:** claude
- **Date:** 2026-10-29
- **Context:** M4.5 measured `local` fsync at p99 34–48 ms under the benchmark load (D066: limit 30 ms). It flushed
  three files in a row: the staging data, the staging sidecar and the store's WAL, each 2–9 ms on the SATA SSD under
  load. A new file's sidecar holds nothing (no base, no `base_rev`), yet every create wrote it (a temp file and a
  rename, about 35 µs on the SSD) and every fsync flushed it. In the A/B builds (docs/plans/m4.md, "M4.5 outcomes")
  flushing data and WAL in parallel brought fsync to p50 13.6 / p99 32.5 ms, and dropping the new file's sidecar to
  10.7 / 25.9 ms.
- **Decision:**
  - **Sidecar only with a base.** `StagingDir::create` writes the sidecar only when the file has a `base_rev` or a
    `base`, and then before the data file, so a data file without a sidecar never had a base. Staging without a base
    removes a sidecar left from earlier staging of the inode. Saves after that (a new `base_rev`, materialization, a
    truncation) write it whenever it is not the default; once written it stays until the staging is removed.
  - **Startup** lists the data files, not the sidecars. A missing sidecar reads as no base; a sidecar without a data
    file is removed (removal deletes the data file first).
  - **`Staged::sync`** flushes the sidecar first, only if it changed since its last flush, then the data.
  - **`Engine::fsync`** keeps flushing the staging data first and then committing the store durably, not both at once
    (measured, see Alternatives).
- **Alternatives:**
  - Flushing data and store in parallel, as the M4.5 A/B suggested (and M4.6.2 first built): fsync p50 8.6–11.6 ms,
    p99 23.6–32.1 ms, but create, mkdir, unlink, rename and setattr went from p99 0.7–1.2 ms to 4.7–7.0 ms, against
    the 2 ms SLO (§5.7), in 3 alternating `m4.sh slo` runs. On btrfs the WAL's fsync then overlaps the staging file's
    and lasts longer; the store writer's appends to the WAL wait for it, and every FUSE reply waits for the writer.
    With the flushes in turn (and no sidecar for new files): fsync p50 15.2–19.1 ms, p99 28.7–31.5 ms, namespace
    operations p99 0.62–0.83 ms.
  - Keep three flushes in a row: p99 above the D066 limit on this disk.
  - Store the sidecar in the SQLite store: the store commit would order it with the rows, but materialization would
    then queue a store write per block, and a crash between data and row is what the file order guards against now.
  - Delete the sidecar when it is back to the default: a second rule for the same state.
- **Consequences:** fsync p99 is at the D066 limit on this disk (28.7–31.5 ms in three runs), not below it with margin.
  Staging from older versions always has a sidecar and reopens unchanged. A pending new file is found
  at startup by its data file; before this change, startup would have recreated it empty had it lacked a sidecar.
- **Refs:** DESIGN.md §5.6, §7.1; D032; D066; docs/plans/m4.md M4.6.2. Accepted by user 2026-10-29.

## D068 — `FUSE_HANDLE_KILLPRIV_V2` is not requested; the kernel keeps asking for `security.capability`
- **Status:** accepted
- **Decided by:** claude
- **Date:** 2026-10-30
- **Context:** M4.5 counted 8819 `getxattr(security.capability)` requests for the 5000 files of the untar row (11 433
  writes). Before a write the kernel checks whether it must drop privileges (`file_remove_privs`,
  `dentry_needs_remove_privs`), which asks the file system for the capability attribute unless the inode is marked
  `S_NOSEC`. Fuse marks inodes `S_NOSEC` only on a super block with `SB_NOSEC`, which it sets only with
  `FUSE_HANDLE_KILLPRIV_V2` (Linux 6.18 `fs/fuse/inode.c` `process_init_reply`); with it, the first write after each
  attribute refresh asks once. The writeback cache does not change this. With V2 the kernel no longer clears
  setuid/setgid on write, truncation and chown: it passes `FUSE_WRITE_KILL_SUIDGID`, `FATTR_KILL_SUIDGID` or
  `FUSE_OPEN_KILL_SUIDGID` and the daemon must clear them (S-12). fuser 0.18 passes the write flag on, but not the
  setattr and open ones.
- **Decision:** not requested. Measured in M4.8 (untar time to acknowledgement, 12 alternating runs each, state on
  the SSD, no network profile): `getxattr` 8819 → 5000 per untar, median 0.777 → 0.774 s (interquartile ranges
  0.748–0.822 and 0.727–0.782 s). The plan kept V2 only if untar improved measurably; it did not.
- **Alternatives:**
  - Request V2 and clear setuid/setgid in the daemon on every write, truncation, `O_TRUNC` open and chown: about
    0.8 round trips less per small file, for code that has to guess the setattr and open flags fuser drops.
  - Answer `getxattr` with ENOSYS, which stops all `getxattr` requests: it would take away PdMount's own `user.pdmount.*`
    attributes (§8.7).
- **Consequences:** none in the code; the kernel keeps clearing setuid/setgid itself (through `setattr`).
- **Refs:** DESIGN.md §5.4; docs/plans/m4.md M4.5, M4.8 step 3a; security review S-12. Accepted by user 2026-11-01.

## D069 — The relist queue is kept in the store until each listing succeeds
- **Status:** accepted
- **Decided by:** claude
- **Date:** 2026-10-29
- **Context:** The 10 000-case run of `two_devices_and_an_app_converge` during M4.6.2 found a case that fails on master
  too (16 of 20 replays at `2380b62`): B creates `/a` while the app creates `/a` on the drive; B's event sync hides the
  app's node behind its own (a clash, D040); B unlinks its `/a` before it uploads, which queues a listing of the root
  (D046); B restarts before the executor runs it. The queue was memory only, and the unlinked node's row is gone, so
  nothing listed the root again: the app's `/a` stayed hidden on B for good. D046 had left persistence out because "a
  listing after a restart finds them anyway", which holds only while the occupant still exists.
- **Decision:**
  - `Engine::list_later` queues the listing and keeps the set of folders still to list in `meta` (`relist`), written
    through the same store queue as the change that asked for it, so the reply to that change waits for it too.
  - The journal counts the open requests per folder; a successful listing answers one, and the folder leaves the set
    when none is open (a request made while a listing is in flight needs a later one, D054). A failed listing keeps it.
  - At start, the folders in `relist` that are still listed folders are queued again, with those of D055.
  - `Node::clashed` stays memory only (D046).
- **Alternatives:**
  - A table of hidden remote nodes (D046's rejected alternative): more state for the same effect.
  - Keeping the unlinked node's row until the listing ran: a second kind of tombstone.
  - Marking the folder not listed: lookups in it would wait for the network until it is listed again.
- **Consequences:** one small `meta` write when the set changes: when a clashed node leaves its name, after the
  listing, and at a start that found the set non-empty. A folder whose listing keeps failing is retried at the next start,
  not sooner, as before.
- **Refs:** DESIGN.md §10.6, §13.2; D040; D046; D054; D055; `engine/proptests.rs` (`concurrent_regression_cases`). Accepted by user 2026-10-29.

## D070 — Security threat model and review method (first review, M4.0.1)
- **Status:** accepted
- **Decided by:** claude
- **Date:** 2026-10-27
- **Context:** The user asked for a first security review of the whole codebase before the M4 work, and for a second
  review of the M4 changes at the M4 exit (docs/plans/m4.md M4.0.1, M4.16). Reviewers need one shared threat model
  and rating scale, so that findings from different areas can be compared.
- **Decision:**
  - **Assets:**
    - the Proton session (tokens, key passphrase, the SDK secret cache and its key);
    - the cache master key and the encrypted block cache;
    - plaintext local data (staging, the SQLite store, logs);
    - the integrity and confidentiality of the user's files on the drive;
    - the user's local system outside the mount.
  - **Actors:**
    - T1: other local users (sockets, file modes, shared paths, races);
    - T2: untrusted remote content (names, sizes, modes, block lists, events written by other clients, devices or shares);
    - T3: the network (TLS, retry behaviour);
    - T4: same-uid processes, only where the design promises more (secrets never logged, D029 file modes, the sign-in link shown only to the user);
    - T5: the user's own config and CLI input (no panics into undefined states, no writes outside intended places).
  - **Out of scope:** root, a compromised Proton server or Proton's crypto design, upstream crates (the fork's own
    patches are in scope), and dependency advisories (`cargo audit` needs GitHub, D023). Access rules are a safety
    feature, not a security boundary.
  - **Ratings:**
    - Critical: account compromise, or arbitrary file write or exec outside PdMount's directories.
    - High: other-user read or write of secrets or local data, secrets in logs or world-readable files, remote-triggered writes outside the mount, or silent corruption of user data.
    - Medium: DoS from remote input, integrity-check gaps without a shown exploit, weak modes on non-secret data.
    - Low / Info: hardening.
  - **Method:**
    - Five reviewers (Fable), one per area, read-only, given this model.
    - Then one verifier (Fable) checks each finding against the code, merges duplicates and sets the final rating.
    - High and Critical findings must be fixed before the next milestone work; the user may add Mediums (done for M4.0.1).
    - The M4.16 review reuses this model for the M4 diff.
- **Alternatives:** `/security-review` alone (it covers a branch diff only, not a first review); one reviewer for the
  whole codebase (about 40k lines: too much for one pass).
- **Consequences:** M4.0.1 found 1 High (S-1: the SDK fork logs the key passphrase and unlocked keys at debug), 5
  Mediums and 20 Lows/Infos (one more finding rejected) (docs/plans/m4.md "M4.0.1 outcomes"); fixes M4.0.2.1–M4.0.2.4; the rest in TODO Known
  gaps. The full threat model text is `docs/security/threat-model.md`; the reports are in `docs/security/2026-10-27-review/`.
- **Refs:** docs/security/threat-model.md; docs/security/2026-10-27-review/; docs/plans/m4.md M4.0.1, M4.16, "M4.0.1
  outcomes"; DESIGN.md §12, §13; D023, D029, D042. Accepted by user 2026-11-01.

## D071 — The sign-in link is never logged or put in a notification (supersedes part of D042)
- **Status:** accepted
- **Decided by:** claude
- **Date:** 2026-10-27
- **Context:** D042 sends the daemon's sign-in link to the log (WARN) and to the desktop notification. The link's fragment
  carries the fork key, half of the secret that completes the sign-in; the log was world-readable (S-3) and
  `notify-send`'s argv is readable by every local user in `/proc/<pid>/cmdline`. The 2026-10-27 review rated this
  Medium (S-2).
- **Decision:**
  - The log names the code only, with a hint: "sign-in needed: run `pdmount login` or `pdmount status` for the link".
  - The notification body is "Sign-in needed for profile <p>: run `pdmount login`", with no link and no code.
  - The link stays available over the 0600 control socket, to `pdmount status` and `pdmount login`, as before.
  - The rest of D042 is unchanged.
- **Alternatives:** Keeping the link in the 0600 log (still in argv for the notification, and logs get copied into bug
  reports); a D-Bus notification library to keep the link out of argv (a dependency, and the log question remains).
- **Consequences:** A user who sees the notification runs one command to get the link; a headless service user reads
  `pdmount status` instead of the log.
- **Refs:** DESIGN.md §12.2; docs/security/2026-10-27-review/verification.md S-2; docs/plans/m4.md M4.0.2.1; D042. Accepted by user 2026-11-01.

## D072 — Directory pages stay in the kernel cache while the entries are unchanged
- **Status:** accepted
- **Decided by:** claude
- **Date:** 2026-10-30
- **Context:** After M4.6 the git row failed at 3.10–3.47× (limit 2.0, D057). Per warm `git status` on git.git the
  daemon answered about 233 opendir, 233 releasedir, 467 readdir/readdirplus and 234 getattr requests, each a few µs,
  but each a FUSE round trip. The opendir reply set `FOPEN_CACHE_DIR` without `FOPEN_KEEP_CACHE`, so the kernel dropped
  the directory's cached pages at every open (Linux 6.18 `fs/fuse/dir.c` `fuse_dir_open`) and listed it again; each
  uncached listing also invalidates the directory's atime, so the next `lstat` asks for its attributes.
- **Decision:**
  - `Engine::opendir` hashes the snapshot it takes (inode, kind and name of each entry) and keeps, per directory, the
    hash given to the kernel at its last open. The reply adds `FOPEN_KEEP_CACHE` when the new hash equals it and no other
    handle of the directory is open.
  - The kernel drops its cache itself on local changes through the mount (`fuse_dir_changed`) and on `inval_entry` and
    `inval_inode` (§9.5); the hash catches any change the daemon made without telling the kernel, at the next open.
  - The second condition guards against a handle opened before a change filling the cache from its older snapshot
    after the change. An open next to another handle also forgets the stored hash, since the pages may then come from
    either snapshot; the next open alone fills them again.
- **Alternatives:**
  - `FOPEN_KEEP_CACHE` on every opendir: relies on every change to a directory sending an invalidation; a missed one
    leaves a listing stale with no expiry.
  - An epoch per directory, bumped where the tree changes entries: about ten places in `vfs.rs`, each a chance to miss
    one.
  - No `opendir` at all (ENOSYS, `no_opendir`): saves the opendir and releasedir round trips too, but readdir would
    have no snapshot, so offsets would shift under concurrent changes.
- **Consequences:** measured in M4.8 (22 alternating `git status` runs per variant, no network profile): the mount
  went from 55 to 32 ms, the ratio from 2.53–2.75 to 1.39–1.41; readdir/readdirplus fell from about 5100 to 0 and
  getattr from 2580 to 11 per 11 runs. The `m4.sh` git row with the network profile: 1.48 (limit 2.0). opendir and
  releasedir still reach the daemon. Each opendir hashes its entries, about as much work as the snapshot it already
  copies. The readdir and readdirplus SLO checks now have no samples, so slo counts 13 checks instead of 15.
- **Refs:** DESIGN.md §5.4, §9.5; D057; docs/plans/m4.md M4.8. Accepted by user 2026-11-01.

## D073 — Milestones: M5 user services and flush, M6 conflict notifications, M7 upstream and crypto
- **Status:** accepted
- **Decided by:** user
- **Date:** 2026-10-30
- **Context:** Two features were added after M4 was planned, ahead of the upstream and crypto work.
- **Decision:** M5 is user services, flush at session end and further roots (D078); M6 is conflict notifications; the former M5 becomes M7,
  unchanged. A conflict resolution UI may follow later and is not decided.
- **Alternatives:** Appending the features after the upstream milestone (not chosen; that work waits on outside events: D023, Q5).
- **Consequences:** The multi-device scenarios M5–M7 of §15.3 share their names with these milestones, as M1–M4 already did.
- **Refs:** DESIGN.md §16; TODO.md; docs/plans/m5.md, m6.md.

## D074 — OpenRC and systemd user services, next to the autostart entry
- **Status:** accepted
- **Decided by:** user
- **Date:** 2026-10-30
- **Context:** D059 chose an autostart entry because the machine has no systemd, and rejected service units. Flushing at
  logout and shutdown is now required, which needs a reliable stop signal, supervision is wanted, and the user has a Debian
  machine to test systemd by hand. An autostart entry gets SIGTERM only if the session manager sends one.
- **Decision:** `pdmount service [install|uninstall]` installs a user service for the profile: an OpenRC user service or a
  systemd user unit. M4.13's autostart entry stays, for desktops with neither; it has no flush guarantee, and the
  documentation says so. A profile uses one of them; each refuses while the other is set.
- **Alternatives:** Services replacing the autostart entry (two integrations instead of three; the user chose to keep it).
- **Consequences:** Three integrations to keep. systemd is tested by hand on Debian (the sandbox has neither session); units
  are golden-file tested in CI. Supersedes D059's "no systemd unit" and its rejection of an OpenRC service; the rest stands.
  D020's systemd part (`ExecStopPost=fusermount3 -uz`) applies again to the systemd unit.
- **Refs:** DESIGN.md §14.2, §14.4, §16; docs/plans/m5.md; D020, D059.

## D075 — SIGTERM flushes the queue for up to `shutdown_grace`; warning at the next start
- **Status:** accepted
- **Decided by:** user
- **Date:** 2026-10-30
- **Context:** D062 (M4.12) has shutdown wait only for running uploads, so a change made just before logout stays on this
  machine until the next mount, and other devices do not see it.
- **Decision:**
  - Every SIGTERM, SIGINT or SIGHUP uploads what was queued at that moment, skipping the debounce, up to
    `timeouts.shutdown_grace`, configurable, default 90 s (was 30 s). This covers service stops and autostart alike.
  - No wait while offline or in `needs_login`; parked entries are not waited for; rate limits and backoff apply as usual.
  - What is left stays in the journal and resumes at the next mount. That mount warns with a desktop notification and a log line.
- **Alternatives:** 10 minutes (logout feels hung, and system stop limits need root changes); no limit (the service manager kills
  the daemon anyway).
- **Consequences:** Logout and shutdown can take up to 90 s when changes are pending, and take no longer when there are none. A 2 GB
  upload at 20 MB/s just misses the default. Unit stop timeouts are grace + 30 s. Whether the network is still up at SIGTERM
  depends on the system: M5.0 measures that before M5.1 is built, and if it is not, the requirement needs another mechanism
  on that system. `unmount --wait` keeps its 10 minutes.
- **Refs:** DESIGN.md §14.4; docs/plans/m5.md M5.0–M5.2; docs/plans/m4.md M4.12 (D062).

## D076 — A pending marker in `.pdmount/` tells other devices that changes are on their way
- **Status:** accepted
- **Decided by:** user
- **Date:** 2026-10-30
- **Context:** The user asked whether a flush can leave an entry such as `.pdmount/<device_id>.pending` in the mount, so that
  other devices know changes are coming. It can: drafts of other clients are not visible, and folder XAttrs cannot be updated
  (Q2), so a small file is the available signal, and the event engine delivers it with no extra polling.
- **Decision:**
  - Only when the grace ends with entries left, the daemon uploads `<root>/.pdmount/<device.id>.pending` in the last 10 s of the
    grace, directly through the backend (not the journal). JSON: device name, time, entries left, bytes left; no file names.
    A flush that finishes writes nothing, so a normal logout costs no extra API calls.
  - The next mount trashes it once the queue has drained (`Backend` has only `trash`), or the marker gets a new revision if it
    is still there at the next unfinished flush.
  - Other PdMount devices show `Incoming: <device>, N changes, since <time>` in `status` and a notification; a marker older than
    7 days is shown as stale. One file per device, so markers never conflict.
  - Writing is opt-in (`sync.pending_marker`, default off), as it adds a visible folder to the Drive. Reading is always on.
    In the mount, `.pdmount` is read-only (EROFS, D019), so `rsync --delete` on the root cannot remove it.
  - **Per tree (D080, user's correction, 2026-10-30):** a marker goes in the root of each writable tree that has entries left, and
    counts only that tree's entries, because only those who can read the tree are waiting for its changes. A marker in My Files
    means nothing to the other members of a share. `<root>` above is the tree's root, for My Files, a device or a share.
  - Opt-in is per root (`pending_marker` under `[[roots]]`), so writing into shares is its own choice. In a share the marker leaves out the
    device name, since a hostname is no business of the other members.
- **Alternatives:** Writing at the start of every flush, as first asked (covers a kill during the flush, but costs API calls and
  flush time on each logout with pending data, and the daemon already ends itself before the stop timeout). One marker in My
  Files for all trees (reaches only the account's own devices, not a share's other members).
- **Consequences:** Best effort: nothing is written when the machine is offline, loses power or crashes, which is when
  changes are most often left behind, so absence of a marker proves nothing. The official apps show `.pdmount` as a normal
  folder and ignore its meaning. A marker that reaches the trash stays there until it is emptied. D028 forbids writing it on the
  real account outside `pdmount-test/`, so tests stay on the fake unless the user grants an exception. In a share, the folder is
  visible to all members and its owner, a member using only the official apps just sees a file, and a marker can only be written
  while the account still has write access. Several trees with entries left share the last 10 s, so some may get no marker.
- **Refs:** DESIGN.md §14.4; docs/plans/m5.md M5.7, M5.14; D019, D028, D080.

## D077 — Conflict notifications name the file, offer actions, survive a missing desktop, and use D-Bus directly
- **Status:** accepted
- **Decided by:** user
- **Date:** 2026-10-30
- **Context:** §10.8 sends "N new conflicts; see `pdmount conflicts list`" through `notify-send`, for all kinds, and loses it when
  there is no session bus. D042 and D071 declined a D-Bus library because it served one message.
- **Decision:** Conflicts in state `open` get a notification naming the file and what happened, with a **Show** action. Conflicts
  not yet shown go out when a desktop session appears. Notifications go through `org.freedesktop.Notifications` directly (`zbus`).
- **Alternatives:** Keeping `notify-send` (no dependency, but no practical actions from the daemon, and file names in
  `/proc/<pid>/cmdline` for other local users); a tray indicator as well (more work, and overlaps with the undecided resolution UI).
- **Consequences:** A new dependency and a store migration (`notified_at`). File names come from other devices, so the text is
  escaped. A setting removes names from the text for lock screens. A resolution UI can build on the action later; not decided.
- **Refs:** DESIGN.md §10.8, §16; docs/plans/m6.md; D042, D071.

## D078 — M5 also maps Proton Drive roots other than My Files
- **Status:** accepted
- **Decided by:** user
- **Date:** 2026-10-30
- **Context:** D003 limited v1 to My Files. The user asked to add the mapping of other folders ("shared, photos, etc") to M5.
- **Decision:** M5 maps Shared-with-me, Photos and Computers. Trash stays out ("etc" is read as the trees D003 named; Trash has
  `trash empty` and the web client).
- **Alternatives:** A milestone of its own (not chosen by the user; see Consequences).
- **Consequences:** Extends D003 and RQ8. This is the largest part of M5, and it does not depend on the services and flush tasks,
  so its tasks come after them and could be split off. The engine assumes one root and one volume today.
- **Refs:** DESIGN.md §1, §14.5, §16; docs/plans/m5.md M5.9–M5.13; D003.

## D079 — Further roots: one mountpoint per root in one daemon, read-only at first
- **Status:** accepted (engine unit and read-only part amended by D080)
- **Decided by:** user
- **Date:** 2026-10-30
- **Context:** The mount root is My Files itself, so new top-level names could collide with the user's folders, and moving My Files
  under a subfolder would change every path, access rule and `.pdignore`. Photos and shares have rules My Files does not: share
  permissions vary, shares can be revoked, and Photos is a timeline with albums rather than a tree.
- **Decision:**
  - One daemon and session, with one FUSE mount and one engine and store per configured root: `<mnt>` stays My Files, and for example
    `~/ProtonDrive-Shared`, `-Computers`, `-Photos` hold the rest. Only configured roots are mounted, so nothing changes by default.
  - One session, because a second daemon on the session could rotate its tokens, and rate limits are per session and user (`cs/README.md`).
    All roots share one set of transfer limits and backoff.
  - All further roots are read-only at first (EROFS, D019). Writing to shares with editor rights is a later decision.
  - Remote changes come from events only, one cursor per volume (`event_cursor` already has the volume as its key). If a volume
    offers no events to a non-owner, that root needs a design change, not recursive polling (`cs/README.md`).
- **Alternatives:** Virtual top-level folders in one mount (collide with user folders, or need a migration of paths); a daemon per
  root (a session each, or token rotation between them).
- **Consequences:** Several mountpoints to configure; `status` and `unmount` become per root. M5.9 can overturn parts of this.
  Real-account tests are read-only on these roots (D028), and the user has to provide a share, a device and a photo.
- **Refs:** DESIGN.md §14.5; docs/plans/m5.md M5.9; D003, D019, D028.

## D080 — Computers and writable shares are read-write through the My Files engine; Photos stays read-only
- **Status:** accepted
- **Decided by:** user (scope); the engine-per-tree shape is claude's, from the code
- **Date:** 2026-10-30
- **Context:** The user asked for read-write on read+write shares before the M5 exit, with the same logic as My Files and without
  rewriting the read-only work of M5.11–M5.13, applicable to Computers too. Photos stays read-only unless it turns out to be the exact
  same logic; the user asked for that impression to be verified. Checked in the SDK sources:
  - **Shares and Computers use the My Files operations.** In the official JS client, a share or a device is reached with the same
    `getFileUploader`, `getFileRevisionUploader`, `createFolder`, `renameNode`, `moveNodes` and `trashNodes`, with only the node's id
    differing. The fork finds the signing address from each node's share (`sdk/node/operations.rs` `get_membership_address`).
  - **Photos is different; the impression holds.** `ProtonDrivePhotosClient` has no `iterateFolderChildren`, `createFolder`, `moveNodes`,
    `getFileRevisionUploader` or `iterateRevisions`. Its `getFileUploader` takes no parent and always uses the volume root; the commit
    goes through a Photos-specific call with a content hash and a capture time (`internal/photos/upload.ts`), and listing is a flat
    timeline. A file edit, a folder and a rename into a folder have no counterpart. Rename and trash exist, and the read path is shared.
    The fork's Photos client does expose folder calls, inherited from its drive client, but the official one does not use them, so
    relying on them would be outside what `cs/README.md` asks of a client.
- **Decision:**
  - The unit is a **tree** (volume, root link, write gate), and each tree has its own engine, store, journal and staging. This refines
    D079, which said one engine per root: a Shared or Computers mount holds several trees under a virtual read-only top level.
  - M5.10–M5.12 build the full engine for every tree with the gate closed; M5.14 opens it. No second write path, and no rewrite of those
    tasks.
  - Open: Computers, and shares where the node's role is `Editor` or `Admin`. Closed: `Viewer` (EROFS, D019), Photos, shared single files.
  - Losing the right with changes pending parks them and closes the gate; staged data is kept and moved to `orphaned/<tree>/` when the
    share is gone (RQ3).
  - Rename between trees returns EXDEV.
- **Alternatives:** One engine over several volumes (the inode tree, journal and event code would all need a volume in every key);
  a separate write path for shares (the rewrite the user ruled out).
- **Consequences:**
  - `ProtonBackend` holds one volume and root already, so the engine stays as it is, but the FUSE layer gets a router and events get one
    poller per volume, because Computers shares My Files' volume.
  - An upload to a share counts against its owner's storage, which `df` does not show; conflict copies are visible to other members.
  - Real-account writes need the user's exception to D028, so they are asked for at M5.14; D028 itself is unchanged.
- **Refs:** DESIGN.md §14.5, §15.3 (X8, X9), §16; docs/plans/m5.md M5.10–M5.14; `cs/client/js/src/protonDrivePhotosClient.ts`; D019, D028, D079.

## D081 — Browsing many images: folders in stages, Proton's thumbnails in the local cache, and a bulk-read guard on all trees
- **Status:** accepted
- **Decided by:** user
- **Date:** 2026-10-30
- **Context:** The user asked what happens when a file manager opens a mapped folder and reads thousands of images. The plan had no answer.
  The timeline gives only ID, capture time and tags, 500 per page, so names and sizes need node lookups. A thumbnailer is expected to read
  each original (not yet measured). Those reads count as interactive, so they get the top download priority and escape the
  pacing of background work (D027), and each fetches whole blocks of about 4 MiB. Roughly, 2,000 photos at 4 MiB each is 8 GB, about 7
  minutes at 20 MB/s, with a risk of HTTP 429 on the session all mounts share and of emptying the block cache.
- **Decision:**
  - **Bulk-read guard (M5.10), every tree:** many different files opened in quick succession become a scan: below interactive
    priority, paced, no readahead, a limited share of the block cache. Opening a single file stays interactive.
  - **Folders in stages (M5.13):** year and month folders from timeline pages; names and sizes when a month is opened, in paced
    batches, kept in the store and updated from events.
  - **Stored thumbnails (M5.13):** Proton's thumbnails are fetched in the background at prefetch priority, and written to
    `~/.cache/thumbnails` under the freedesktop rules, so a file manager need not open the originals. `photos.thumbnail_cache`
    turns this off.
  - M5.9 first measures what a file manager and thumbnailer read, whether they accept the pre-filled cache, and the format and size of
    Proton's thumbnails (assumed JPEG, about 512 px).
- **Alternatives:** Only the guard (safe for the API, but thumbnails come slowly and the originals are still downloaded); asking users to
  turn thumbnails off (the mount looks local to the file manager, so a default desktop would still hit the problem); a shared thumbnail
  repository per folder (unclear file manager support; can be looked at in M5.9).
- **Consequences:**
  - The daemon gains an image decoder, since the cache requires PNG: new attack surface for the M5.8 security review.
  - Decrypted thumbnails sit in `~/.cache/thumbnails`, as they do with any thumbnailer, but for everything browsed in Photos.
  - A file manager that ignores the cache relies on the guard alone: safe, but slow thumbnails.
  - The guard also sees `rsync` and `git`, which open many files, so it is checked against the M4 benchmarks.
- **Refs:** DESIGN.md §14.5, §15.3 (X10), §16; docs/plans/m5.md M5.9, M5.10, M5.13; D027, D080.

## D082 — `kernel.writeback_cache` applies to the My Files mount only
- **Status:** rejected (2026-10-30, by the user: with D083 there is no option to limit)
- **Decided by:** claude
- **Date:** 2026-10-30
- **Context:** M4.8 kept the writeback cache off by default and left the option for "a mount whose files no other device or app
  changes" (D064). D080 adds Computers and writable shares, which other people or devices write to by nature. With the option on, a
  remote size change is not seen, and a write can reach the daemon after the gate has closed (scenario A6).
- **Decision:** Further mounts never request `FUSE_WRITEBACK_CACHE`. D079's one mountpoint per root makes this possible, since each
  mount has its own FUSE session. When the option is set, the mount log and `doctor` say it covers My Files only.
- **Alternatives:** A per-root setting (a way to get wrong results in a share, for no measured gain: untar was 9 % slower with it);
  leaving it global (the option would then silently affect shares).
- **Consequences:** One setting has a narrower effect than its name suggests, so Appendix B must state it.
- **Refs:** DESIGN.md §5.4.1, §14.5; docs/plans/m5.md M5.14; D064, D079, D080.

## D083 — The writeback cache is never requested; `kernel.writeback_cache` is removed
- **Status:** accepted
- **Decided by:** user
- **Date:** 2026-10-30
- **Context:** D064 kept the option for a mount whose files no other device or app changes. Its measurements (D064) show no
  benchmark gain worth having: untar 9 % slower, reads unchanged, `git status` slightly slower; fsync p99 and the SLO count were
  a little better, within a spread that may be noise. Large-file writes and a real network were not measured. Further roots (D078,
  D080) would each need the option limited (D082). D064 had set removal aside only because the M4 plan kept the code.
- **Decision:** `FUSE_WRITEBACK_CACHE` is not requested on any mount. The key, its WARN and the test switches `KERNEL_WRITEBACK` and
  `PDMOUNT_TEST_WRITEBACK` are gone. Supersedes the option part of D064; "off" and its measurements stand.
- **Alternatives:** Keep it for My Files only (D082, rejected): a setting that can cost correctness, for no measured gain.
- **Consequences:**
  - A6's failure, unseen remote size changes and the conflict hazard of unseen dirty pages cannot occur through this path.
  - A profile that still has the key fails to load, as with any unknown key (the parser rejects them); the line must be removed.
  - Trying it again means restoring about 10 lines and rerunning the M4.8 suite.
- **Refs:** DESIGN.md §5.4, §5.4.1; docs/plans/m5.md M5.1; D017, D064, D082.

## D085 — Account key passphrases live in their own uncapped store; keys that cannot be unlocked mean `needs_login`
- **Status:** accepted (migration part superseded by D088)
- **Decided by:** claude
- **Date:** 2026-10-31
- **Context:** M4.15: `sdk-secrets.db` is capped at 5000 entries and drops the 25 % read least recently when full. Each new file or
  folder adds an entry. `account:passphrase:<key id>` is written at login and read only when the cached user keys are gone, so it
  goes first, and only a new login restores it. On profile `default` this parked 12 trash steps and made `status` fail.
- **Decision:**
  - A wrapper (`crates/pdmount/src/secret_cache.rs`) sends `account:passphrase:*` to `sdk-keys.db`, which has no cap, and the rest to
    the capped cache. Same encryption and cache key. Opening the caches moves passphrases still in `sdk-secrets.db`, as sealed values,
    by SQL on the SDK's `Entries` table: the repository trait cannot list keys, and waiting for a read could lose them first.
  - `clear()` on the wrapper keeps the passphrases. Logout and a new cache key delete the file.
  - `map_error` turns the SDK's "none could be unlocked" into `Unauthorized`: `needs_login`, no parking. `status` reports it, and `mount`
    comes up waiting for the sign-in when the session resumes but the keys do not open (D042).
- **Alternatives:** No cap on the secret cache (it grows by an entry per file for good). A fork patch exempting the prefix from eviction,
  or a typed error for locked keys (one more patch and a rev bump, for what the wrapper and one message check do; a test runs the
  SDK's own path, so a changed message fails there). Keeping passphrases with the credentials (a keyring write and dialogs for what
  already has a sealed store).
- **Consequences:**
  - A passphrase cannot be evicted, on any account. A profile that already lost it still needs one login.
  - The move relies on the SDK's table layout, and the wrapper on its key prefix; a test goes through the SDK's own passphrase cache.
  - One more refresh per concurrent call before `needs_login` stops requests (`with_reauth`), so a handful at most.
- **Refs:** DESIGN.md §12.4, §13, Appendix A; `docs/handoff-log.md` M4.15; D029, D042, D088. Accepted by user 2026-11-01, with the
  migration removed by D088.

## D086 — The §5.7 SLO row is the median of 5 rounds on a quiet disk; `fsync` p99 < 40 ms
- **Status:** accepted
- **Decided by:** user
- **Date:** 2026-10-31
- **Context:** The M4 exit run failed the slo row: `fsync` p99 132, 52, 30 and 60 ms over four runs against 30 ms (D066). Six alternating rounds
  of M4.10 and the final tree gave 33, 28, 27, 33, 104, 78 against 28, 29, 36, 31, 98, 69 ms, so the code did not regress. Both jump
  with the disk: a bare `fdatasync` p99 was 5.2–5.6 ms in rounds 1–4 and 21.4 and 17.0 ms in rounds 5 and 6, with nothing of ours running.
  On a quiet disk 30 ms sits in the middle of the spread, and `setattr` at its 2 ms limit (0.85–4.10 ms).
- **Decision:** `m4.sh slo` runs rounds until 5 are quiet, at most 15; a round with a bare `fdatasync` p99 above 10 ms before or after it
  does not count. Each key is judged on the median of its p99 over those 5. `fsync`'s limit is 40 ms; the other limits stay.
- **Alternatives:** 40 ms on a single run (a slow moment of the disk still fails the row); 30 ms with the median (close to a coin flip);
  lowering `fsync` itself (two flushes, D066, no known fix).
- **Consequences:**
  - A real regression still fails the row; a slow spell of the disk costs rounds instead. A disk that never gets quiet fails it.
  - The row takes about 5 minutes or more instead of one. `hot_path_network_calls` must be 0 in every round.
  - The 10 ms gate and the 40 ms limit are tied to this SATA SSD, as D066's limit was.
- **Refs:** DESIGN.md §5.7, §15.1, §16; `scripts/bench/slo.py`, `scripts/bench/fdatasync.py`; D057, D066.

## D087 — A WAL fsync and a staging fsync take turns, so commits do not stall behind them
- **Status:** accepted
- **Decided by:** claude
- **Date:** 2026-11-01
- **Context:** the M4.16 exit run failed the §5.7 row on `setattr` alone, median p99 3.00 ms against 2. The tail belonged to the first operation that commits after a `fsync` while other clients' `fsync`s run, not to `setattr`: swapping `chmod` and `rename` in the workload moved it to `rename`, and with one client it was gone. Temporary timing in the daemon showed why: 272 of 275 commits of 2 ms or more ran inside a WAL `fdatasync`, against the claim that a fsync never delays commits. In a stand-alone SQLite test on this btrfs, commits stall only when a WAL `fdatasync` overlaps a `fdatasync` of another file; either alone does not. Why btrfs does this was not established.
- **Decision:** a read-write gate in the store. A WAL fsync holds it exclusively; a staging fsync holds it shared (`Store::outside_wal_sync`), so staging fsyncs may still overlap each other. Checkpoints are outside the gate.
- **Alternatives:** raising the 2 ms namespace limit (accepts stalls that have a small fix); reordering `ops.py` (moves the miss to `rename`); serializing every fsync (costs more fsync latency for no known gain).
- **Consequences:**
  - Measured with the gate off and on in alternating rounds, 9 and 11 quiet rounds of 4 clients, client side: `setattr` p99 2.04 → 1.09 ms, every round under 2 ms; `chmod` right after a `fsync` 4.97 → 2.27 ms.
  - `fsync` p99 got worse, 32.1 → 37.5 ms against the 40 ms limit, p50 17.0 → 15.9 ms. Rounds above 60 ms: 4 of 11 against 1 of 9, too few to tell from disk noise.
  - The `TRUNCATE` checkpoint on the writer still stalls replies by about 40 ms (5 times in 3 rounds). Too rare to move a p99 (13 of about 12 000 `setattr`s at 10 ms or more in that run), so it stays.
  - Measured on this SATA SSD only. Nothing says the gate helps or hurts on another file system or on a spinning disk.
- **Refs:** DESIGN.md §4.3, §5.7; `crates/pdmount-core/src/store/mod.rs`, `engine.rs` (`fsync`); D032, D066, D086. Accepted by user 2026-11-01.

## D088 — No migration of passphrases stored before D085; such a profile logs in once (amends D085)
- **Status:** accepted
- **Decided by:** user
- **Date:** 2026-11-01
- **Context:** D085 moved passphrases from `sdk-secrets.db` to `sdk-keys.db` by SQL on the SDK's `Entries` table at every open. PdMount
  has no users beyond this project's own profiles, and one login restores what the move would save.
- **Decision:**
  - The move (`move_passphrases`) is removed, and with it the dependence on the SDK's table layout. The rest of D085 stands.
  - A profile with its passphrase only in `sdk-secrets.db` goes to `needs_login` once its cached keys are gone, and a login stores the
    passphrase in `sdk-keys.db`. Nothing is lost: the state and the queue stay. The old sealed copy is never read again and leaves by
    eviction or at logout.
  - The sandbox profiles `default` and `second` are cleared and start with a clean login.
- **Alternatives:** Keeping the move (code and tests for one login saved, on no one's behalf).
- **Consequences:** No profile is left with a passphrase in the old place: the user unmounted and wiped their own mount's state the same day.
- **Refs:** D085; DESIGN.md §12.4.
