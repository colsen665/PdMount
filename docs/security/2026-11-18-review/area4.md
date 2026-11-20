# M6.0.3 security review, area 4: PdMount code changed since the M5.8 review

Reviewer: area 4 (finding IDs C4-n). Read-only; nothing was built or run.

## Scope

- **Reviewed:** `git diff 976ea61 9818aa1 -- crates scripts tests` (42 files, about 3,450 added lines): the fixes of
  V-1 to V-8 (D124–D128, D115 and D117 amended), the M5 squash's OpenRC `-U` change, the `build-doctor` check, and
  M6.0.1's comment change in `proton.rs`. Line numbers are of the worktree at `9818aa1`.
- **Read in full:** `engine/marker.rs`, `engine/orphan.rs`, `engine/ignore.rs`, the changed parts of `engine/io.rs`,
  `journal.rs`, `vfs.rs` (`apply_listing`, `remove_subtree`, `bring_back`), `photos/thumbnails/frames.rs` and
  `thumbnails.rs` `decode`, `roots/shared.rs`, the changed parts of `roots.rs`, `incoming.rs`, `mounts.rs`,
  `daemon/launch.rs`, `daemon/client.rs`, `status.rs`, `doctor.rs`, `supervise.rs`, `pdmount-fuse/src/mount.rs`.
- **Dependencies read:** `image-webp` 0.2.4 (`decoder.rs` `read_data`, `read_image`, `read_frame`; `extended.rs`
  `read_extended_header`; `vp8.rs` `read_frame_header`; `lossless.rs` `decode_frame`), to compare with
  `frames::check`. OpenRC 0.64.1's `sh/supervise-daemon.sh` (how it calls `supervise-daemon --stop`).
- **Not covered:** the release pipeline, `.github`, dist and `.betterleaks.toml` (area 1); the SDK fork and upstream
  (areas 2 and 3); the test-only changes in `fake.rs` beyond a scan for switches reaching shipped builds; the docs
  except as references. `supervise-daemon`'s C source was not read. fuser's unmount path was not re-read.

## Findings

### C4-1: A gone share still deletes staged edits of an uploaded file that sits in a local-only folder (V-1 residual)

- **Rating:** High, by the Ratings' "loss of A4 triggered by untrusted input" and for consistency with V-1, whose
  no-malice variant is the same loss. The precondition is narrower than V-1's (the user's own move and edit; the
  attacker cannot cause it, only trigger the deletion), so the verifier may lower it to Medium.
- **Location:**
  - `crates/pdmount-core/src/engine/orphan.rs:91-97` (`is_local_only_file` requires `local_only && remote.is_none()`),
    `:116-121` (`local_only_staged`), `:130-136` (`export_queued` takes the journal's inodes plus those)
  - `crates/pdmount/src/roots/shared.rs:867` (the count that decides whether anything is exported), `:891` (`purge_tree`)
  - `crates/pdmount-core/src/journal.rs:1227-1232`, `:1372-1374` (`plan` is `Done` for any node whose parent is a
    local-only folder), `:1317` (`Done` removes the journal entry)
  - `crates/pdmount-core/src/engine/ignore.rs:48-49` (`reclassify` never sets `local_only` on a node with a remote)
- **Actor:** T2s (the share's owner, or whoever revokes or deletes the share); also without malice.
- **Scenario:**
  1. The user is an editor of a share. Their profile's own `.pdignore` has `build/`.
  2. In the share: `mkdir build` (a local-only folder), then `mv report.txt build/` where `report.txt` is an uploaded
     file. The move is accepted; remotely the file stays where it was (D121, test
     `a_folder_whose_child_went_into_a_local_only_folder_is_kept`).
  3. The user edits `build/report.txt`. The write goes to the staging file. `plan` returns `Done` at
     `in_local_only_folder` before it looks at the content, so the edit is never uploaded and the journal entry is
     removed. The node keeps `local_only == false` and its remote.
  4. The owner revokes the share. `SharedRoot::remove` counts `pending + parked + local_only_staged().len()`. The
     file is not in the journal and not matched by `is_local_only_file` (it has a remote), so with nothing else
     queued the count is 0, nothing is exported, and `purge_tree` deletes `trees/<key>/` with its staging.
  5. Even when other changes make the count positive, `export_queued` iterates only the journal's inodes and
     `local_only_staged()`, so this file is not exported and `retire_tree` deletes the state (no entry has an error).
- **Impact:** edits the mount acknowledged are deleted from the disk; the drive holds only the old revision, in a
  share the user can no longer read. This is the loss V-1 described, through the one class of staged content that is
  neither journaled nor flagged `local_only`. `orphan.rs`'s module doc states "nothing staged is deleted (RQ3)".
- **Fix:** make the export independent of why a staged file is not queued, as the M5.8 verifier proposed ("a gone tree
  exports every staged inode, not only the journal's"): in `local_only_staged` (renamed, e.g. `unqueued_staged`) keep
  every attached, not deleted file of `self.staged` whose node has `content_pending()` or `local_only`, and use that
  set for the count in `remove` and in `export_queued` (change `local-only` for those without a remote, a new value
  such as `not-uploaded` for the others). Add a test beside `engine/tests.rs:1525`: a remote file moved into a
  local-only folder, written, then `export_queued`.
- **Confidence:** high for the code trace; not run.

### C4-2: The marker cap prefers the lowest device UUID after every start, so one share member can keep the user's own devices' markers out (V-4 fix)

- **Rating:** Low
- **Location:** `crates/pdmount-core/src/engine/marker.rs:621-628` (order: in `previous` first, then by UUID; cut to
  16), `:670-673` (`forget_seen` clears `seen`); `seen` is memory only, so it is empty at every start.
- **Actor:** T2s (an editor of a share the user can write).
- **Scenario:** Mallory uploads 16 files `00000000-0000-0000-0000-00000000000n.pending` to the share's `.pdmount/`.
  At the next start of the user's daemon (or after the write gate closed and opened, or simply because hers were
  there first), `previous` is empty or holds hers, the sort puts her 16 first, and the markers of the user's other
  devices (random UUIDs) fall beyond the cap. They are counted as "+N more" and never read.
- **Impact:** in that share the pending-marker feature shows only the attacker's markers (each announced once, with
  her device names) and hides the real ones. Advisory data only; `status` shows the count and the log warns.
- **Fix:** persist the known device ids (the `notified` keys are stored already; store the ids with them) and prefer
  those; among unknown ones prefer the newest revision or the marker's remote modification time over the UUID. Or
  reserve part of the cap for ids seen in an earlier run.
- **Confidence:** high.

### C4-3: Info items

- **`detach` removes whatever is mounted at a root's mountpoint.** `crates/pdmount/src/roots.rs:1438-1448` runs
  `fusermount3 -u`, then `-uz`, on any mount found there; the callers (`:704`, `:1245`, `:1332`) hold a mount in
  state `Mounted`, but a session that ended from outside is noticed only by the 1 s watch, and `shutdown` calls
  `detach` even when `active.mounted.is_finished()`. A mount put there in that second (another profile's, or another
  file system) would be removed, against D127's rule. Same uid only (T5). Fix: in `detach`, act only when the entry
  is PdMount's and its `profile()` is this daemon's, and skip it when the session has finished.
- **`MountEntry::profile()` is not validated.** `crates/pdmount/src/mounts.rs:31-35` returns the text after
  `pdmount:` as read from the mount table, and `status` (`status.rs:309-318`), `doctor`, and the refusals in
  `launch.rs:179-186` print it. The kernel escapes only space, tab, newline and backslash there, so a mount of type
  `fuse.pdmount` with terminal control characters in its `fsname` prints them. Only the same uid can mount at the
  user's mountpoint (T4, out of scope). Fix: return `Some` only for text that parses as a `Profile`.
- **`shared-names.json` (V-3 fix).** `crates/pdmount/src/roots/shared.rs:143-152`: written with the umask's mode
  (protected only by the 0700 data directory, as V-22), and neither `reset` nor `logout` removes it, so share names
  and owner addresses stay after the store that held them is deleted (`daemon/reset.rs:66-99` removes `trees/`,
  staging, the store and the caches only). Also `:607` keeps names only for the shares of the current selection: a
  share whose root is degraded at the first refresh after a mount (`with_degraded`, not open yet) loses its kept
  name, and a share that arrived meanwhile with that name keeps it when the first is readable again. The degraded
  state is not under another share owner's control.
- **A node renamed remotely to `.pdmount-swap-<its inode>` in its folder is taken for this device's move aside (V-7
  fix, with V-18).** `crates/pdmount-core/src/vfs.rs:1254-1264` and `journal.rs:1740-1757` do not follow such a name;
  the node keeps its local name, its remote state takes the swap name, and the next plan (at the latest after a
  restart) renames it back. A share member who guesses the inode number (small, sequential per tree) gets only their
  own rename undone, under the user's account. No loss; noted because V-18 (swap names not unique) now also decides
  whose rename wins.
- **Marker notifications are bounded per pass, not over time (D126, accepted).** A member who writes a new marker
  revision before each poll gets one notification per pass per share: at most one per 10 minutes on another user's
  volume (`poller.rs:46`), with up to three 64-character device names in the summary (`incoming.rs:46-74`).
  `Markers::trashed` (`marker.rs:859`) grows by one link for each planted `<own id>.pending` that is trashed.
- **`is_dead` leaves a thread when `statfs` never returns** (`mounts.rs:58-73`, by design). In the daemon it runs from
  `clear_dead_mount` at each root mount, so a hung foreign mount costs one blocked thread per attempt.
- **D128, the root as the fallback place** (`vfs.rs:1963-1984`): a file whose removed folder is no longer in memory
  comes back in the tree's root under its own name. In My Files a file named `.pdignore` from a subfolder would then
  be the root's rules file. It needs a client of the user's own account and an open write handle on that file;
  shares take no rules from the root (D124).
- **Two profiles starting at the same moment (D127 residual, plausible, not traced into fuser).** `preflight` and
  `stale_action` read the mount table before the mount; nothing serialises two daemons of different profiles, so
  both can pass and stack their mounts. `service install` and `doctor` catch equal configured mountpoints, which
  covers the login case unless a config changes after the install.

## Fix completeness of the M5.8 findings

| Finding | Fix | Complete? | Pinned by |
|---|---|---|---|
| V-1 | local-only staged files counted and exported (`orphan.rs`, `shared.rs:867`) | **No: C4-1** (staged edits of a file with a remote in a local-only folder) | `engine/tests.rs:1525`, `x16.sh` |
| V-2 | `remote_ignore = spec.own` (`roots.rs:1046`); `ignore.rs:88`, `:100` | Yes: the matcher of a share is the profile's file only; `refresh_ignore` is a no-op there | `engine/tests.rs:1477`, `x16.sh` |
| V-3 | kept names (`display_names`, `shared-names.json`) | Yes for the takeover named; residue in C4-3; lookalike characters stay V-10 | `a_shown_name_stays_with_its_share` |
| V-4 | gate check, cap 16, one notice per pass, `is_own_marker_node` | Yes as decided (D126); C4-2 for the cap's order | `engine/tests/markers.rs`, `incoming.rs` tests |
| V-5 | `frames::check` before the decoder, cross-check after | Yes (see below) | `frames.rs` tests |
| V-6 | `is_dead`, `fsname=pdmount:<profile>`, refusals in every unmount path | Yes for `--clear-stale`, `--if-stale`, `preflight`, `unmount`, `unmount --force`, roots; C4-3 for `detach` and the start race | `launch.rs` tests, `svc.sh`, `tests/service.rs` |
| V-7 | `moved_meanwhile`/`found_aside`, the listing's skip | Yes: the swap name is never adopted locally | `engine/tests/swap.rs` |
| V-8 | `written_at` in `parse` and `age`; `catch_unwind` in `run_markers` | Yes for the marker task; other engine tasks stay in Known gaps | `marker.rs` tests, `markers.rs:104` |
| D128 | `removed_remotely`, `bring_back` in write and truncate | Yes for write, truncate and `O_TRUNC`; creates in a detached folder are refused (`listed_dir`) | `engine/tests/brought_back.rs`, proptest regression |

## Open findings (TODO "Known gaps"): made worse by new code?

- **V-22** (export modes): slightly. Local-only files, which are often what the user keeps off the drive on purpose
  (`secret.env` in the fix's own test), are now copied to `orphaned/<tree>/files/` with the node's mode under
  umask-mode folders. The 0700 data directory still protects them.
- **V-10** (names cleaned of control characters only): one new sink. The tree's name is now in the notification
  summary (`incoming.rs:50`, `status::printable`).
- **V-18** (swap names): a new consequence, in C4-3.
- **V-13:** improved (`doctor` warns about thumbnail folders open to others).
- The others (V-9, V-11, V-12, V-14 to V-17, V-19 to V-21, V-23 to V-33): untouched by the diff.

## Checked and fine

- **V-5 against `image-webp` 0.2.4.** `frames::check` requires the RIFF size to equal the file, a 10-byte `VP8X`
  header, and chunks that fill the body exactly, so the decoder's walk (first occurrence of each known chunk, up to
  the end of the file) sees the chunks the check saw. Chunk codes are compared byte for byte in both. A still allows
  exactly one image (the decoder would prefer `VP8L` over `VP8 `; two are refused), an `ALPH` only before a `VP8 `,
  and no `ANMF` (whose subchunks the decoder would otherwise index). An animation allows one `ANIM` and `ANMF`s only;
  each frame's first subchunk is the image, as in `read_frame`, and its size must equal its rectangle, inside a
  canvas of at most 1024 × 1024. The `VP8 ` and `VP8L` header fields are read as the decoders read them (14-bit
  masks; the lossless decoder's `+ 1`). `decode` then compares the decoder's canvas and animation flag with the
  check's. Decoding stays under `catch_unwind` in a blocking task.
- **V-8.** `written_at` uses `i64::try_from` and `checked_add`; `local_time` and `short_age` cannot panic on any
  `u64`; sums in the notification saturate. A panic in a marker pass is caught, and the next pass waits for the next
  event pass or 120 s, so no tight loop. `parking_lot` locks do not poison.
- **Marker reading.** No guard is held across an `.await` in `scan_markers`, `trash_own_marker` or
  `is_own_marker_node`; the 4 KiB cap and the field checks are unchanged; device names keep `clean_device_name`.
- **D128.** `ensure_writable` runs before the write (gate, marker folder, access rules); `bring_back` cannot loop
  (`chain.contains`), indexes only nodes it just found, picks a conflict name when the name is taken locally or
  remotely, and is not reached for local removals (`unlink`, a replaced rename target, `queue drop`). Inode numbers
  come from a monotonic allocator, so a stale `parent` cannot name another node.
- **V-7.** `found_aside` requires the same link, the node's own swap name and its known folder; the tree lock is
  released before `persist`.
- **V-6.** Profile names are `[a-z0-9_-]`, so `fsname=pdmount:<profile>` and the mount table need no escaping;
  `parse_line` takes the source after the `-` separator and unescapes it; `stale_action` asks `is_dead` only when no
  daemon of the profile answers or runs; a hung mount counts as live (refused, never detached).
- **OpenRC `-U`.** `openrc_stray_text` interpolates only the validated profile name; nothing is executed by PdMount.
  The generated script's own `supervise-daemon "$RC_SVCNAME" --stop` (without `-U`) matches what OpenRC 0.64.1's
  `supervise-daemon.sh` does inside a service script.
- **`doctor`.** `profiles_sharing_a_mountpoint` reads only names that parse as profiles and skips configs that do
  not load; the new checks execute nothing.
- **`save_names`/`load_names`.** A fixed temp name in the private data directory, renamed over the file; a broken or
  foreign file yields no names (no panic); keys and names are used only for comparison and as shown names that
  passed `usable`.
- **Scripts.** `build-doctor.sh` uses `mktemp -d` and removes it; no new environment switches in shipped code; the
  `proton.rs` change is a comment.
