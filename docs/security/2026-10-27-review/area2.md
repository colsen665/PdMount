## Area 2 (FUSE adapter, namespace and permission logic): findings

No Critical/High in this area per the reviewer. Remote names never reach local paths (staging by ino, `staging.rs:76-85`; cache by hash, `cache.rs:393-395`); names filtered by `representable()` (`vfs.rs:574-580`); mount `nosuid,nodev`, no `allow_other`.

### A2-1: Remote `pdmount.mode` keeps setuid/setgid/sticky bits
- **Rating**: Low (duplicate of A4-4)
- **Location**: `crates/pdmount-core/src/vfs.rs:73`, `:99`; `crates/pdmount-backend/src/proton.rs:222-226`; `crates/pdmount-fuse/src/lib.rs:291`; `engine.rs:926-927`.
- **Fix**: mask remote modes with 0o777 in `RemoteState::from_remote`/`with_content_of`, or clear 0o7000 in `file_attr`.
- **Confidence**: high (bits pass), low (matters today)

### A2-2: A huge remote `/.pdignore` slows every create/mkdir/rename on the hot path
- **Rating**: Low (DoS, T2)
- **Location**: `crates/pdmount-core/src/engine.rs:2327-2350` (`keep_ignore_rules`, up to 1 MiB), `pdignore.rs:106-119` (≤4 globs per line), `engine.rs:2255-2268` (`is_ignored` under `tree.read()`), `engine.rs:2272-2299` (`reclassify`: subtree × rules).
- **Attack**: 1 MiB `/.pdignore` with ~100k patterns → ~400k matches per create under the tree read lock.
- **Fix**: cap rules (e.g. 1,000 lines / 64 KiB) with a log line; drop the tree lock before matching.
- **Confidence**: medium

### A2-3: Remote-written `/.pdignore` silently makes new local files local-only
- **Rating**: Info (design D018)
- **Location**: `engine.rs:2330-2347`, `:724,738`, `:756,760`; `journal.rs:595-600`.
- **Attack**: another device writes `*` into `/.pdignore`; every new local file stays local-only, unnoticed.
- **Fix**: info log with rule count when rules change; count in `status`; maybe a desktop notice.

### A2-4: `apply_listing_at` can panic mid-mutation and leave the directory empty in RAM
- **Rating**: Low, uncertain
- **Location**: `crates/pdmount-core/src/vfs.rs:1393` (`new_inos.pop().expect("counted above")`), count `:1198-1202`, `remove_subtree` `:1283-1291`, `:1227` (`mem::take` of children), `follows` `:1240-1245`.
- **Attack**: folder F and file X in listed dir D; remote moves F away; listing of D fetched (shows X, not F); before apply, the executor completes a local move of X into F and stamps X (`changed_since`); apply skips X in `follows`, `remove_subtree(F)` unindexes X; incoming pass finds X without an inode, `unknown` did not count it → `expect` panics. D's children already taken; D looks empty until relisted.
- **Impact**: files vanish from view; slow-path task dies (EIO); no persisted loss found.
- **Fix**: compute removals first and count incoming links that will be unindexed, or keep stamped nodes; proptest with a stamped node under a vanished folder.
- **Confidence**: low-medium (traced by reading). NOTE: related to the G6 fix in 29355a6 (`a_node_moved_out_of_a_vanished_folder_survives_the_listing`).

### A2-5: Control characters in remote names reach logs and the CLI unescaped
- **Rating**: Info
- **Location**: Display-formatted names in logs `vfs.rs:1370-1373`, `engine.rs:421`, `:1458`; CLI `crates/pdmount/src/conflicts.rs:83-96`, `:106-134`; `access list`.
- **Attack**: name with `\n` forges log lines; `\x1b]0;…` escape sequences when `conflicts list` prints.
- **Fix**: `?name`/`?path` in tracing; escape control characters in CLI output on a tty.
- **Confidence**: high

### A2-6: Kernel caching limits the daemon-side permission checks (D031)
- **Rating**: Info (same uid)
- **Location**: `engine.rs:2052-2062`, `:907-947`; `lib.rs:959-961`; `lib.rs:321-328`, `engine.rs:966-968` (`O_RDONLY|O_TRUNC` accepted without write check, truncate ignored).
- **Fix**: document advisory mode bits under caching in §11.2; optional entry invalidations on losing `x`; treat `O_TRUNC` as a write in `open_mode`.

### Note (outside area): `crates/pdmount-backend/src/proton.rs:264-273` unchecked u64 sum of remote claimed block sizes; wraps in release. Use `saturating_add`.

## Checked and fine (area 2)
- Mount options (`lib.rs:956-965`), uid/gid of daemon; only regular files and dirs, rdev 0; mknod/symlink/link EPERM.
- Panic containment: `catch_unwind` on hot path; slow-path panics drop the Reply → EIO; parking_lot locks (no poisoning).
- Errno mapping sane; `deferred_error` only ENOSPC/EFBIG.
- Kernel names `validate_name` (`vfs.rs:562-571`) on all entry points; remote names `representable()`; byte-exact keys.
- Cycles refused (`vfs.rs:1243`, `:1717`, `:1902`); unreachable rows dropped.
- Conflict names sanitized, ≤255 bytes (`conflicts.rs:79-124`).
- Local paths never from remote names; `resolve()` rejects `..` (`engine.rs:2104-2118`).
- Integer handling saturating where needed.
- D031 checks on lookup/opendir/create/mkdir/unlink/rmdir/rename/setattr/open/write/access (`engine.rs` refs in report).
- `.pdignore` upload guard (`journal.rs:595-600`, `engine.rs:887`, `vfs.rs:1983`, `surface.rs:289-299`, `engine.rs:2285-2287`).
- xattrs: only five `user.pdmount.*`; ERANGE protocol correct.
- readdir snapshot offsets; invalidations on own thread.
