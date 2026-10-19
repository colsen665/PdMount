# pdcli mount evaluation

This document evaluates the FUSE mount in **pdcli 0.4.0** by Thribhu K (MIT, repository
`github.com/tirbofish/proton-sdk-rs2`). The same author maintains the community SDK that PdMount builds on. The evaluation explains
why PdMount is a separate project rather than a fork, what PdMount borrows from pdcli, and how the two projects relate.

The user verified in practice that pdcli's write-sync behaviour breaks `rsync` and `git` workflows on the mount. Those two tools
are therefore PdMount's primary acceptance tests (see DESIGN.md §15–16).

Path prefixes used below:

| Prefix | Location |
|---|---|
| `pdcli/` | `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/pdcli-0.4.0/src/` |
| `sdk/` | `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/proton-drive-sdk-0.4.0/src/` |

Version 0.3.1 has the same `fs.rs`/`db.rs` logic. 0.4.0 adds only upload progress tracking and download
cancellation.

All findings come from reading the source. The data-loss paths in §3 were traced through the code but not reproduced individually. The user's
practical observation (rsync and git break) is consistent with them.

## 1. Architecture

| Aspect | pdcli 0.4.0 | Evidence |
|---|---|---|
| FUSE API | Synchronous `fuser::Filesystem` (fuser 0.17). The `experimental`/`tokio` features are enabled but never used | `pdcli/fs.rs:844` |
| Threads | `spawn_mount2` with `n_threads = 8` | `pdcli/fs.rs:31`, `:2432` |
| Mount options | `FSName` only: no `auto_unmount`, no `default_permissions` | `pdcli/fs.rs:2432` |
| Mountpoint | Hard-coded `~/ProtonDrive`, virtual `MyFiles/` and `Computers/` folders | `pdcli/fs.rs:84` |
| Metadata state | No in-memory cache. Every lookup/getattr runs SQL under one global `Arc<Mutex<FuseDb>>` | `pdcli/fs.rs:123` |
| Kernel TTL | 0, so the kernel re-asks for every attribute | `pdcli/fs.rs:26` |
| Inode numbers | SQLite `AUTOINCREMENT`, persistent and never reused (a good choice) | `pdcli/db.rs:111-131` |
| Read path | The whole file is downloaded in `open()`, and the FUSE thread blocks on a channel with no timeout | `pdcli/fs.rs:537-560` |
| Content cache | `~/.config/pdcli/fuse_cache/`, plaintext, no eviction, two naming schemes | `pdcli/fs.rs:455`, `:1302` |
| Write path | Writes go to the local copy. `release()` queues an upload; a 5 s rescan also re-queues dirty files that are still open | `pdcli/fs.rs:1181`, `:1240`, `:1906-1910` |
| Journal | SQLite table with JSON payloads (`create_file`, `update_revision`, `rename`, `move`, `delete`…), 4 parallel uploads | `pdcli/fs.rs:1972`, `pdcli/db.rs:586-589` |
| Remote changes | Polls the My Files volume events every 15 s, with the cursor persisted | `pdcli/fs.rs:1743` |
| Thumbnail guard | Walks the requesting process's ancestors in `/proc` and denies downloads to file managers and thumbnailers | `pdcli/thumbnail.rs:185` |

## 2. Behaviour that breaks rsync and git

| # | Defect | Effect on rsync / git | Evidence |
|---|---|---|---|
| 1 | **st_size is not the plaintext size.** The SDK fills `claimed_size` from the revision's `storage_quota_consumption` and `claimed_modification_time` from the revision's `creation_time`, without decrypting the revision XAttr. pdcli uses these values as-is | Files on a second client or after a cache wipe have the wrong size and mtime, so the rsync quick check always differs and git sees modified files | `sdk/node/mod.rs:1386-1418`, `pdcli/fs.rs:371-372` |
| 2 | **`.git/` is in the default global ignore list**, together with `*.tmp`, `*.bak`, `build/`, `target/`, `node_modules/` | Git repositories on the mount are never uploaded | `pdcli/pdignore.rs:9-50` |
| 3 | **Renaming a source that was never uploaded is dropped.** A rename is journaled only if the source already has a `node_uid` | rsync's `.name.XXXXXX` → `name` and git's `index.lock` → `index` produce nothing remotely, or the wrong state | `pdcli/fs.rs:1583-1604` |
| 4 | **An ignored temp file renamed over a target trashes the original.** At release the temp is ignored and marked clean, and the rename queues a trash of the target | Editors and tools that save atomically lose the remote file | `pdcli/fs.rs:595-599`, `:1577-1581` |
| 5 | **Partial uploads, then dirty is cleared.** Files still being written are uploaded. `set_dirty(false)` afterwards has no generation check. The echoed UpdateContent event then deletes the local copy | Large `cp`/rsync writes and git packs can end up truncated remotely, and the full local copy is gone | `pdcli/fs.rs:1906`, `:2230`, `:2315`, `:630`, `:1839-1842` |
| 6 | **A remote UpdateContent wipes unsynced local edits.** The handler never checks dirty or pending state | Edits from another device or the web app silently destroy local work | `pdcli/fs.rs:1839`, `pdcli/db.rs:508` |
| 7 | **setattr ignores mode, uid/gid, atime and mtime** but reports success; permissions are hard-coded 0644/0755 | `rsync -t`/`-p` never converge. git's `core.filemode`, 0444 object files and freshening do not persist | `pdcli/fs.rs:211`, `:915-966` |
| 8 | **Rename flags are ignored.** `RENAME_EXCHANGE` is treated as replace-and-delete | Data loss for callers that rely on `renameat2` semantics | `pdcli/fs.rs:1537` |
| 9 | **Rename over a directory that was never listed** looks empty and is trashed | Silent loss of a remote subtree | `pdcli/fs.rs:1573` |
| 10 | **Partial downloads look complete.** Downloads write to the final path, so after a crash `exists()` serves the truncated file | Corrupt reads after a crash, and git object or pack errors | `pdcli/fs.rs:516`, `:544-557` |
| 11 | **Remote renames and moves are not applied** to existing nodes | The tree goes stale relative to other devices | `pdcli/fs.rs:389-397` |
| 12 | **The event loop returns for good** if startup fetches fail | No remote sync, and no offline detection, for the rest of the session | `pdcli/fs.rs:1759-1760`, `:1778-1779` |
| 13 | **No conflict detection.** Uploads target the revision recorded when the job was queued, and a conflict is retried 10 times and then failed | Lost updates across devices | `pdcli/fs.rs:2192-2209` |
| 14 | **No `fsync`** (ENOSYS), and `flush` only fsyncs the local file | git logs fsync errors | `pdcli/fs.rs:1213` |
| 15 | **Thumbnail-guard false positives.** The `/libexec/` pattern matches `gnome-terminal-server`, so `cat` of an uncached file in a terminal can get EACCES | Confusing EACCES errors in a terminal | `pdcli/thumbnail.rs:74`, `:82` |

The following structural issues are not rsync- or git-specific but block the latency goal:
- The global DB mutex serializes every operation.
- Blocking `rt.block_on` calls run inside FUSE callbacks (`pdcli/fs.rs:606`).
- Directory population is a busy-wait of up to 200 × 50 ms (`pdcli/fs.rs:343-359`).
- 82 `.unwrap()` calls on locks: one poisoned mutex brings down every FUSE thread.

## 3. What PdMount reuses vs redesigns

| Area | pdcli | PdMount decision |
|---|---|---|
| Persistent inode table, never reusing numbers | Yes | **Keep the idea.** New schema (DESIGN.md §12) |
| Persistent operation journal | Typed JSON payloads, with remote ids captured at enqueue time | **Redesign.** Desired-state reconciliation per inode; dependencies resolved at run time |
| Event cursor persisted | Yes | **Keep the idea.** Add `more`/`refresh` handling, apply remote renames and moves, retry forever, suppress own echoes |
| SQLCipher DB keyed from the keyring | Yes | Keep the keyring master key. Block cache encrypted per block (XChaCha20-Poly1305) |
| `credentials.rs` (keyring + 0600 file fallback) | Good | **Adapt** under MIT with attribution; separate keyring service `pdmount` |
| `pdignore.rs` matcher | Good matcher, harmful default list | **Adapt** the matcher. Default list empty; "ignored" means local-only, never trashing remote |
| `service.rs` (systemd user unit) | Good | **Adapt**; add `ExecStopPost=fusermount3 -uz` |
| `statfs` from cached quota | Good | Keep |
| Thumbnail guard by process name | False positives | **Replace** with prefetch budgets and priority classes. If needed later, an opt-in rule set limited to known thumbnailer binaries |
| Whole-file download on open | Blocks; no range reads | **Replace** with a block cache keyed by revision and block index, readahead and prefetch |
| Sync trait + global mutex + TTL 0 | Slow | **Replace** with RAM inode table, per-directory locks, non-zero TTLs and kernel invalidation |
| Conflict handling | None in the mount | **New**: base-revision tracking and keep-both conflict copies (DESIGN.md §10) |

## 4. Relationship going forward

- **pdcli remains usable** for CLI tasks (transfers, shares, photos, Computers backups). PdMount is mount-only.
- **Coexistence:** separate config directories (`~/.config/pdmount/` and `~/.config/pdcli/`), separate keyring services (`pdmount` and
  `pdcli`), separate sessions and refresh tokens, and separate default mountpoints. Both can run at the same time.
- **SDK fixes go upstream** (docs/sdk-patches.md), so pdcli also benefits, notably from P1 (files over 40 MiB truncated) and P4
  (wrong st_size/mtime). The fork exists only to carry patches until they are merged.
- Mount-level defects (§2) can be reported to the pdcli maintainer with references to this document. PdMount does not depend on
  pdcli's code being fixed.
