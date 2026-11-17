## Area C5 (engine, journal, cache and FUSE layer, M5 changes): findings

Reviewed read-only at commit 976ea61, `git diff 8b01e65..976ea61` (8b01e65 = the M4 release on `master`), with the behaviour
changes found through `git log -p` and the commit messages rather than the moved code of the module splits (b92ee18,
8302c95, 3ddcd79): `crates/pdmount-core/src/engine/` (open, io, access, rights, scan, background, prefetch, cache_glue,
accessors), `journal.rs` (D116, D117, D121, D105), `cache/` (`block_cache.rs`, `disk.rs`, `budget.rs`, `layout.rs`),
`scan.rs`, `gate.rs`, `events.rs` and the parts of `poller.rs` it now calls, the store schema 4 (`store/0004.sql`),
`crates/pdmount-fuse/src/` (`lib.rs`, `mount.rs`, `convert.rs`, `router.rs`, `trees.rs`), `crates/pdmount/src/daemon/reset.rs`,
and, for context, `vfs.rs` (`yield_name`, `adopt_remote_placement`), `pool.rs` and `crates/pdmount/src/roots{.rs,/shared.rs}`.
Decisions D105, D110 (cache part), D111, D116, D117, D118, D120, D121. Threat model: `docs/security/threat-model.md` (D070).
Nothing was run (no build, tests or scenarios); every scenario below is traced by reading. Known gaps in `TODO.md` (S-n,
R-n, the D110 cache gaps) are not repeated.

### C5-1: A swap move retried after a lost response renames the user's node to `.pdmount-swap-<ino>` for good, silently
- **Rating**: Medium
- **Location**: `crates/pdmount-core/src/journal.rs:1376-1391` (the planner's `blocked`: `Work::Move` to `swap_name(ino)`, the
  node's local name unchanged); `journal.rs:1607-1650` (`move_node`: the fresh node no longer at the known placement goes to
  `remote_moved_first`); `journal.rs:1652-1665` (`NameConflict` after the SDK's own retry, S-25, takes the same way);
  `journal.rs:1717-1733` (`remote_moved_first`: "the other device made the very same move" when the fresh node is at
  `(target, name)`); `journal.rs:1780-1799` (`take_remote_placement`) and `crates/pdmount-core/src/vfs.rs:1893-1914`
  (`adopt_remote_placement_in`: unplaces the inode and places it at the remote name).
- **Actor**: T3 (a lost or late response), also a crash in the group-commit window after the rename.
- **Scenario**: the user swaps two names (`mv a t; mv b a; mv t b`), or makes any cycle D117 now breaks (three names rotated,
  `mv d b; mkdir d; mv b d/b`, ...). The planner sends node X (ino 42) to `.pdmount-swap-42`. The server applies the rename,
  but the response is lost (timeout, a 5xx after the commit, or the SDK's 5xx retry resending the stale `OriginalHash`, which
  answers `NameConflict`, S-25). The step fails transiently; X's stored remote name is still `a`, so the next plan is the
  same swap move. `move_node` now fetches X at `.pdmount-swap-42`, which differs from the known placement, and calls
  `remote_moved_first(.., target = parent, name = ".pdmount-swap-42")`. Its "same move" branch assumes that the move's
  target is the node's local placement, which held for every move before the swap existed: it calls
  `adopt_remote_placement`, which moves the local inode from `b` to `.pdmount-swap-42` and records no conflict. X is then
  "done" (local = remote), Y takes `a`, and the user's `b` is gone: its content now lives under a hidden name, locally and on
  every device and share member, with nothing in `pdmount conflicts` or the log above debug level. The same happens after a
  crash between the rename and the commit of X's row, when the journal plans before the event pass has applied the echo.
  The trash path (`move_aside`, `journal.rs:1890-1950`) handles this case correctly (`moved` → only the remote state
  follows); the planner's path does not. The two-name swap had the same flaw before M5; D117 extends it to every cycle.
- **Impact**: A4 integrity: a file or folder the user named silently ends up under a dot name the user never chose; no
  content is lost. Needs a lost response at the wrong moment, so not High.
- **Fix**: in `move_node`, when the fresh node is already at the planned `(target, name)` and that is not the node's local
  placement (always so for a swap), record the fresh placement as the node's remote state only, as `move_aside` does, and
  return `Ok`/`Replan` without `adopt_remote_placement`; or let `remote_moved_first`'s "same move" branch adopt only when
  `(node.parent, node.name)` equals the target. Add a test with a fake fault that applies a rename and then fails it, for
  the swap of two and for one D117 cycle.
- **Confidence**: high on the code path; the fake has no "applied, then failed" fault, so it is not covered by a test.

### C5-2: `.pdmount-swap-<ino>` is not unique: a node holding that name renames the user's file to a conflict name or stops the cycle
- **Rating**: Low
- **Location**: `journal.rs:1191-1193` (`swap_name`: the device's inode number only); `journal.rs:1666-1682` (`move_node`:
  `AlreadyExists` → `yield_to_remote`); `journal.rs:2454-2493` (`yield_to_remote` → `Tree::yield_name`,
  `vfs.rs:1837-1864`: renames the local node to a conflict name); `journal.rs:1890-1920` (`move_aside`: `AlreadyExists`
  is a failed step, retried by back-off).
- **Actor**: T2: another device of the account (each device numbers its own inodes from 2 upwards, so devices that
  mounted the same tree get similar numbers), a `.pdmount-swap-<n>` left by a device that crashed or was retired, or a
  member of a share with write access, who can create such names in advance.
- **Scenario**: X (ino 42) must move aside, and the remote folder already holds a node named `.pdmount-swap-42`. The
  rename answers `AlreadyExists`, and `move_node` treats it as "the node's name is taken": `yield_to_remote` renames X
  *locally* to `<its local name> (conflict …)`, records a `NameTaken` conflict and lists the folder. X's local name was
  never taken (only the temporary one), so the user's file gets a conflict name that then uploads to every device. For a
  deleted folder whose trash waits (`move_aside`), the same answer is a failed step that repeats with back-off: the
  folder is neither moved aside nor trashed, so the cycle D117 breaks stays stuck, the node waiting for the name never
  gets it, and the entry parks or keeps failing.
- **Impact**: A4: an unrequested rename of a user's file, or a cycle of pending changes that never completes. No loss.
- **Fix**: make the temporary name unique per device and attempt (for example `.pdmount-swap-<device id prefix>-<ino>`,
  plus a counter or a random suffix on `AlreadyExists`), and never answer an `AlreadyExists` of a swap move with
  `yield_to_remote`: choose another temporary name instead, in `move_node` and `move_aside`.
- **Confidence**: high on the code path; the likelihood of a collision between devices is not measured.

### C5-3: A moved-aside folder that is kept comes back, for everyone, as the hidden `.pdmount-swap-<ino>`
- **Rating**: Low
- **Location**: `journal.rs:1826-1834` (a waiting trash moves the folder aside); `journal.rs:1980-2070` (`trash_check`: new
  children → `LocalDeleteRemoteChildren`, `Kept`); `journal.rs:2076-2104` (`keep_remote`: unbinds the tombstone and lists
  the parent, so the folder appears at its remote name, the temporary one; the conflict names the folder's old name).
- **Actor**: T2: another device of the account, or a member of a writable share, adding to a folder the user deletes.
- **Scenario**: the user runs `rm -rf d; mkdir d` (or any change that places a node at a deleted folder's name). Following
  D117, `d` is renamed remotely to `.pdmount-swap-<ino>` before its trash. Meanwhile another device or a share member
  puts files into `d`. The trash check finds children the delete did not know about and keeps the folder: it reappears
  locally and stays remotely under `.pdmount-swap-<ino>`. On the other devices and for the other members of the share, the
  folder they were writing into was renamed to a hidden dot name by someone else's device, with their new files inside;
  file managers and `ls` do not show it. The conflict record exists only on the deleting device and names `d`, not where
  the files went.
- **Impact**: A4: files of other devices or users hidden under a name no one chose; no loss (D117 notes the trash name
  and the `queue drop` case, not this one).
- **Fix**: in `keep_remote`, when the tombstone's remote name is its swap name, rename the remote folder to a visible
  conflict name (`d (conflict <device> <time>)`, the `pick_conflict_name` rule) before listing the parent, and put that name
  in the conflict record; mention the case in DESIGN §8.4 and the user guide.
- **Confidence**: high on the code path; the rename back is a design choice for the user.

### C5-4: Every further tree reserves about 20 MiB of resident memory, so other users' shares set up to about 640 MiB
- **Rating**: Low
- **Location**: `crates/pdmount/src/roots.rs:1050` (each further tree's `ram_bytes: MIN_RAM_BYTES`, 16 MiB);
  `crates/pdmount-core/src/pool.rs:1-12`, `:135-174` (the pool maps and populates its regions at creation, plus four
  1 MiB scratch slots); `crates/pdmount/src/roots/shared.rs:45` (`MAX_SHARES` = 32); `crates/pdmount-fuse/src/trees.rs:28`.
- **Actor**: T2: other Proton users sharing folders or files with the user.
- **Scenario**: a Shared mount opens one engine per share, each with its own populated block pool of about 20 MiB,
  whether the share is ever read. 32 shares give about 640 MiB of resident memory in the daemon, plus each engine's
  store, tree and tasks; the number is set by how many people share with the user, not by the user. Not traced: whether
  a share reaches the shared-with-me listing without the user accepting an invitation; if it does, any Proton user can
  cause this. D110 already says a shared pool may be needed (left for M5.12); this is the security view of it.
- **Impact**: memory use from remote input, bounded by `MAX_SHARES`; not exhaustion on most desktops.
- **Fix**: give further trees one pool shared by the session, or create a tree's pool lazily on its first block read and
  release it after an idle period; at least, map the pool without populating it for further trees.
- **Confidence**: high on the reservation; medium on who can add shares.

### C5-5: The volume pollers and the engine's tasks are not restarted after a panic; with D110 one panic stops every tree's sync
- **Rating**: Low (defence in depth; no remote-triggered panic was found)
- **Location**: `crates/pdmount-core/src/poller.rs:394` (one task per volume runs `sync_trees` for all trees on the volume,
  My Files included with `[[roots]]`), `:467` (the core stream's task); `crates/pdmount-core/src/engine/background.rs:71-125`
  (journal, health, pins, markers, ... spawned once); only the Photos stages are restarted (D119, `photos/sync.rs:49-100`).
- **Actor**: T2, if any remote input reaches a panic in `apply_events`, the listing code or the journal.
- **Scenario**: before M5 a panic in one engine's event task stopped that engine's sync. Now `sync_trees` applies each page
  to every tree on the volume in one task, so a panic while applying a page to one share stops the event sync of all the
  volume's trees, and with `[[roots]]` of My Files too, until the daemon restarts; a panic in `run_journal` stops all
  uploads of its tree while writes are still accepted. Neither shows in `status` (only `handler_panics` for FUSE callbacks
  exists). `parking_lot` locks do not poison, so the rest keeps running on whatever state the panic left.
- **Impact**: silent loss of sync (not of data: staged changes stay and upload at the next start).
- **Fix**: run the pollers and the engine's long-lived tasks under a supervisor like D119's (catch the panic, count it in
  `stats`/`status`, restart after a delay), and apply a page per tree inside `catch_unwind` so that one tree's panic is
  that tree's failed page (then refreshed) rather than the volume's.
- **Confidence**: high on the missing restart; no panic path from remote input shown.

### C5-6: Minor hardening (Info)
- `crates/pdmount/src/daemon/reset.rs:132-146`: for a further tree whose `state.db` cannot be read (a newer schema, a corrupt
  file), `trees_with_changes` falls back to counting staged files with no message, so the tree's pending renames, deletes
  and folder creates are deleted with `trees/` without the warning My Files gets (`reset.rs:24-31`). Print the same warning,
  or refuse unless `--orphan-trees` keeps the state.
- `crates/pdmount-fuse/src/trees.rs:28`, `:105-112`: slots are never reused (good), so a share that its owner removes and
  adds again uses up one of 65 534 slots each time; after that, new shares stop showing until the mount is remounted.
  Needs tens of thousands of listing changes; log when fewer than, say, 1 000 slots are left.
- `crates/pdmount-core/src/cache/block_cache.rs:487-507` (`get`): a scan flight inserts its raise signal into `raises` under the
  flights' lock and removes it after the class permit; a flight whose future is dropped before that leaves its entry
  (one `Notify` per block key). Remove it in a drop guard.
- `crates/pdmount-core/src/journal.rs:1191`: the temporary names expose the device's inode numbers to other share members
  (not sensitive; one more reason for C5-2's per-device name).

## Checked and fine (area C5)
- **Read integrity after the split.** Every remote block still goes through `BlockCache::get` → `download`
  (`block_cache.rs:473-579`): `backend.fetch_block_into` (the fork's block hash and manifest checks, P2), then the length
  against the layout; a failed block is never cached. Scan reads (`Priority::Scan`), a scan flight raised to interactive
  (D120), readahead, prefetch, pins and materialization all use that one path; disk hits are authenticated
  (XChaCha20-Poly1305, AAD = link, revision, index) and a failing file is dropped and fetched again. `Layout::new` still
  refuses blocks over 4 MiB, empty middle blocks and sums other than the file size; nothing else sizes a buffer. The
  stored layout read on the hot path is R-15 (known).
- **No block crosses trees.** Each tree has its own `BlockCache`, directory (`trees/<key>/blocks/`, key = BLAKE3 of volume
  and root) and `cache_blocks`/`revision_blocks` rows in its own store; nothing looks a block up in another tree's cache.
  The cipher is shared (same cache key, D110), but the AAD's `(link, revision, index)` names the content server-wide (link
  ids are global, the fake's too), so even a file copied between tree directories (same-uid only) would decrypt only as
  the same content. The scan guard evicts by `(seat, key)` through the holding tree's own `evict_scan_block`.
- **Disk and memory bounds.** `DiskBudget` (`budget.rs`): one enforcement at a time, taken before any index lock;
  members hold `Weak` caches and leave when the cache drops; the loop tries each member once, so it ends; My Files'
  share is half the cap and others evict it no further. Pinned blocks are outside the cap and further trees cannot pin
  (path commands reach My Files only); the pin reserve gap is R-9. The scan share is capped at `cache_share` percent of
  the cap, forgets blocks that leave the disk or get pinned (D120), and only a user's programs start a scan. Small-file
  prefetch stays once per directory and within `prefetch_dir_budget`. A gone share's state and blocks are purged by key
  (`roots.rs:987-1004`). `queued` (schema 4) has one row per journal entry, deleted when the entry leaves and at mount
  for inodes without one; restored ages are clamped to 0 .. 365 days.
- **FUSE layer.** Mount options unchanged for every mount kind: `nodev`, `nosuid`, `noatime`, no `allow_other`, no
  `default_permissions` (D031), `ro` where the mount is read-only (`mount.rs:136-148`, `readonly.rs:53-59`). No new FUSE
  capability flag. The virtual top level is 0555 and answers EROFS to every change, `access(W_OK)` included; a rename
  between trees is EXDEV; chown to another uid is EPERM; `mknod`, `symlink`, `link`, `setxattr`, `fallocate` stay refused.
  Routed inode numbers above 2^48 are refused (`router.rs:76-86`), slot numbers are never reused, so a handle or inode of a
  removed tree reaches no other tree (ENOENT on a name, EIO on a handle, `release` always succeeds). Every callback still
  runs in `catch_unwind`; slow-path tasks answer EIO when their Reply drops.
- **Write gate.** Checked at each write entry: `open` for writing and every `write_local` (handles opened before a
  closing gate are refused too), `create`/`mkdir`/`unlink`/`rmdir`/`rename`/`setattr` through `ensure_writable` and
  `ensure_dir_writable`, and `access`. A retired engine answers a change EIO when its commit ends after the retirement
  (`accessors.rs:112-138`), and a poll pass skips it (`poller.rs:243-249`).
- **Journal waits (D116, D121).** A child whose wait leaves at once no longer wakes its folder; a child in a local-only
  folder keeps the folder instead of looping; waits back off up to 5 min and are counted apart from attempts; the cycle
  walk is bounded (4096 steps) and stops at tombstones. A hold covers entries made while it lasts and lifting it retries
  only its own parks. My Files never follows the server's write refusals (`rights.rs:56-60`).
- **Events and gate (D120).** No rate goes up: patient turns still queue and keep the 500 ms spacing; `background_within`
  can go without a turn after its patience, but scan downloads stay one at a time (one scan permit). The refresh task is a
  background task now, stops at a shutdown and ends when the tree's root is gone or refused, so a revoked share no longer
  lists itself every 30 s. Retry-After is still honoured by the poller's back-off (`poller.rs:49-58`; its missing upper
  bound is S-10).
- **`reset` and `--orphan-trees`.** Every path comes from `ProfilePaths` (restricted profile name, hex tree keys); no remote
  name becomes a path. Refuses while the daemon runs or My Files is mounted. `std::fs::remove_dir_all` removes a symlink
  itself without following it, `dir_size` uses `DirEntry::metadata` (no traversal), and `--orphan-trees` moves a tree's
  directory by `rename` inside the 0700 data directory, so nothing outside the profile's directories is read, written or
  deleted.
