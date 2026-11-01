# PdMount — Design

| | |
|---|---|
| Status | Draft 1, design phase. Current status and next steps: [TODO.md](TODO.md) |
| Decisions | [docs/decisions.md](docs/decisions.md) (who decided what, and why) |
| Date | 2026-10-19 |
| Scope | Proton Drive "My Files", mounted read-write on Linux through FUSE |
| Language / libraries | Rust (edition 2024), fuser 0.18, proton-drive-sdk / proton-sdk-rs2 0.4 (patched fork), tokio, rusqlite |

Source citations use these prefixes:

| Prefix | Location |
|---|---|
| `fuser/` | `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/fuser-0.18.0/src/` |
| `sdk/` | `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/proton-drive-sdk-0.4.0/src/` |
| `rs2/` | `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/proton-sdk-rs2-0.4.0/src/` |
| `pdcli/` | `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/pdcli-0.4.0/src/` |
| `cs/` | `<official SDK checkout>/` (official Proton Drive SDK, C# and JS, used as the behaviour reference; clone of `https://github.com/ProtonDriveApps/sdk` at tag `js/v0.21.0` = `e6a661f5`, cs 0.24) |

---

## 1. Goals, non-goals, requirements

### 1.1 Requirements

| ID | Requirement | Where addressed |
|---|---|---|
| RQ1 | `rsync` and `git` work correctly in and out of the mount. Their scenarios are the milestone functional tests | §15, §16 |
| RQ2 | Low latency: working in the mount feels close to a local disk | §5 |
| RQ3 | Use from several devices at once (PdMount instances and the official apps). Conflicting changes are expected and must never lose data | §10 |
| RQ4 | Per-path access control: a path and its sub-entries are Read or Read+Write. Rules are changed and listed at runtime with the CLI | §11 |
| RQ5 | Web authentication either opens the link in the default browser or only prints it on the command line | §12.2 |
| RQ6 | Crash-safe: acknowledged writes survive a daemon crash; fsync'd writes also survive a power loss; both are uploaded later | §4.3, §5.6, §7, §9 |
| RQ7 | A mount never hangs indefinitely, and recovery never needs a reboot | §4, §14.4 |
| RQ8 | Scope is My Files read-write; from M5, Computers and writable shares read-write, and other shares and Photos read-only, as further roots (D078–D081) | §14.5 |

### 1.2 Non-goals (v1)

- Trash as a browsable tree. (Shared-with-me, Computers/Devices and Photos were non-goals until M5, D078.)
- Symlinks, hardlinks, device nodes, and arbitrary extended attributes.
- Multi-user mounts (`allow_other`); Windows and macOS.
- Real-time collaboration semantics. Concurrent writers on the same file get conflict copies, not merges.

### 1.3 Design principles

1. **Local first.** The local metadata store and staging area are authoritative for what the kernel sees. The network is
   never on the hot path (§5).
2. **Desired-state reconciliation.** The journal records *what the remote side should look like* per inode, not a list of
   past operations. Coalescing and reordering follow from that (§8.2).
3. **Never destroy a version.** Every byte acknowledged to an application ends up at its name, in a conflict copy, in revision
   history, or in the Proton trash (§10).
4. **Bounded everything.** Deadlines on every slow path, bounded queues, bounded caches, and backoff on every retry.
5. **The SDK sits behind a trait.** Gaps in the community SDK are fixed in a fork and upstreamed. The core never sees SDK types (§3).

---

## 2. Background and findings

### 2.1 Prior attempts

| Attempt | Approach | Outcome | Lesson |
|---|---|---|---|
| protonfuse (Aug–Sep 2026) | Rust/fuser 0.18, drove the official `proton-drive` CLI as a subprocess REPL; whole-file download on open | Read-only. 1–2 s per operation; rsync of a 200 KB repo took 3.5–6.5 min. Mounts hung and once required a reboot | No subprocess. Resolve by node id, not by path from the root. No global mutex. Never panic in callbacks |
| Official CLI (Bun build of ProtonDriveApps `sdk/cli`) | Path-based commands | 12k root-folder resolutions and 18k `children` calls in 7 days of logs; p50 API latency 222 ms | Cost grows with path depth. Use events, not tree walks |
| pdcli 0.3.1 / 0.4.0 mount | Community SDK, fuser 0.17 sync trait, whole-file cache plus journal | Write-sync bugs break rsync and git (verified by the user) | See `docs/pdcli-evaluation.md` |

### 2.2 SDK findings that shape the design

All of these are fixed in the fork (`docs/sdk-patches.md`) and are hidden behind the `Backend` trait.

| # | Finding | Evidence | Patch |
|---|---|---|---|
| F1 | File nodes report `storage_quota_consumption` as `claimed_size` and the revision `creation_time` as `claimed_modification_time`. The revision XAttr is never decrypted, so st_size and st_mtime are wrong | `sdk/node/mod.rs:1386-1418` | P4 |
| F2 | Only the first 10 blocks are ever listed, so files over 40 MiB are truncated | `sdk/node/revision.rs:236-243`, `sdk/node/download.rs:18-19` | P1 |
| F3 | Block SHA-256 is not verified; there is no manifest signature check | `sdk/node/download.rs:547-561` | P2 |
| F4 | `download_range` rebuilds revision and block state on every call | `sdk/node/file/download.rs:173-212` | P3 |
| F5 | One whole-file transfer at a time per client (`Semaphore::new(1)`) | `sdk/node/transfer.rs:19` | P7 |
| F6 | Upload returns only `NodeUid`, not the new revision | `sdk/node/file/upload.rs:129-134` | P6 |
| F7 | Errors are `anyhow` strings | `sdk/api/file.rs:163-185` | P8 |
| F8 | No 401 refresh in requests, and a failed refresh silently keeps the old tokens | `rs2/auth.rs:194-197` | P9 |
| F9 | Folder mtime is ignored on create and never decoded | `sdk/node/folder.rs:35`, `:106` | P5 |
| F10 | Revision XAttr carries custom keys (flattened `additional_metadata`), so POSIX mode can be stored remotely | `sdk/client.rs:694`, `:723`; `sdk/api/attr.rs:53-56` | (P4 to read) |
| F11 | New-revision drafts send `CurrentRevisionID` (an optimistic-concurrency base), and rename/move send `OriginalHash` | `sdk/node/draft.rs:175-178`; `sdk/api/links.rs:562`, `:638` | P11 |
| F12 | Events contain only ids and type (`Delete=0, Create=1, UpdateMetadata=2, UpdateContent=3`, `link_id`, `parent_link_id`, `is_trashed`), plus `more` and `refresh` flags | `sdk/api/events.rs:143-214` | — |
| F13 | Login is only the browser session fork; `on_sign_in(url, code)` callback | `rs2/session.rs:240-250` | — |
| F14 | `get_node` and `enumerate_nodes` (one `get_node` per id, 8 at once) answer from the SDK's entity cache, which revision commits and other devices' changes do not update; `get_node_uncached` evicts the entry and fetches it again; `get_nodes_uncached` (P13) does that for many links, 50 per `get_details`. Folder listings (`NodeBatchLoader`, 50 links per `get_details`) are always fresh but do not refresh that cache. Seen on the real API in M2.10 (D035) | fork `9176b0f`: `client.rs:556-563`, `node/operations.rs` (`get_node_metadata`, `enumerate_nodes`), `node/mod.rs` (`NodeBatchLoader`) | — |
| F15 | Rename and move fetch the node's name hash fresh right before sending it as `OriginalHash`, so the server's check never fails; decoded nodes drop the hash | fork `4548aa7`: `node/operations.rs` (`rename`, `move_single`) | P14 |
| F16 | Link details (`v2/volumes/{id}/links`) carry the trash time as `TrashTime`; the SDK reads `Trashed`, so every node looks untrashed | fork `4548aa7`: `api/links.rs` (`LinkDto`) | P15 |

### 2.3 Environment (development machine)

- Gentoo, libfuse/fusermount3 (setuid), `/home` on btrfs, so `FICLONE` reflinks are available.
- `/etc/fuse.conf` has `user_allow_other` commented out. fuser rejects `AutoUnmount` without `allow_other`/`allow_root`
  (`fuser/session.rs:163-171`), so the design must not depend on it (§14.4).
- pjdfstest and fsx are not installed; agents build them from source in `<tools dir>/` (§15.1).

### 2.4 Official SDK constraints (`cs/README.md`)

- Third-party clients must send an honest `x-pm-appversion` (`external-drive-<name>@<version>`). They must use event-based sync rather than
  frequent tree walks, and must back off on errors.
- Login must show that the app is third-party and not officially supported by Proton.
- A cryptography migration is announced for the end of 2026 or early 2027. Old clients will stop working (§17).

---

## 3. Architecture overview

### 3.1 Workspace

```
PdMount/
├── Cargo.toml                 workspace; [patch.crates-io] → SDK fork at a pinned rev
├── crates/
│   ├── pdmount-core/          no fuser dependency; all logic; unit + property tests
│   │   ├── engine.rs          M1: hot/slow operations for the FUSE adapter, prefetch, revalidation
│   │   ├── events.rs          M2: event loop, echo filter, refresh (§9, D034)
│   │   ├── singleflight.rs    M1: shared in-flight work (listings, block maps, blocks)
│   │   ├── vfs/               inode table, dentries, handles, dir locks, attrs, name rules
│   │   ├── access/            access rules, effective-mode resolution (§11)
│   │   ├── store/             SQLite schema, migrations, single writer, group commit
│   │   ├── cache/             block cache (disk, encrypted), RAM LRU, dedup, scheduler, pins
│   │   ├── staging/           per-inode staging files, materialization bitmap, snapshots
│   │   ├── journal/           desired-state reconciler, executor, dependencies, backoff, parking
│   │   ├── sync/              event loop, refresh reconcile, remote apply, echo suppression
│   │   ├── conflict/          detection, policy, naming, records
│   │   ├── notify/            kernel invalidation queue (trait over fuser::Notifier)
│   │   ├── policy/            .pdignore (local-only rules), device identity
│   │   ├── metrics/           histograms, counters, hot-path network guard
│   │   └── errors.rs          CoreError → errno
│   ├── pdmount-backend/
│   │   ├── lib.rs             trait Backend, RemoteNode, BlockInfo, BackendError
│   │   ├── proton.rs          SDK adapter: block-map memo, error classification (401 retry: M3)
│   │   └── fake.rs            in-memory Proton model: clones share a drive, events, faults, latency
│   ├── pdmount-fuse/          fuser::Filesystem impl → core; catch_unwind; reply dispatch
│   └── pdmount/               binary: CLI, daemon, control socket, auth, service unit
├── tests/
│   ├── scenarios/             rsync_*.sh, git_*.sh, access_*.sh, multi_*.sh (VERIFY_MODE)
│   ├── pjdfstest.sh           runner: the subset, expected failures from pjdfstest.list
│   ├── pjdfstest.list
│   └── fsx.sh
├── benches/
└── docs/
```

**Adapted from pdcli (MIT, with attribution in file headers):**
- `pdcli/credentials.rs`: keyring storage with a 0600 file fallback
- `pdcli/pdignore.rs`: matcher only; the default list is emptied
- `pdcli/service.rs` (systemd user unit): not used; an XDG autostart entry replaces the unit (D059)

### 3.2 Backend trait

The core talks only to this trait, implemented by `proton::ProtonBackend` (SDK adapter) and `fake::FakeBackend`.

```rust
#[async_trait]
pub trait Backend: Send + Sync + 'static {
    async fn account(&self) -> Result<AccountInfo>;                              // username, root (cached)
    async fn root(&self) -> Result<RemoteNode>;                                  // My Files root
    async fn list_children(&self, folder: &LinkId) -> Result<Vec<RemoteNode>>;  // paginated inside
    async fn get_nodes(&self, ids: &[LinkId]) -> Result<Vec<Option<RemoteNode>>>;
    async fn blocks(&self, file: &LinkId, rev: &RevisionId) -> Result<Vec<BlockInfo>>; // {index, plain_size?}
    async fn fetch_block(&self, file: &LinkId, rev: &RevisionId, idx: u32) -> Result<Bytes>; // verified plaintext
    async fn fetch_block_into(&self, file: &LinkId, rev: &RevisionId, idx: u32, buf: &mut [u8])
        -> Result<usize>;                    // into a pool slot (§6.2); default: fetch_block and copy
    async fn upload_new(&self, parent: &LinkId, name: &str, src: UploadSource, meta: FileMeta)
        -> Result<(LinkId, RevisionId)>;
    async fn upload_revision(&self, file: &LinkId, base: &RevisionId, src: UploadSource, meta: FileMeta)
        -> Result<RevisionId>;                                                    // Err(RevisionConflict)
    async fn create_folder(&self, parent: &LinkId, name: &str, mtime: SystemTime) -> Result<LinkId>;
    async fn rename(&self, node: &LinkId, new_parent: Option<&LinkId>, new_name: &str, expect: Option<&NameHash>)
        -> Result<()>;
    async fn trash(&self, nodes: &[LinkId]) -> Result<Vec<Result<()>>>;
    async fn latest_event_id(&self) -> Result<EventId>;
    async fn poll_events(&self, since: &EventId) -> Result<EventPage>;          // {next, more, refresh, events}
    async fn storage_info(&self) -> Result<StorageInfo>;                          // used, max
}
```

`RemoteNode` carries the following fields:

| Field | Meaning |
|---|---|
| `link_id`, `parent_link_id`, `name`, `kind` | Identity, location, and type |
| `active_revision` | Current revision id |
| `size` | Plaintext size, from P4 |
| `mtime` | Modification time |
| `mode` | From the `pdmount.mode` XAttr, if present |
| `sha1` | Content digest, if present |
| `name_hash` | Name hash, used as `OriginalHash` |
| `trashed` | Whether the node is in the trash |

`BackendError` is a typed enum: `NotFound`, `AlreadyExists`, `PermissionDenied`, `QuotaExceeded`, `FileTooLarge`, `TooManyChildren`,
`NestingTooDeep`, `RevisionConflict{current}`, `NameConflict`, `InvalidMove`, `RateLimited{retry_after}`, `Transient`, `Unauthorized`, `Integrity`,
`Offline`, `Unsupported`, `Other`.

The SDK's `BlockMap` (content key, URLs, tokens, hashes) stays inside the Proton adapter, memoized per revision for 10 minutes with
one fetch per revision at a time. The core sees only `BlockInfo` and asks for blocks by index.

### 3.3 Component diagram

```
             kernel (VFS, page cache, dcache)
                        │  /dev/fuse
        ┌───────────────▼────────────────┐
        │ pdmount-fuse  (n_threads = 4)  │  catch_unwind around every callback
        └───────┬───────────────┬────────┘
       hot path │ (RAM only)    │ slow path: Reply moved into a tokio task, with deadline
        ┌───────▼───────────────▼──────────────────────────────────────────┐
        │ pdmount-core                                                      │
        │  vfs ── access ── staging ── cache(RAM LRU │ disk blocks)         │
        │   │                  │            ▲                               │
        │   ▼                  ▼            │ fetch / seed                  │
        │  store (SQLite WAL, single writer, group commit)                  │
        │   ▲        ▲                ▲                                     │
        │   │   journal executor ── conflict ──┐                            │
        │   │        │                         │                            │
        │   └── sync engine (events) ──► notify queue ──► fuser::Notifier   │
        └────────────┬─────────────────────────┬────────────────────────────┘
                     │ Backend trait           │
        ┌────────────▼──────────┐   ┌──────────▼─────────┐
        │ proton adapter (SDK)  │   │ fake (tests)       │
        └────────────┬──────────┘   └────────────────────┘
                     ▼
              Proton Drive API
```

### 3.4 Data flows

- **Read (warm):** kernel `read`, then vfs, then cache RAM hit, then reply from a slice of the block's pool slot (§6.2). No disk
  access, no network.
- **Read (cold):** kernel `read`, then cache miss, then a slow-path task: `block_map` (memoized), then `fetch_block_into` a pool
  slot (§6.2), then verify, then seal it in place for the disk cache and open it again, then insert into the RAM LRU, then
  reply from the slot. Readahead schedules the next blocks.
- **Write:** kernel `write` → access check → `pwrite` to staging, bump `content_gen`, reply. Then `release`
  → debounce → journal executor → snapshot → upload → commit to store (clean only if the generation is unchanged).
- **Remote change:** sync loop → `poll_events` → resolve nodes → one store transaction (apply and advance the cursor)
  → notify queue → `inval_entry`/`inval_inode`/`delete`.

---

## 4. Concurrency model

### 4.1 fuser 0.18 facts

| Fact | Evidence | Consequence |
|---|---|---|
| `Filesystem` is a sync trait with `&self`, `Send + Sync + 'static` | `fuser/lib.rs:400` | Shared state needs interior mutability |
| `experimental::AsyncFilesystem` covers only lookup, getattr, read, readdir; `TokioAdapter` builds a private runtime | `fuser/experimental.rs:220-257`, `:126-140` | Not usable for a read-write FS. **Use the sync trait** |
| Reply objects are `Send + 'static` | `fuser/reply.rs:104` | Slow operations move the Reply into a tokio task |
| A Reply dropped unsent answers EIO | `fuser/reply.rs:150-160` | A failed task cannot hang a request |
| A panicking loop thread fails `run()` ("event loop thread panicked") | `fuser/session.rs:310-316` | Every callback body runs in `catch_unwind(AssertUnwindSafe(..))`; build profile `panic = "unwind"` |
| `FUSE_INTERRUPT` returns ENOSYS | `fuser/request.rs:117-120` | No cancellation. **Every slow path has a deadline** |
| `Config { n_threads, clone_fd }` (non-exhaustive) | `fuser/mnt/mount_options.rs:16-20` | `n_threads = 4`, `clone_fd = true` |
| `Notifier`: `inval_entry`, `inval_inode`, `store`, `delete` | `fuser/notify.rs:72-104` | Remote changes reach the kernel cache explicitly |

### 4.2 Hot path and slow path

| Path | Operations | Runs on | Budget |
|---|---|---|---|
| Hot | lookup, getattr, readdir/readdirplus of listed dirs, forget, opendir/releasedir, open, create, mkdir, unlink, rmdir, rename, setattr, write into materialized ranges, read hits, flush, release, statfs, fsync (`local`) | fuser thread, RAM plus local disk only | §5.7 SLOs |
| Slow | first listing of an unlisted directory, read miss, materializing a partially overwritten remote block, `fsync` in `remote` mode, `rmdir`/rename-over of an unlisted directory | tokio task holding the Reply; bounded by a semaphore (64) | metadata 20 s, block 60 s; when not online, `timeouts.offline_read` (2 s) |

When a deadline expires, the request replies EIO and logs it. The background fetch continues so that the cache warms.

### 4.3 State and locking

- **Inode table** (RAM, authoritative for FUSE): `HashMap<Ino, Node>` plus per-directory `BTreeMap<Name, Ino>` child maps.
  Each node holds its local state, its last known remote state and its generations (§8.2). The whole table is under one
  `RwLock`, held for microseconds and never across `.await`; it is loaded completely from the store at mount. Sharding
  is a later option if profiling shows contention.
- **Per-inode staging lock**: the staging file and its sidecar (§7.1) of a changed file have their own mutex, so file I/O
  never holds the table lock. A write or truncation changes the staging and the node (size, `content_gen`) under the
  staging lock, and an upload commit compares the generations and takes a clean staging out of the map under it
  (§7.4 7–8); otherwise a write between the two is lost (fsx, M2.9). A staging file is closed only after the map of
  staged inodes is unlocked: on btrfs, closing a file that was truncated to 0 bytes flushes it and waits for a commit
  in progress.
- **Lock order**, outermost first: one inode's staging lock, the map of staged inodes, the access rules (§11), the
  table, then leaf locks that take nothing while held (journal entries, the conflict registry, `.pdignore` rules). A
  lock is taken only while holding locks above it, so the table is never read recursively (with a writer waiting, the
  second read blocks for good). Debug builds check the order on every acquisition and panic on a violation
  (`lock_order.rs`), so the tests and property tests enforce it. No lock is held across `.await` or while calling the
  Notifier. The full order, with the asynchronous locks, is in the `engine.rs` module doc.
- **SQLite** (WAL): one writer thread owns the write connection. Mutations are sent as batches, with group commit: everything
  queued when the writer wakes goes into one transaction. The writer does not sync: it commits with
  `synchronous=NORMAL` and automatic checkpoints are off. Callers that need durability (fsync, fsyncdir, `access set`,
  inode reservations, staging removal) are acknowledged by a sync thread once it has fsync'd the WAL after their
  commit, which is what `synchronous=FULL` does. A WAL fsync and a staging fsync never overlap, since on btrfs that
  stalls commits for milliseconds; staging fsyncs may overlap each other (D087). The same thread runs a `PASSIVE` checkpoint once no commit has come for
  1 s, or after 20 000 writes under steady load: on btrfs a checkpoint slows every file creation in the file system for
  as long as it runs (600 ms measured after a burst). A passive checkpoint never catches up with a writer that keeps
  committing, so under steady load the WAL never starts over (14 GB in a few hours of fsx, M2.9). When 64 MiB or more
  is left in it after a passive checkpoint, the writer runs a `TRUNCATE` checkpoint before its next batch, which
  copies the rest and empties the file. It runs on the writer's connection because on another one it made writes in
  the writer's transactions fail with `SQLITE_BUSY` (lost rows); between two batches a failed checkpoint loses
  nothing. Readers use a separate read connection.
- **Replies wait for the commit** (D032): a successful reply to a request that changes state (create, mkdir, unlink,
  rmdir, rename, setattr, open, write, flush, release) is sent only once the batch holding the rows it queued has
  committed (`synchronous=NORMAL`, no fsync). The fuser thread does not wait: the Reply goes into a waiter list, which
  the writer drains after each commit, and is sent from there; waiting adds no write of its own. A request that queued nothing, or whose rows are already committed, is answered at once; on the
  hot path only the rows queued by the handler's own thread count. Errors are answered at once.
- **Kernel notifications** go through a single async notify task (§9.5). They are never sent from a FUSE handler thread, because invalidating
  an inode with a request in flight on the same channel can deadlock.

### 4.4 Runtime layout

```
main
 ├─ tokio multi-thread runtime (workers = num_cpus, max 8)
 │   ├─ sync engine task
 │   ├─ journal executor (upload workers = 4, namespace worker = 1 ordered)
 │   ├─ cache scheduler (block downloads: 5 in total; interactive 5, readahead 2, prefetch 1; D033)
 │   ├─ notify task
 │   ├─ auth task (proactive refresh, needs_login)
 │   └─ control socket server
 ├─ store writer thread and store sync thread (§4.3)
 └─ fuser BackgroundSession (4 threads, clone_fd)
```

`Session::new` performs the handshake and calls `init` (`fuser/session.rs:186`). The notifier is obtained via
`Session::notifier()` (`fuser/session.rs:493`) before `spawn()`.

Before mounting, the daemon raises its soft open-file limit to the hard limit and grows its descriptor table once, to
min(hard limit, 65 536). Each staged file holds a descriptor, and each later growth of the table makes every thread that
opens a file wait for an RCU grace period: 30–80 ms stalls of all FUSE threads at once, measured with 4 writers.

---

## 5. Low-latency design (RQ2)

### 5.1 Local-first metadata

- After a directory has been listed once, every lookup (including negative), getattr, readdir, readdirplus, and statfs is
  answered from RAM. The SQLite store persists the tree, so a remount serves the last known state immediately and catches up in
  the background.
- **Catching up after a mount (D034):** the event task resumes the stream from the stored cursor (§9.1), so remote changes
  made while unmounted arrive as events and reach the kernel through the invalidation queue (§9.5). The listed tree is
  walked (every listed directory listed again, one at a time, paced as in §5.3 by D027, root first) only when the server
  asks for a `refresh` or the profile has no cursor (§9.4). This replaced startup revalidation on every mount (D025).
- **Network guard:** handlers marked hot run inside a `metrics::hot` scope. Any `Backend` call within that scope increments
  `hot_path_network_calls` and, in debug builds, panics. Tests assert the counter is 0.

### 5.2 Write-back

- A write is acknowledged after `pwrite` to the staging file and the in-RAM generation bump (and, for the first write
  that makes a file dirty, the commit of its row). Durability against power loss comes from fsync; uploads are
  asynchronous (§7).
- Namespace operations (create, mkdir, rename, unlink, rmdir, setattr) are acknowledged after the in-RAM update and the
  group commit of their rows, without fsync (§4.3, D032).

### 5.3 Prefetch and pinning

| Mechanism | Behaviour | Budget |
|---|---|---|
| Listing prefetch | On a user opendir, once per directory per mount, background-list subdirectories to depth 2 | 32 listings per trigger, prefetch priority |
| Small-file prefetch | Files ≤ 256 KiB in that directory are fetched into the block cache | 16 MiB per directory |
| Sequential readahead | A per-handle detector prefetches the next 2–4 blocks (8–16 MiB) | readahead permits |
| Pins (`pdmount pin <path>`, D060) | Every file at or below the path is downloaded and kept, and new revisions are fetched as the tree learns of them. One background task looks at the tree when woken (pin, unpin, a listing or event that changed nodes, a rename, a removal, an upload), at the start and every 5 minutes: it pins the current revision of each clean remote file, lets go of the others, lists unlisted pinned folders depth first, then fetches what is missing at prefetch priority, one background turn per file or 16 blocks. Failures back off like the journal, without parking; it waits while offline or needing a login. A pin follows its inode; a file replacing a pinned one takes the pin over (atomic save); deleting the node deletes the pin | Not counted against `cache.max_bytes`, reported apart. `pin` is refused when what is known of the path does not fit in the free disk space less 1 GiB; the fetcher stops below 1 GiB free, with a WARN |

Background API work (listing prefetch, small-file prefetch, the event loop and refresh listings, §9) is paced (D027): one request at a time,
at most two starts per second, and none while a foreground listing or read is in flight. Proton rate-limits bursts with
HTTP 429, and the SDK's retry back-off exceeds the 20 s metadata deadline, so unpaced prefetch turns into EIO for users.

### 5.4 Kernel caching

| Setting | Value | Reason |
|---|---|---|
| entry / attr TTL | 60 s (`fuser/reply.rs:257` `entry_with_ttls`) | Remote changes are pushed by invalidation, so a long TTL is safe |
| negative-entry TTL | 10 s (reply `ino = 0`) | git and rsync probe many missing paths |
| `FUSE_DO_READDIRPLUS` + `FUSE_READDIRPLUS_AUTO` | requested (`fuser/ll/flags/init_flags.rs:36-38`) | `ls -l` and `git status` avoid per-entry lookups |
| `FUSE_PARALLEL_DIROPS` | requested (`:46`) | Locking is per directory |
| `FUSE_ATOMIC_O_TRUNC` | requested (`:16`) | `O_TRUNC` opens never fetch the old content |
| `max_write` | 1 MiB (`fuser/lib.rs:299`) | Fewer write requests |
| `max_readahead` | kernel maximum (`fuser/lib.rs:316`) | Fast sequential reads |
| `max_background` | 64 (`fuser/lib.rs:356`) | Matches the slow-path semaphore |
| `FOPEN_KEEP_CACHE` | when the inode's content epoch equals the one last served to the kernel; the epoch moves when remote content replaces the cached content (`fuser/ll/flags/fopen_flags.rs:12`) | Page cache survives reopen |
| `FOPEN_CACHE_DIR` | on listed directories (`:16`) | Directory pages are cached |
| `FOPEN_KEEP_CACHE` on opendir | when the directory's entries hash to what its last open served and no other handle of it is open (D072) | `git status` lists unchanged directories from the kernel; local changes and §9.5 invalidations drop the pages, the hash catches the rest at the next open |
| `FOPEN_DIRECT_IO` | never | git mmaps packs and the index |
| `FUSE_WRITEBACK_CACHE` | **never** requested (`:42`) | See §5.4.1 (D064, D083) |
| `FUSE_HANDLE_KILLPRIV_V2` | not requested (`:66`) | It would spare about 0.8 `getxattr(security.capability)` per small file written, but untar was not measurably faster, and the daemon would have to clear setuid/setgid itself (D068) |

Capabilities are added with `KernelConfig::add_capabilities` (`fuser/lib.rs:342`). An unsupported flag is logged and
the mount continues.

#### 5.4.1 Why the writeback cache is not used (D064, D083)

M4.8 measured `FUSE_WRITEBACK_CACHE` on Linux 6.18 behind an option (D064), and the option was then removed (D083). With it:
- **The kernel owns size, mtime and ctime of every regular file it caches** (`fuse_get_cache_mask`): it ignores them in
  attribute replies, also after `inval_inode`, as long as the inode lives (a dentry or an open file holds it). A remote
  edit that changes a file's size is not seen: reads stop at the old size or return zeros after it. Measured: a file
  grown remotely from 5 to 21 bytes still read 5 bytes after 60 s, closed or open.
- **Writes reach the daemon late**: `write()` returns once the page cache has the data; the daemon gets it at close,
  fsync, or the kernel's writeback. Until then the daemon treats the file as clean, so a remote revision can be
  adopted and the local data written over it later, without the conflict check of §10.5 (from reading the kernel; no
  test demonstrates it). A write on a handle opened before a switch to `ro` (§11.4) succeeds and fails at fsync and
  close instead, and until a remount the file reads as zeros past the daemon's end (scenario A6 fails).
- **The kernel sends its own mtime and ctime** (`setattr` with `FATTR_MTIME` without `FATTR_MTIME_NOW`) at close and
  when it writes back dirty inodes. `rsync -t` and `touch -d` still end with their mtime: their `utimensat` comes after
  the close.
- **Reads arrive on write-only handles** (partial pages read before a write). Linux 6.18 passes `O_WRONLY` through
  unchanged; the daemon checks access at open, not per read, so nothing depends on it. Remote invalidation does not
  drop dirty pages: `invalidate_inode_pages2` writes them to the daemon first (`launder_folio`).
- **Requests per file**: small writes are merged (untar 11 433 writes → 5000), but each written file gets one more
  `setattr` (5101 → 10 101), which waits for the store commit like the write it replaces. untar took 9 % longer
  (median 0.848 against 0.777 s); `git status` and read throughput do not change.

It helped no benchmark and breaks coherence with other writers, so PdMount does not request it, on any mount.

### 5.5 Coherence without polling the kernel

Each remote change that is applied to the store produces invalidations (§9.5): `inval_entry` for names, `inval_inode` for data
and attributes, and `delete` for removals (which also notifies inotify watchers). Local changes go through the kernel and need no
invalidation, except access-rule changes (§11.4).

### 5.6 fsync semantics

| Mode | Meaning | Use |
|---|---|---|
| `local` (default) | `fdatasync(staging)` plus a durable store commit of the inode's generation and journal entry, one after the other (D067), then reply. Data survives a crash or power loss and is guaranteed to upload later | git fsyncs refs and packs on every commit. A remote round trip per fsync would make each commit cost several network RTTs |
| `remote` (global, or per glob via `fsync.remote_paths`, matched against the path from the mount root at fsync time, such as `/important/**`; `*` stops at a `/`) | Wait until the generation captured at fsync time is uploaded and committed, at most `timeouts.block` (`timeouts.offline_read` when offline): EIO at expiry, and the upload goes on. A parked entry gives EIO; quota and size errors come as ENOSPC/EFBIG at the inode's next flush or fsync | Workflows that need "on the server when fsync returns" |

- `fsyncdir` commits the store.
- A crash of the daemon (`kill -9`, panic, OOM kill) loses nothing a request was answered for: replies wait for the store
  commit (§4.3), and writes to staging files are in the page cache. Only a power loss or kernel crash is limited to what
  was fsync'd.
- `flush` (close) never blocks on uploads. It returns any error deferred from a previous upload of that inode (§7.8).
- `pdmount sync [--timeout <s>] [<path>]` is the explicit remote barrier. It forces every entry under the path (or all), and
  what they wait for, and returns when none is left, also those that came up meanwhile. It prints the parked ones and exits 1 if any
  holds up the path, at once, or if the timeout (default 10 min) passes.

### 5.7 Latency budgets (SLOs)

These are measured in the daemon from request receipt to reply, on SSD, warm process. Kernel cache hits never reach the daemon.

| Operation | p99 target |
|---|---|
| getattr, lookup, readdir(plus) of a listed dir | < 1 ms |
| create, mkdir, unlink, rmdir, rename, setattr | < 2 ms |
| write into a materialized range | < 1 ms per request |
| read hit, RAM / disk (including at-rest decrypt) | < 1 ms / < 5 ms |
| fsync (`local`) | < 40 ms (D086; D066: two flushes, file data and store log; one bare `fdatasync` has p99 8.9 ms on the SATA SSD under the benchmark load) |
| first byte, small file, prefetched | 0 network RTT |
| first byte, small file, not prefetched | 1 API RTT (block map) + 1 storage RTT |
| first listing of a directory | ⌈children / page size⌉ API RTTs, once |

### 5.8 Instrumentation

- **Latency (M4.1):** one HDR histogram per key (`metrics.rs` `Op`), in µs, 1 µs–120 s, 3 significant digits; longer
  values count as 120 s.
  - **FUSE operations** (`lookup`, `getattr`, `setattr`, `readdir`, `readdirplus`, `opendir`, `releasedir`, `open`,
    `create`, `mkdir`, `unlink`, `rmdir`, `rename`, `write`, `flush`, `fsync`, `fsyncdir`, `release`, `statfs`,
    `getxattr`, `listxattr`, `access`, `forget`): from callback entry until right after the reply call. A
    `Stopwatch` travels with the Reply, so a reply deferred until the store commit (§5.6) or sent from the slow path
    counts that wait. Operations answered at once with a fixed error (`mknod`, `symlink`, `link`, `setxattr`,
    `removexattr`, `fallocate`) are not timed.
  - **Reads**, by where the bytes came from: `read_ram` (RAM cache, or past the end), `read_disk` (at least one block
    from the disk cache), `read_staging` (a file with local changes), `read_slow` (waited on the slow path).
  - **Backend calls** (`backend_<method>`, one per `Backend` method): the call into the backend as the engine makes
    it, inside the connectivity monitor (calls it refuses while signed out are not timed).
- **Counters:** cache RAM/disk hits, misses, evictions and corrupt entries, pool fallbacks, prefetched blocks, listings,
  backend calls, `rate_limited` (backend calls answered HTTP 429), uploads, expired deadlines, handler panics and
  `hot_path_network_calls`.
- **Gauges,** computed when read: `queue_pending` and `queue_parked` (journal entries), `queue_oldest_secs` (how long the
  oldest entry that is not parked has been dirty), `staged_open_files` (staging descriptors open in the LRU, §7.1).
- `pdmount stats [--json] [--reset]` prints counters, pool slots, gauges and a latency table (`count`, p50, p99, max;
  operations not seen are left out) over the control socket. `--json` prints the reply (adds p90); `--reset` clears the
  histograms after reading them (counters stay).
- **Prometheus textfile** (`[stats] prometheus_textfile`, off by default): written every 15 s, atomically (a temporary
  `.<name>.tmp` renamed into place). Counters as `pdmount_<name>_total`, gauges as `pdmount_<name>`, latencies as the
  summary `pdmount_latency_seconds{op,quantile="0.5|0.9|0.99"}` plus `_count`. Every series has a `profile` label, so
  several profiles can share a collector directory. `stats --reset` resets its summaries too.
- Slow-path requests run in a `debug_span!("fuse", op, unique, ino)`; the hot path has no span and logs nothing per
  request. A request slower than 50 ms that stayed on the hot path is logged at WARN (`op`, `ino`, `elapsed_ms`),
  except `flush`, `release` and `fsync`, which wait on commits or the disk. At most one line per operation per 10 s;
  the next line carries the number held back (`suppressed`).
- A slow-path request answered EIO is logged at WARN with its operation and cause, since EIO tells the caller nothing.
  Other failures are logged at DEBUG, except the expected ENOENT, EEXIST and ENOTEMPTY, which are not logged.

---

## 6. Read path

### 6.1 Block map

- Proton stores content in plaintext blocks of 4 MiB (`sdk/node/revision.rs:34`), encrypted per block. Offsets come from
  `Common.BlockSizes` in the revision XAttr (`sdk/node/file/download.rs:165-170`). If that field is absent, offsets assume a uniform 4 MiB size and are
  validated against the decrypted lengths.
- Sizes are the uploader's claim, so they are bounded before they size anything (S-5, S-6):
  - A file's size is the sum of its claimed block sizes when the revision has them (saturating); a claimed `Size` that
    differs is logged at DEBUG and ignored. Without block sizes, the claimed `Size` is used, and staging bounds it (§7.1).
  - A layout (`cache.rs` `Layout::new`) refuses, as `Integrity` (EIO), a declared block above 4 MiB, an empty block
    before the last, and declared sizes that do not add up to the file size. A block fetch refuses an expected length
    above 4 MiB before it takes a buffer.
- `BlockMap` = content key plus a list of `{index, url, token, sha256, plain_size}` (P1 pagination, P3 public type). It is kept in RAM
  with a 10 min TTL, because URLs and tokens expire. It is refreshed when a block download reports an expired target (`sdk/error.rs:38-40`).
- Sizes per revision are persisted in `revision_blocks`; hashes and URLs are not.

### 6.2 Cache layers

| Layer | Key | Content | Limit | Eviction |
|---|---|---|---|---|
| RAM LRU | `(link, rev, idx)` | decrypted plaintext blocks, in block pool slots | 32 MiB (`cache.ram_bytes`, at least 16 MiB), reserved at startup | LRU per pool class, when a slot is needed |
| Disk | `(link, rev, idx)` → `cache/blocks/xx/<blake3(key)>` | nonce ‖ XChaCha20-Poly1305 (key = `blake3::derive_key("pdmount block cache v1", cache master key)`, AAD = block key) | `cache.max_bytes` (10 GiB), 90 %/80 % watermarks on the unpinned bytes | LRU by `last_access`, kept ordered in memory; RAM hits count too (stored in batches every 60 s) |

Revisions are immutable, so cache entries never need invalidation, only eviction.

The RAM LRU is sized to the working set of blocks being read, not to a long-term cache: a block arrives as 4–32 FUSE reads of
at most 1 MiB, and without it each read would decrypt the whole block again (AEAD per block). Long-term caching of file
content is the job of the kernel page cache (layer 1, §5.4, `FOPEN_KEEP_CACHE`) (D049).

**Block pool (D049, `crates/pdmount-core/src/pool.rs`).** All of `cache.ram_bytes` is reserved when the cache opens, so after
startup the cache makes no block-sized allocation:
- One anonymous mapping per size class (`MAP_PRIVATE | MAP_ANONYMOUS | MAP_POPULATE`, never `MADV_FREE`): it is resident
  from the start and stays so. The classes hold blocks of up to 64 KiB, 1 MiB and 4 MiB; each slot is 40 bytes larger, room
  for the nonce and tag of a sealed block, so a slot is laid out like a disk-cache file with the plaintext at offset 24.
- The split is fixed: about 5/8 to the 4 MiB class, rounded to whole slots, 1/4 to the 1 MiB class and the rest to 64 KiB.
  The 32 MiB default gives 5, 7 and 79 slots: one 4 MiB slot per download in flight (§6.4). The 4 MiB class needs at least
  2 slots (a read across a block boundary holds two), so `cache.ram_bytes` must be at least 16 MiB.
- A block uses the smallest class it fits in. A free list per class hands out slots; a slot frozen into `Bytes`
  (`Bytes::from_owner`) goes back when the last clone is dropped. The RAM LRU holds those `Bytes`, so a reply holding a
  clone keeps its slot alive after eviction; the LRU's size counts slot sizes, so it never exceeds `ram_bytes`.
- Taking a slot when none is free evicts LRU entries of that class until one is. If every slot of the class is still
  referenced, the hot path (`get_local`) never waits: it reads into a heap buffer, counted in `cache_pool_fallbacks`, and
  does not insert the block into the LRU. The slow path (downloads, `seed`) waits up to 5 s for a slot of its class to be
  freed or become evictable, then falls back the same way.
- A read holds at most 2 blocks at once (`READ_BLOCKS_HELD`): it copies each block out as it arrives and drops it, so a
  read over many small blocks cannot pin a whole class and make its own downloads wait.
- Disk-cache hits are read (`pread`) into a slot and decrypted in place; blocks for the disk cache, from `seed` and from
  downloads, are sealed in place in a slot (`seed` blocks do not go into the LRU).
- Downloads go straight into a slot: `Backend::fetch_block_into`. The fake reads its blob into it; the Proton adapter
  overrides it: `download_verified` writes through a cursor over the slot, after a check of the claimed size. The slot is sealed, written to the disk cache and opened again in place, then goes into the LRU.
- **Replies:** a read inside one block (the common case) is answered with that block's `Bytes`, sliced to the range;
  `ReplyData::data` copies it into the kernel. Only a read across blocks assembles, into a scratch slot of 1 MiB (the
  largest FUSE read): 4 of them, one per fuser thread, reserved with the pool and not part of `cache.ram_bytes`. The hot
  path takes the heap if none is free (`cache_pool_fallbacks`); the slow path waits up to 5 s for one, after the first
  block is there. Larger reads (materialization across blocks) and reads of dirty inodes from staging (§6.5) use the heap
  as before (`ReadData::Owned`).
- `pdmount stats` shows the slots in use per class (`cache_pool_slots_<class>`, `cache_pool_slots_scratch`).
- Outside the cache, streaming loops over files (hashing a staged file before upload) use a 64 KiB buffer on the stack,
  not a heap buffer per call: once glibc has raised its mmap threshold, freed heap buffers of 1 MiB and more stay in its
  arenas, and per-upload churn of them made RSS grow in the soak (M3.19).

**Never evicted:**
- Blocks with `pin = 1`, the current revisions of pinned files (§5.3, D060), which are also not counted against the cap (their size
  is reported apart). A revision's later blocks and `seed`s get the same value, and a revision let go is evictable by its last use.
- Blocks of a revision a staged file still reads, that is, the `base` of a staging that is not fully materialized. Each staging
  holds this protection for as long as it has a base, however it ends, so it comes back at mount when the staging is reopened. It
  follows `base`, not `base_rev`: an upload's new precondition does not change where the unmaterialized bytes come from.

Open handles have no protection of their own: the kernel page cache and the RAM LRU serve them, and a block evicted under one is
fetched again.

The watermarks are also checked when the cache opens, so a lowered `cache.max_bytes` applies at mount, not at the next insert.

**Orphan sweep.** Once per mount, a background task without API calls removes files in `cache/blocks` that have no row, and rows
whose file is missing (both left by a crash). It holds the index mutex only to drop the rows of one directory. Files younger than the
sweep's start (less 2 s) are left alone: `put` renames a file into place before its row is queued.

### 6.3 Integrity

Each fetched block is checked against `BlockDto.hash_digest` (P2), and the manifest signature over the listed thumbnail and block
hashes is verified once per block map, when it is built (P17); a listed block without a hash fails it too. On mismatch: EIO, the
block (or, for the manifest, every block of that revision) is not cached, and the event is logged at ERROR. Disk-cache entries that fail AEAD authentication are deleted and re-fetched
transparently. Local corruption is treated as a cache miss, never as a fatal error.

### 6.4 Scheduling

- **In-flight dedup:** a `HashMap<BlockKey, Shared<BoxFuture<…>>>` running a spawned task means readers, readahead, prefetch, and materialization share one download.
- **Three priority classes** with separate permits: interactive (reads and foreground materialization), readahead (also materialization and base reads for uploads), prefetch.
- **Transfer limits (D033), kept slightly below the official SDKs:**
  - Downloads: at most 5 blocks in flight over all classes. The C# SDK allows 6 block transfers per direction, shared by
    all files (`cs/client/cs/src/Proton.Drive.Sdk/ProtonDriveClient.cs:28`, `Nodes/TransferQueue.cs:39-41`); the JS
    SDK 5 files (`download/queue.ts:17`) with up to 10 blocks each (`download/fileDownloader.ts:22`). Readahead (2) and
    prefetch (1) together leave interactive reads at least 2.
  - Uploads: `upload.concurrency` (4, at most 4) files at once, with at most 5 blocks in flight over all of them (D058):
    the drive client is built with P7's `with_max_concurrent_file_transfers(upload.concurrency)` and P18's
    `with_block_transfer_parallelism(5)`; the SDK's own defaults are 1 file and 6 blocks. The official limits are 5
    files with 5 blocks each and 40 MiB in total (JS, `upload/queue.ts:11,20`, `upload/streamUploader.ts:69`) and 6
    files with 6 blocks in total (C#). The client's download queue gets the same limits: the adapter's block
    downloads (`download_verified`) take a block permit from it, 5 now as `DOWNLOADS` above (before, 6), and take no
    file permit.
- **Seeding:** after a successful upload, the snapshot is split into blocks and inserted under the new revision. When git reads a pack it
  just wrote, nothing is downloaded.

### 6.5 Reads of dirty inodes

Materialized ranges come from staging. Unmaterialized ranges come from the base revision through the cache. Reads past the staging size
return EOF. Base ranges are fetched without the staging lock; a write meanwhile can move them, so the read plans again and
fetches only what is still missing (up to 8 times) instead of failing.

---

## 7. Write path

### 7.1 Staging

- One sparse file per inode: `~/.local/share/pdmount/<profile>/staging/<ino>`, directory mode 0700. Its length is always
  the file's size.
- A remote file above `MAX_STAGED_SIZE` (2^20 blocks of 4 MiB, 4 TiB) is never staged: a write or truncation of it fails
  with EFBIG, and nothing is created. Its size is only the uploader's claim (§6.1), which must not size a sparse file or
  the materialization. Before a snapshot, the missing base blocks are materialized in batches of 64, so no list of all
  of them is ever built.
- A sidecar `staging/<ino>.state` beside it (JSON, replaced atomically) holds:

  | Field | Meaning |
  |---|---|
  | `base_rev` | Remote revision the edits derive from (the upload precondition); NULL for new files |
  | `base` | The revision whose content is still partly used, and how many of its bytes (truncation cuts it) |
  | `materialized` | The 4 MiB blocks of the base already copied into staging |

  The sidecar is replaced before a write that depends on it is acknowledged, so data and bookkeeping agree after a
  crash of the daemon. The data goes first (a write, a truncate's cut), then the sidecar: a crash between the two leaves
  a block on its base instead of zeros, and a reopen whose file is shorter than the base finishes the cut. A base block
  read short is never marked materialized. Both reach the disk with `fsync` (§5.6), the sidecar first when it changed
  since its last flush, so durable data never comes with a lost sidecar that held its base. The `staging` table of
  §13.2 is not used.
- **A file without a base has no sidecar** (D067): a new file gets none until it has a `base_rev` (after its upload).
  A file with a base gets its sidecar before its data file, so a data file without a sidecar never had a base; staging
  without a base removes a sidecar left from earlier staging of the inode. Once written, a sidecar stays until the
  staging is removed, even when it is back to the default. Startup lists the data files; a missing sidecar reads as no
  base, and a sidecar without a data file (a crash in `create` or removal, which deletes the data file first) is
  removed.
- The generations live in the inode row (§13.2):

  | Field | Meaning |
  |---|---|
  | `content_gen`, `meta_gen` | Generation counters (u64) |
  | `uploaded_content_gen`, `uploaded_meta_gen` | Generations last confirmed uploaded |

- Staging files are opened on demand (D063): a descriptor LRU per profile keeps at most 1024 of them open, or a quarter of
  the soft open-file limit when that is lower and at least 16 (the daemon first raises the soft limit to the hard one, at most 65 536, §14). Each staging
  operation takes the inode's descriptor from it, opening the file on a miss; an evicted descriptor is closed when the
  last operation using it ends, never under the LRU's lock (a close can make btrfs flush the file). `fsync` works on
  whichever descriptor is current, since it flushes the inode. Removing a staging file closes its descriptor first. So
  the number of pending files is not bounded by the open-file limit. A backend's local EMFILE/ENFILE is a transient
  error (retried), not a permanent one that parks the entry.
- A staging file is deleted (clean after upload, unlinked and closed, trashed, removed remotely) only after the store rows
  that stop naming it are committed durably, and not if a write has started a new staging of the inode by then. Otherwise
  a daemon crash in between leaves a stored row whose content is gone, and the file comes back empty. Removals are
  collected for 1 s and share one store fsync, because each fsync stalls the commits that replies wait for (§4.3).
- Staging is plaintext (it is the working copy). The recommended mitigation is full-disk encryption (§17).

### 7.2 Open, write, truncate

- `open(O_TRUNC)`: with `FUSE_ATOMIC_O_TRUNC`, staging starts empty, nothing is materialized from the base, and the size is 0.
- `write` covering whole blocks, or beyond the base size, goes straight to staging.
- `write` partially covering an unmaterialized base block first materializes that block (slow path: cache or network).
  This is what makes `rsync --inplace` and in-place edits of large files correct.
- `setattr(size)` materializes the block containing the cut point and drops the blocks after it.
- Each change bumps `content_gen` and replies immediately.

### 7.3 Upload trigger and debounce

- An upload job becomes eligible when there is no writable handle and no shared writable mmap on the inode, and the inode has been
  quiet for `upload.debounce` (2 s).
- Inodes that stay dirty and open continuously are flushed at least every `upload.max_dirty_age` (10 min), from a snapshot.
- `fsync` in `remote` mode and `pdmount sync` make the job eligible immediately.
- **The debounce matters for correctness.** git lock files (`*.lock`), git temp objects, and rsync temps (`.name.XXXXXX`) are
  renamed or unlinked within milliseconds and never reach the server under their temporary names (verified in scenario R7).

### 7.4 Upload job

The job executes against the current local state, not a payload stored at enqueue time.

1. Materialize the remaining unmaterialized blocks from `base_rev`.
2. Snapshot with `ioctl(FICLONE)` into `staging/<ino>.snap`. Fallback: copy, then re-check `content_gen`. Record `snap_gen`.
3. Compute SHA-1 (`set_expected_sha1`, `sdk/node/file/upload.rs:117`) and the exact size. The SDK requires the size up front and
   rejects a mismatch at commit with `ContentSizeMismatchIntegrityException` (`sdk/node/revision.rs:667`).
4. If the SHA-1 equals the remote active revision's SHA-1, adopt the remote revision and skip the upload (§10.3).
5. Conflict pre-check (§10.4, layer 2), for an existing node and **before** steps 1–3, so that the common case costs one
   `get_nodes([link])` and no materialization: `base_rev` is the staging's `base_rev`, else the node's remote revision.
   - The remote file is gone or trashed → `resolve_vanished` (§10.5), then plan again.
   - `active_rev == base_rev` → go on (the node's name hash is refreshed for renames).
   - Otherwise the remote file has a revision the upload is not based on. If the node has only metadata changes, the other
     client's content is adopted (`readopt_remote_content`) and the metadata revision is planned again on top of it: not a
     conflict (M7). If the sizes differ, edit/edit (§10.5). Otherwise steps 1–3 run and the content decides (§10.3):
     identical content only moves the base (`adopt_base`), different content is edit/edit.
   Every such outcome ends the job with "plan again" (`UploadError::Replan`), not with an error.
6. Upload:
   - Existing node: `upload_revision(link, base_rev, snap, meta)` (`get_file_revision_uploader`, `sdk/client.rs:718`).
     A `RevisionConflict` (layer 3, §10.4) fetches the node again: still at `base_rev` means another client's draft is open
     (retry later); otherwise the decision of step 5 is made with the snapshot at hand. `NotFound` → `resolve_vanished`.
   - New node: `upload_new(parent_link, name, snap, meta)` (`get_file_uploader`, `sdk/client.rs:687`).
   - In both cases, `meta` = `{mtime, additional_metadata: [("pdmount.mode", "0644")]}` (F10).
7. In one store transaction:
   - Set `base_rev = remote_rev = new_rev`.
   - Insert the new revision into `own_revisions`.
   - Set `uploaded_*_gen = snap_*gen`.
   - **Mark clean only if `content_gen == snap_gen` and `meta_gen == snap_meta_gen`.** Otherwise the inode stays dirty and the job is
     re-queued on the new base.
   - Clear `remote_newer`.
   - **Commit guard:** if the node is no longer bound to the uploaded link (a resurrection unbound it while the revision
     went up), nothing is recorded, the snapshot goes and the job plans again; the node's content is uploaded as a new file.
8. Seed the block cache from the snapshot (§6.4) and delete the snapshot. A clean inode's staging leaves the map of
   staged inodes in step 7, under its staging lock (§4.3); its files go after the durable commit of step 7 (§7.1).

### 7.5 Echo suppression

- The executor records every remote change it makes in the tree's remote state: the revision an upload returns (P6, also
  kept in `own_revisions`), the new parent and name of a rename, the trash. An event whose node, fetched with `get_nodes`,
  equals that state (parent, name, active revision) is an echo and changes nothing (§9.3); no listing is made.
- An event for an inode whose upload is in flight may arrive before the client knows the new revision. It then lists the
  directory again, and the listing leaves the inode alone: remote state recorded after the listing was fetched wins
  (`remote_stamp`), and pending content is never replaced (§8.2).

### 7.6 Atomic-save rename (temp → existing target)

This is the central pattern used by rsync, git, and editors.

| Source state | Remote action | Remote result |
|---|---|---|
| Source not uploaded (temps always qualify, because of the debounce) | Upload the source content as a **new revision of the target node**. Drop the source's pending create | Atomic: the target's link id, history, and shares are preserved |
| Source already remote | Trash the target, then rename/move the source (`OriginalHash` precondition) | Short window with no file at the name. Logged; documented in §17 |

Locally, the rename is always atomic: one in-RAM swap plus one store transaction.

### 7.7 `.pdignore`: local-only files

- File: `<mountroot>/.pdignore` (synced like any file) plus `~/.config/pdmount/<profile>.pdignore` (local). Gitignore syntax:
  a `/` at the start or in the middle anchors a pattern at the mount root, `*` and `?` match within one name, `**`
  crosses directories. Matcher adapted from `pdcli/pdignore.rs`. **The default list is empty.**
- "Ignored" means **local-only**: content lives in the staging file of the inode (§7.1) with the `local_only` flag set,
  survives remounts, and is never uploaded. Entries created inside a local-only folder are local-only too.
- An ignore rule only prevents creating new remote nodes. A remote file renamed to an ignored name keeps syncing.
- A local-only file renamed to a non-ignored name becomes a normal pending upload. If it is renamed over an existing remote target, it becomes a
  revision of the target (§7.6). pdcli instead trashed the original (`docs/pdcli-evaluation.md` §2 #4).
- Matchers are compiled once per `.pdignore` content generation.

### 7.8 Quota and errors

- Local disk full: `write` returns ENOSPC directly.
- Pre-check in `write`: if pending upload bytes plus quota used (from `storage_info`, cached 60 s) exceed the maximum, return ENOSPC.
- Remote quota failure during upload (200001, 200002, 200100) parks the job and sets `deferred_error = ENOSPC` on the inode. The next
  `flush`, `fsync`, or `close` on that inode returns it, then clears it. 200003 (file size limit) gives EFBIG.
- Parked jobs show in `pdmount status` (count), with paths and reasons in `pdmount queue list` and the `unmount` output;
  `pdmount queue retry` retries them (§8.2). Parked state is kept in RAM: a new local change of the inode, or the next mount,
  retries it. A job parked with `queue park` waits for `queue retry` or the next mount alone (D061).

---

## 8. Namespace operations

### 8.1 Local-first create and mkdir

- A new inode gets a fresh `ino` and `link_id = NULL`. The parent directory must be fully listed: the first touch of an unlisted
  directory lists it first (slow path).
- `O_CREAT|O_EXCL` returns EEXIST against the merged local view (remote-known, pending, and local-only entries).
- Mode = requested mode & ~umask. Owner = mounting user.

### 8.2 Journal as desired-state reconciliation

- Each inode row holds the **desired** state (`parent_ino`, `name`, generations, `deleted`). The `remote` table holds the **last
  known remote** state (`parent_link`, `name`, `name_hash`, `active_rev`, `size`, `mtime`, `mode`).
- The executor diffs desired against remote per inode and derives work items: CreateDir, CreateFile, UploadRevision, Rename/Move,
  Trash. Sequences coalesce naturally. For example, create → write → rename → unlink before the first upload produces nothing remotely.
- **Dependencies are by inode.** A create or move waits until the parent's `link_id` is known. A rename or trash waits until its own
  `link_id` is known. A blocked entry records what it waits for; the executor has one wake signal and rechecks a blocked entry after 5 s at the latest. No remote ids are captured at enqueue time.
- **Listings are merged, not copied:** a node follows its remote placement and attributes only while it has no pending
  local change of that kind; local nodes the server does not know are kept; a node removed remotely while something below
  it changed locally is reported as vanished and, once the server confirms it is gone, only the minimal chain of pending
  items and their folders is created again (§10.5, D040).
- **Name reservation:** before creating or moving a node to `(P, N)` remotely, any remote occupant that the local state has moved or
  deleted is processed first. Cycles (a ↔ b swaps) are broken through a temporary `.pdmount-swap-<ino>` name.
- **Backoff and parking:** transient errors back off exponentially (1 s → 5 min with jitter). Permanent errors (typed, P8) park
  the item with a reason, at once for quota, size, permission, too-many-children, nesting-too-deep and unsupported errors, after 5 attempts for the others.
  Nothing is retried in a tight loop, and parked items never grow without bound (one entry per inode).
- **Where the journal lives:** the entries (schedule, attempts, parked reasons) are in RAM; what is pending is derived from
  the inode rows at mount (new nodes, tombstones, generations, placements that differ from the remote ones). The `journal`
  table of §13.2 is not used.
- **Stale plans:** every local change bumps the entry's generation; a plan made from an older generation does not defer,
  block or remove the entry, so a change made while the executor plans is never dropped.
- **Draining:** `unmount` (`--wait` is the default) makes everything eligible at once and waits up to 10 minutes for the
  queue to empty; what remains continues at the next mount. `reset --keep-login` refuses while changes are pending.

**Queue tooling (D061).** `pdmount queue` works on journal entries, named by inode number:

- `list` shows id, kind (`upload`, `create-folder`, `move`, `trash`, `metadata`), state (`waiting`, `running`, `backoff until T`,
  `parked: reason`), attempts, age and path; `show` adds the last error, the base revision and what the entry waits for. A deleted
  file's entry is listed, and selected, at the place of its remote file. An entry the planner is about to remove is left out.
- `retry` clears park and back-off and forces the entry.
- `park` holds an entry with the reason `parked by user`. Unlike an automatic park, a local change does not resume it; `retry` or
  the next mount does. A step that is running finishes.
- `drop --yes` discards the local intent. A node with a remote counterpart goes back to its remote revision, attributes and place,
  and its parent is listed again; a delete is undone and the file comes back from the listing; a node that never reached the server
  is removed, with its subtree (`.pdignore`d files in it too; there is no other copy). The whole command is refused while a step of a
  selected entry runs. A node that cannot go back now (its old name is held by a node not dropped, its folder is deleted too, a
  name swap is half done) is reported, stays queued, and the command exits 1. No executor step starts on a selected entry meanwhile.

### 8.3 rename

fuser routes RENAME and RENAME2 to `rename(.., flags)` (`fuser/lib.rs:536`).

| Case | Result |
|---|---|
| `RENAME_NOREPLACE` and target exists | EEXIST |
| `RENAME_EXCHANGE`, `RENAME_WHITEOUT` | EINVAL |
| Source equals target | no-op success |
| File over directory / directory over file | EISDIR / ENOTDIR |
| Directory over non-empty directory | ENOTEMPTY (unlisted target directories are listed first) |
| Access rule violation (§11) | EROFS |
| Within the same parent (remote) | `rename_node` (`sdk/client.rs:1044`) |
| Across parents (remote) | `move_node(.., Some(name))` (P10; `sdk/node/operations.rs:342`), one call |

Before each remote rename or move the executor fetches the node fresh (`get_nodes`, one call) and decides (D040):

- **Gone or trashed:** the node is settled like any vanished one (§10.5), then planned again.
- **Moved or renamed remotely** since the plan was made (the remote committed first): the inode follows the remote placement
  and a `RenameRename` record is written. If it cannot follow (the folder is not shown here, the name is taken locally),
  the local placement is kept and pushed at the next plan, with a record saying so. A remote move to the very target
  of ours is no conflict.
- **Unchanged:** the rename carries the name hash just read as its precondition (`OriginalHash`, §10.4). If the server
  still refuses it (code 2000, `NameConflict`), the node is fetched again and treated as above; an unchanged node with a
  refused hash is parked.
- `InvalidMove` (the server refuses a move into the node's own subtree because another device moved things): the server's
  placement is kept, a `MoveCycle` record is written; if the inode cannot follow, the item is parked.
- `AlreadyExists` (a remote node unknown to us holds the target name): our node yields to its conflict name (§10.7), the
  target folder is listed, and a `NameTaken` record is written.
- `NotFound`: settled like a vanished node.

The server enforces the hash (M3.1, Q1b), so a race inside the pre-check's round trip is caught too. Were it not enforced,
a rename race would be last writer wins; a test documents that case.

### 8.4 unlink and rmdir

- `unlink` removes the name. If the inode is open, it becomes an **orphan** (`parent_ino = NULL`). Staging and cache pins are kept until the
  last release and `nlookup == 0` (forget); meanwhile `fstat` reports `nlink` 0. A remote node gets a Trash work item; the per-node result map is checked
  (`sdk/client.rs:1053`). A never-uploaded file cancels its pending work.
- `rmdir` returns ENOTEMPTY if any child exists, including pending and local-only children. An unlisted directory is listed first, so a directory
  with unknown remote children is never trashed blindly.
- Deletion is always to the Proton trash (recoverable). Nothing in v1 deletes permanently.
- **Trash pre-check (§10.5):** the executor fetches the batch's nodes fresh before trashing (one `get_nodes` per batch).
  A file whose active revision differs from the one the local delete was based on (or that is marked `remote_newer`) is
  not trashed: the tombstone is forgotten, the parent is listed so that the file reappears, and `LocalDeleteRemoteEdit` is
  recorded. A folder is listed uncached (`list_children`): a child that moves out locally but has not moved remotely yet,
  or a child folder deleted locally and not trashed yet (its own check lists it), makes the folder wait (transient,
  retried with backoff); an unknown child, or a child file that changed, keeps the
  folder (`LocalDeleteRemoteChildren`, with the count) and the folder comes back with them. Known, unchanged children
  are trashed through their own entries. A write that lands between the check and the trash call ends up in the
  trash with the file (recoverable, no server precondition).

### 8.5 setattr

| Attribute | Behaviour |
|---|---|
| mode | Stored `& 0o7777`. Sent as XAttr `pdmount.mode` on the next revision |
| uid / gid | Accepted only when equal to the mounting user; otherwise EPERM |
| size | Truncate (§7.2). Through a handle opened for writing (`ftruncate`) the write bit is not checked again |
| mtime (`TimeOrNow::Now` or a value) | Stored. Sent as `ModificationTime` on the next revision |
| atime | Local only (`inodes.atime_ns`); never set (0), it is reported as mtime |
| ctime | Updated locally on every change |

A mode or mtime change bumps `meta_gen`. On a dirty file it folds into the pending upload. On a clean file it creates a
**metadata-only revision** (the protocol has no metadata-only update, so the content is re-uploaded from cache or staging).
These are coalesced over 30 s and governed by `upload.meta_revision = always | max_size:<bytes> | never` (default `always`,
required by R5 and G5).

### 8.6 Directory attributes

Directory mode and mtime are stored locally. The creation-time mtime is sent with P5. No folder update API is known (TODO.md Q2;
decision D013), so on a fresh client
directories show their creation time. Scenario R1 therefore uses `--omit-dir-times` in `fresh`/`second` modes until an API exists.

### 8.7 Unsupported operations

| Operation | Reply | Why |
|---|---|---|
| `link` | EPERM | git's object finalization falls back from `link()` to `rename()` on errors other than EEXIST. Not yet verified against git source (TODO.md Q3) |
| `symlink`, `mknod` | EPERM | git `init` probes and sets `core.symlinks=false` |
| `setxattr` | ENOTSUP | — |
| `getxattr` / `listxattr` | ENODATA, except the read-only `user.pdmount.*` attributes below | Introspection |
| `fallocate` | EOPNOTSUPP | glibc `posix_fallocate` falls back |
| `copy_file_range` | ENOSYS (fuser default) | The kernel falls back to a generic copy |
| locks (`getlk`/`setlk`, flock) | Not implemented; the kernel provides local locking | Not shared across devices |

The `user.pdmount.*` attributes come from RAM only (`Engine::xattr`, `xattr_names`; hot path). A name with no value for the inode is
absent from the list and answers ENODATA. `getxattr` and `listxattr` follow the usual size protocol (size 0 returns the length, a
smaller buffer ERANGE); `setxattr` stays ENOTSUP, `removexattr` ENODATA.

| Name | Value |
|---|---|
| `user.pdmount.state` | The first that holds: `local-only` (`.pdignore`d, §7.7), `conflict` (an unresolved conflict names the inode, §10.8), `remote-newer` (a newer remote revision under dirty content, §10.2), `parked` (the journal gave up for now, §7.8), `uploading`, `pending` (queued, or changed since the last upload), `clean` |
| `user.pdmount.link_id`, `user.pdmount.rev` | The remote link and active revision; absent before the first upload |
| `user.pdmount.conflict` | `<id> <kind> <path of the other copy>` of the newest unresolved conflict of the inode, on both copies (`-` when the other copy is gone) |
| `user.pdmount.access` | `ro` or `rw` (§11) |

### 8.8 Names and inode numbers

- Names must be valid UTF-8 (otherwise EILSEQ) and at most 255 bytes (otherwise ENAMETOOLONG). That also satisfies Proton's limit of 255 UTF-16 code units
  (`cs/client/js/src/internal/nodes/validations.ts:5`). Names are case-sensitive and not normalized.
- Inode numbers are allocated from a persistent monotonic counter, one per `link_id` (or per local inode before upload), and never
  reused. Generation is 0. This keeps the git index stat cache and rsync stable across remounts.

---

## 9. Sync engine

### 9.1 Cursor

On first mount, the engine calls `latest_event_id()` (`sdk/client.rs:1110`) and persists the cursor **before** the initial root
listing, so no change can fall into a gap. The cursor is stored in `event_cursor`. It is queued to the store after the rows a
page changed, so a crash never stores it without them, and is not stored at all while a refresh runs (§9.4). Every mount
resumes the stream from the stored cursor, so changes made while unmounted arrive as events; a profile without a cursor
refreshes.

### 9.2 Loop

```
loop {
    page = poll_events(cursor)                    // sdk/client.rs:1121
    if page.refresh { cursor = start_refresh(); continue }   // §9.4, in the background
    apply(page.events)                            // §9.3; invalidations queued
    cursor = page.next; store(cursor) unless refreshing
    if page.more { continue }                     // drain immediately
    sleep(sync.poll_interval)                     // 30 s, as the official clients (D033)
}
```

Every API call of the loop (poll, `get_nodes`, listings) takes a background turn (D027), so user requests go first.
The interval runs from the end of a pass, not from its start, as in the official event manager
(`cs/client/js/src/internal/events/eventManager.ts:114-123`), so polls start `poll_interval` plus the length of the previous
pass apart. A remote change therefore shows after up to `poll_interval` plus two passes; the SLO is timed from the writer's
commit (D037, D050).

Errors never end the loop. After a failed poll the next one waits `poll_interval` times 1, 1, 2, 3, 5, 8, 13 (then 13 again,
6.5 min at 30 s) plus up to 1 s of jitter, as the official event manager does (`cs/client/js/src/internal/events/eventManager.ts:4,132`);
a 429's `Retry-After` is honoured when longer. A successful poll resets the sequence. Connection failures are counted
with those of every other caller in the shared health state (§9.6); once it is not `online`, the loop waits for `online`
instead of backing off and polls at once when it returns.

### 9.3 Applying events

Events carry only ids (F12). Applying a page (D034):

1. Keep the events whose link the tree knows, or whose parent is a listed directory. Others are dropped: a later listing
   fetches fresh state.
2. Resolve those links with `get_nodes`, fresh from the API, 50 links per request as in the official SDKs (F14, D035, P13).
3. A node whose parent, name and (for files) active revision equal the tree's remote state is an **echo** (§7.5) and needs
   nothing.
4. For every other node, list again the listed directories it is in now and was in before (its local parent and the directory
   of its known remote parent). A directory a node left is listed after the one it moved into, so a node moved between two
   listed directories keeps its inode and its listed subtree; a cycle (two nodes swapped) is broken in event order. A
   directory the server no longer lists (it is trashed) is skipped: the page must still complete and advance the cursor, and
   the pass after the listings settles what the folder held locally (§10.5). Concurrent listings of a directory are
   shared, but a request is answered by a listing that started after it (D054): one already in flight may have been
   fetched before the event. Only the first listing of a directory (`ensure_listed`) takes whatever listing runs. A remote
   state the executor wrote or just read is stamped (`Tree::set_remote_at`), so a listing fetched before the stamp leaves
   that node alone.

So remote changes take the same path as listings (`Tree::apply_listing_at`, §8.2), whose rules the property tests check:

| Change (event kind on the real API, M2.10) | Clean inode | Dirty / pending inode (D040) |
|---|---|---|
| Create (Create) | Inserted. `inval_entry(parent, name)` drops any cached negative dentry | A local node holds the name: the remote node is not shown and the occupant is a clash, settled when the journal next looks at it (§10.6). A `.pdignore`d occupant is renamed to a conflict name at once (`NameTaken`) |
| Rename, move (UpdateContent) | Applied. `inval_entry` on the old and new names, `inval_inode` on the directories. Onto a name a local node holds: the clean node is detached and returns when the name is free | A pending local rename loses against a remote one: the inode takes the remote placement (`RenameRename`, the first committer wins). Onto a used name, a node with local changes becomes a new local node under its old name or a conflict name (`NameTaken`); a folder keeps only its pending items and the folders that lead to them, and nodes moved into it from outside keep their pending move (with their subtree), while its clean contents stay in the remote folder and return with it; renames inside it that are the only pending change lose (`RenameRename`) (D052) |
| New revision (UpdateMetadata) | Rev, size, mtime and mode updated, the content epoch bumped (no `FOPEN_KEEP_CACHE` at the next open), `inval_inode` | Local content kept, `remote_newer` set and a notice sent (layer 1, §10.4); the upload settles it |
| Trash (UpdateContent) | Subtree removed, `notifier.delete(parent, ino, name)` (`fuser/notify.rs:102`) | Reported as *vanished*; the engine asks the server and resurrects the minimal chain (§10.5). A tombstone (local delete) whose file got a new revision is revived instead |

A listing never decides a vanished node itself: it keeps the old remote state and returns the node in `Changes::vanished`.
`Engine::resolve_vanished` then fetches the node and its remote ancestors fresh (children of a trashed folder are not
reported trashed), resurrects below the highest removed ancestor that holds the node locally, or, when nothing is gone,
lists the folder the node moved to. Events also resolve known nodes reported trashed or missing, and the nodes whose known
remote parent is such a node (moved out of it locally).

### 9.4 Refresh

When `refresh = true`, the engine keeps serving the current state. It takes a fresh `latest_event_id` and follows the event
stream from it at once, while a background task lists every listed directory again (one at a time, paced, D027) through the same
rules as §9.3. Only when that pass succeeds is a cursor stored again, so a crash during a refresh finds the old cursor and the
next mount refreshes again. A refresh asked for while one runs makes it run once more, because listings made before the newer
cursor may have missed changes. A failed pass is retried after 30 s.

### 9.5 Kernel invalidation queue

- One task owns a clone of `fuser::Notifier` (`Clone`, `fuser/notify.rs:52-53`). It receives `{InvalEntry, InvalInode, Delete}`
  messages from the store writer for every change that did **not** originate from a kernel request.
- The tree derives the messages; no mutation lists them by hand. Mutations record which directory entries they touch
  (`place`/`unplace` in `vfs.rs`, a listing records all entries of its directory) and which nodes' attributes changed.
  One pass at the end (`Effects::finish`) compares each touched entry with its state before the first change. For each
  directory that was listed before the change and is still attached, it sends one message per changed name, then
  `inval_inode` on the directory (its pages are cached, `FOPEN_CACHE_DIR`). The message is `delete` when the node that
  held the name has left the namespace, and `inval_entry` otherwise (a moved node must not be marked dead). Entries
  inside a removed subtree get nothing: the `delete` of its top covers them. Then `inval_inode` goes to each node whose
  attributes changed.
- ENOENT from the kernel (nothing cached) is ignored.
- The task never runs on a FUSE thread and never holds core locks while calling into the kernel.

### 9.6 Offline mode

One `Health` per engine holds the state `online`, `offline` or `needs_login` (`crates/pdmount-core/src/health.rs`, D041).
The backend the engine uses is wrapped in `Monitored`, which shows every result to `Health`:

- `Offline` (a connection failure, Appendix A): the shared failure count goes up; at 3 in a row the state becomes `offline`.
- `Unauthorized`: `needs_login` (§12.3). The wrapper then answers `Unauthorized` itself, with no request, until a login
  sets `online`.
- Any other answer, an error included, proves the server is reachable: the count is 0 and an `offline` state becomes `online`.

| Aspect | Behaviour |
|---|---|
| Entered | 3 consecutive `Offline` results from any caller (reads, event task, executor, probe) |
| Left | Any successful answer; in practice the probe |
| Probe | While `offline`: `storage_info` every `sync.probe_interval` (30 s), in a background turn. `needs_login` is probed only after a login (`Health::kick`, §12.3): the first answer ends it |
| Cached reads | Served |
| Uncached reads | Fail fast with EIO after `timeouts.offline_read` (2 s), instead of the 20 s / 60 s deadlines |
| Metadata of listed directories | Served |
| First listing of an unlisted directory | EIO (same 2 s) |
| Writes and namespace operations | Accepted and queued; local `fsync` keeps its normal deadline |
| Executor | Waits for `online`; failures from `Offline` / `Unauthorized` do not count as attempts, so nothing parks. On `online` every entry that is not parked retries at once |
| Event task | Waits for `online` and polls at once on the transition |
| Visibility | `pdmount status` shows `Connection: offline since <time> (N pending)`, or `needs login` |

**Mounting offline.** After every online mount the daemon keeps the volume, root and account name in `proton-root.json`
(§13.1). If the session check cannot connect, the daemon builds the session from the stored tokens (unchecked), creates the
backend from that file, opens the engine on the stored state (which makes no request once the profile was mounted online),
and starts in `offline`. A profile that was never mounted online cannot be mounted offline.

---

## 10. Multi-device and conflicts (RQ3)

### 10.1 Assumptions

- Several devices mount the same account, possibly at the same time, and the official apps (web, desktop, mobile) also edit the tree.
- Concurrent edits to the same file **will** happen. The system must never lose any version that any device acknowledged to an application.

### 10.2 Device identity and tracking

- Config `device.name` (default: hostname) and `device.id` (UUID, generated on first run, stored in `meta`).
  The SDK client uid is `pdmount-<device.id>`, which becomes the `ClientUID` on drafts (`sdk/node/draft.rs:177`).
- Per inode: `base_rev` (the revision local edits derive from) and `remote_rev` (the newest revision seen remotely).

### 10.3 The identical-content rule

If the snapshot's SHA-1 equals the remote active revision's claimed SHA-1, there is **no conflict**. The engine adopts the remote revision as
the new base and skips the upload. git objects and packs are content-addressed, and rsync re-runs write identical bytes, so neither produces
conflict copies.

A revision that claims no SHA-1 (the official apps may leave it out) is compared by content: same size, at most 16 MiB
(`IDENTICAL_PROBE_MAX`), and the SHA-1 of the remote revision, read through the block cache at readahead priority, equals the
snapshot's. Above 16 MiB without a claimed SHA-1 the versions are treated as different and a conflict copy is made (D040).

### 10.4 Edit/edit detection, in three layers

1. **Listing or event:** a listing that shows another revision for an inode with unuploaded content sets `remote_newer` and
   sends a notification. Local content is untouched. The planner then skips the upload debounce for that file (unless a
   writer is open), so the conflict is settled within seconds instead of after the quiet period.
2. **Pre-check:** before upload, `get_nodes([link])`. If `active_rev != base_rev` → the cases of §7.4 step 5: metadata-only
   changes adopt the other client's content, identical content moves the base, anything else is a conflict.
3. **Draft:** `upload_revision` sends `CurrentRevisionID = base_rev` (F11) when it creates the revision draft. A rejection is
   mapped to `RevisionConflict` (P11). If the fresh `active_rev` still equals `base_rev`, another client's draft is open and
   the upload is retried later; otherwise → conflict.

Server behaviour, verified on the real API in M3.1 (TODO.md Q1, Q6; docs/plans/m3.md, "M3.1 outcomes"):
- `CurrentRevisionID` is checked when the draft is created, not at commit. Revision drafts are exclusive per file: while
  one client's draft is open, another client's draft is refused (HTTP 409, code 2500, `Details.ConflictDraftClientUID`),
  so uploads of two clients cannot interleave. A version restore by another client can land inside our draft window and
  is then replaced by our commit; its revision stays in the history. There is no post-check (D043, §17).
- A draft left by a crashed client blocks other clients' drafts on that file until the same client UID uploads again
  (P11 removes its own stale draft) or the server collects it.
- `OriginalHash` is enforced on rename and on move (code 2000, "This file or folder was out of date, … failed."); the
  caller supplies the hash it last saw (P14, D038).

### 10.5 Policy: keep both; the first committer keeps the name

| Situation | Resolution |
|---|---|
| Edit/edit | The server version stays at the name. Local content is uploaded as a new file with the conflict name. Locally, the dirty inode is renamed to the conflict name (open fds follow it), and the server version appears at the original name as a new inode |
| Remote delete, local edit | **Resurrect**: upload the local content as a new file at the same path, recreating missing ancestors with new links (conflict name if the path is now occupied) |
| Local delete, remote edit | Before trashing, check `active_rev == base_rev` (§8.4). If not, **do not trash**: the newer remote revision reappears locally and a `LocalDeleteRemoteEdit` record is written. Trash has no server precondition, so a small window remains: a revision written inside it is in the trash with the file. The trash is recoverable, which mitigates it |
| Remote delete, local clean rename | The delete wins (content is in the Proton trash). Recorded. This holds for a node whose remote placement is inside the deleted folder. A node renamed into the deleted folder from outside it is not deleted on the drive: its rename loses, and it returns to its remote placement (`adopt_remote_placement`; a rename step whose destination folder is gone settles the folder, not the node) |
| Rename/rename (same node, different targets) | Fresh pre-check, then the `OriginalHash` precondition (enforced, §10.4; §8.3). The first committer wins: the inode moves to the remote name, and a `RenameRename` record is written |
| Move cycles rejected by the server | Keep the server's placement, write a `MoveCycle` record (park if the inode cannot follow) |
| Remote folder trashed, local pending descendants | Resurrect a minimal chain of folders containing only the pending items. Clean descendants are removed locally |
| Local folder delete, remote has unknown/newer children | Re-enumerate the folder uncached before trashing (§8.4). Trash only the children known and unchanged, and keep the folder with the rest (`LocalDeleteRemoteChildren`). Children that were moved out locally but not yet remotely, and child folders deleted locally but not trashed yet, make the folder wait |

The tree carries the pure steps of these resolutions (`vfs.rs`, unit-tested; the listing rules and `resolve_vanished` call them since M3.8, the journal from M3.9 on, including its rename and trash steps since M3.11):
`split_conflict` (edit/edit: the local inode takes the conflict name, the server version becomes a new inode at the original name),
`yield_name` (a local rename to a free conflict name), `unbind_tombstone`, `adopt_remote_placement` (refuses cycles, unlisted
folders and taken names), and `resurrect_minimal` (keeps the pending items and the folders that lead to them as nodes to create
again, removes the rest, and reports the removed nodes whose clean rename lost). `Changes` also carries `vanished`, `clashes`,
`notes` and `remote_newer` for them.

### 10.6 Create/create

The server refuses a create whose name is taken (`AlreadyExists`, 2500, with the holder's link when it names one; otherwise
the parent is listed to find it). The executor looks at the holder (`get_nodes`, fresh) and decides:

| Local node | Remote holder | Resolution |
|---|---|---|
| File | File with the same content (claimed SHA-1, or a read of at most 16 MiB, §10.3) | No conflict: the local node is bound to the remote file and builds on its revision. Mode and mtime that differ go up as a metadata revision |
| Folder | Folder | Merge: the local node is bound to the remote folder, which is listed to pick up its children. The same rules apply per child, recursively |
| Anything else | Anything else | The local node **yields**: it is renamed to its conflict name (§10.7; open descriptors follow), a `CreateCreate` record is written (state open), the parent is listed so that the remote node appears at the original name (the record then names it as the other copy), and the node is created under the conflict name |
| Local-only (`.pdignore`d) | Any | The listing moves the local node aside at once (`NameTaken`); it never uploads (§10.5, D040) |

A holder that is a draft or gone, or that another attached inode knows, is a transient failure (retried); the next plan
blocks on that inode. The record comes before the listing, so a stop that cuts the listing short leaves an open conflict
whose other copy is missing from the tree; at start, the folders of such `CreateCreate` and `NameTaken` conflicts are
listed again, and a failed listing goes to the relist queue (D055). A tombstone or detached inode that knows the remote node is unbound first. A parent folder that is
gone (`NotFound`) is settled like any vanished node (§10.5).

A clash whose local node leaves the name before it uploads (unlinked, or renamed away) has its folder listed again in
the background, so the hidden remote node appears (D046). The relist queue is kept in the store (`meta` key `relist`)
from the change that asks for a listing until a listing of that folder succeeds; at start, the folders it names that are
still listed are listed again, so a stop or crash in between hides nothing (D069).

### 10.7 Conflict names

```
<stem> (conflict <device> <YYYY-MM-DD HH-MM-SS UTC>)<.ext>
e.g.  report (conflict laptop 2026-10-19 14-03-27 UTC).odt
```

- `<device>` is the device whose content goes into the copy (`device.name`, or `remote` if unknown). `/`, parentheses and control characters become `_`, and the label is cut at 64 bytes (at a character boundary).
- The extension is split at the last dot, except for leading-dot names (`.bashrc` → `.bashrc (conflict …)`).
- On a collision, ` 2`, ` 3`, … is appended inside the parentheses. The stem is truncated at a UTF-8 boundary to keep the name ≤ 255 bytes; an extension too long to leave room for the suffix is treated as part of the stem.
- Names are deterministic for a fixed clock and device, which property tests rely on. The engine takes both as options (`EngineOptions::device`, `clock`); the tree's `Namer` turns them into names, and `Tree::pick_conflict_name` takes the first attempt that is free locally, remotely and for the caller's own reservations.

### 10.8 Surfacing

- Each conflict gets a row in the `conflicts` table: kind, ino, the second inode if any, link, original name, conflict name,
  both revisions, the local device, time, free-text detail, and state.
  - Kinds (`ConflictKind`, snake_case in the table): `edit_edit`, `create_create`, `name_taken`, `remote_delete_local_edit`,
    `remote_delete_local_rename`, `local_delete_remote_edit`, `local_delete_remote_children`, `rename_rename`, `move_cycle`.
    `interleaved_revision`, reserved for the revision post-check that D043 dropped, was removed; a stored row of a kind
    this version does not know is skipped at load, and its id stays taken.
  - States: `open` (the user has something to review: `edit_edit`, `create_create`, `name_taken`,
    `local_delete_remote_edit`), `notice` (the engine settled it: the other kinds), `resolved`.
  - The registry (`Conflicts` in `conflicts.rs`) is loaded at mount; `Engine::record_conflict` numbers a conflict (ids never
    repeat), queues its row like the tree rows (group commit, so a crash may lose the record but never the conflict copy),
    logs it at WARN level and sends a `ConflictNotice` to `Engine::subscribe_conflicts`. Its mutex is a leaf lock (§4.3).
  - `Engine::set_conflict_state` changes a state (what `conflicts resolve` uses) and queues the row. A stored row whose kind or
    state this version does not know (written by a newer one) is skipped with a warning at load, and its id stays taken.
- CLI: `pdmount conflicts list [--all] [<path>]` (unresolved ones; `--all` adds the resolved), `conflicts show <id|path>`,
  `conflicts resolve keep-both|keep-local|keep-remote <id|path>`. A target of all digits is an id; any other is a path (`./2024`
  for a file of that name), which names the newest unresolved conflict that has the file as either copy. `status` prints
  `Conflicts:  N open` while N conflicts are in state `open`. The daemon answers through `Request::ConflictsList|Show|Resolve`;
  the work is `Engine::conflicts_list`, `conflict_show` and `conflict_resolve` (`engine/surface.rs`).
- Resolution goes through the ordinary namespace operations, with current paths, so the journal does the remote side. The copy
  that has a conflict name is `ino`; the other copy is `other_ino`, or the node now bound to the conflict's link:

  | Kind | keep-both | keep-local | keep-remote |
  |---|---|---|---|
  | `edit_edit`, `create_create`, `name_taken` | mark resolved | `rename` of the conflict copy over the other copy (replace): a copy never uploaded becomes a new revision of the other file (§7.6), otherwise the other file goes to the trash | wait until the copy is uploaded (up to 120 s), then remove it: its remote node goes to the trash, recoverable |
  | `local_delete_remote_edit` | dismiss | remove the file that came back (trash) | dismiss |
  | other kinds (notices) | dismiss | refused: "not applicable to <kind>; use keep-both to dismiss" | refused |

  A copy that is gone is refused with "the other copy no longer exists; use keep-both". keep-remote is refused when the
  conflict copy is local-only (§7.7): it never uploads, so it would never reach the trash (the user deletes it or uses
  keep-both; D053). A resolved conflict is refused. The state change is written durably.
- WARN-level log, an INFO line with the command that shows the conflict, and a desktop notification (`notify-send`, only
  with a session bus; at most one per 30 s, later ones summed up in the next, D042). The xattrs of §8.7 mark both copies.
- **From M6 (D077, `docs/plans/m6.md`):** only `open` conflicts notify, through `org.freedesktop.Notifications` directly, with the
  file's path, what happened in plain words and a **Show** action; at most three per 30 s, the rest summed up. A `notified_at`
  column keeps unshown ones, and they go out when a desktop session appears.

### 10.9 git and rsync on shared trees

- **One writer per git working tree at a time.** Reading from other devices is fine. Two devices committing to the same working tree cannot be
  made safe by any sync filesystem.
- **A bare repository on the mount is a reasonable shared remote** when pushes are sequential. Objects and packs are content-addressed (§10.3).
- If a conflict does hit `.git`:

  | Part of `.git` | Outcome |
  |---|---|
  | objects / packs | No conflicts (identical-content rule) |
  | refs | The conflict copy name (`main (conflict …)`) is not a valid ref name; git warns and ignores it, and the server's ref wins |
  | index | The loser's copy is ignored; `git status` or `git reset` recovers |

  After any conflict notice under `.git`, run `git fsck`.
- **rsync:** use one writer per destination tree. Re-runs are idempotent because of the identical-content rule.

---

## 11. Access control per path (RQ4)

### 11.1 Model

- A rule sets an entry **and all its sub-entries** to `ro` (Read) or `rw` (Read+Write).
- **The nearest rule wins.** The effective mode of an entry is the mode of the closest ancestor-or-self that has a rule. With no rule anywhere, the root default
  applies: `access.default = "rw"`. `mount -o ro` is equivalent to a root rule of `ro`.
- Rules are **bound to the node** (`link_id`, or `ino` for entries not uploaded yet), and they record the path at the time they were set,
  for display.
  - A rule follows its entry through local and remote renames and moves.
  - If the entry is deleted, the rule becomes `orphaned`. It is shown by `list`, never applied to a new entry with the same name, and removed with
    `access unset`.
- Rules are **local to the device and profile**. They are not synced to Proton.

### 11.2 Enforcement

Rules are enforced in the FUSE handlers before any state change. Denied operations return **EROFS**, so rsync and git report "Read-only file
system".

| Operation | Checked against |
|---|---|
| `open` with `O_WRONLY`/`O_RDWR`/`O_TRUNC`, `write`, `setattr` (size, mode, times, owner) | the entry |
| `create`, `mkdir` | the parent directory |
| `unlink`, `rmdir` | the parent directory |
| `rename` | source parent **and** target parent (and the target entry when replaced) |

- Moving an entry with a rule carries the rule. Moving a subtree out of a `ro` area is denied, because its source parent is `ro`.
- **Reported attributes:** in `ro` subtrees, `getattr`/`lookup`/`readdirplus` clear the write bits (`& !0o222`). Tools see read-only
  before they try to write. The stored mode is unchanged, so switching back to `rw` restores the exact original mode.
- **Permission checks** are the daemon's, not the kernel's: the mount has no `default_permissions`, which would answer
  EACCES from the cleared bits before PdMount could answer EROFS (D031). The engine checks the owner bits of the stored mode
  (read, write, search) and answers `access(2)`; EROFS comes first.
- **Remote changes keep syncing into `ro` subtrees.** `ro` blocks local writes; it does not freeze synchronization. Conflict handling is
  unchanged.

### 11.3 Resolution

A check walks up parent links to the nearest rule (bounded by tree depth); with no rules at all it is O(1). A rule change
increments `meta.access_version`. Memoizing per inode is a later option if profiling shows the walk.

### 11.4 Changing rules at runtime

The command goes from the CLI to the control socket to the daemon. No remount is needed.

1. Resolve `<path>` to an inode (listing it if needed). Reject paths outside the mount.
2. Persist the rule and bump `access_version` in one durable transaction.
3. Queue `inval_inode` for every inode in the subtree that is cached by the kernel, so the reported mode bits update without waiting for the TTL.
4. Switching to `ro` while writable handles are open:
   - New write opens are refused.
   - Later `write`/`setattr` calls on already-open handles return EROFS.
   - Content already accepted (dirty or in staging) and queued journal items **still upload**: they were acknowledged before the switch.
     `access set` prints how many pending items fall inside the subtree.

### 11.5 CLI

Following the CLI convention, the path is always the last argument (§14.1).

```
pdmount access set ro|rw <path>     # create or replace the rule on <path>
pdmount access unset <path>         # remove the rule on <path>
pdmount access list [<path>]        # all rules, or rules at/below <path>
pdmount access effective <path>     # effective mode of <path> and the deciding rule
```

`<path>` is absolute inside the mountpoint or relative to the mount root.

Example output:

```
$ pdmount access list
MODE  PATH                      SET AS                    SOURCE  STATUS
ro    /Archive                  /Archive                  cli     active
rw    /Archive/inbox            /Archive/inbox            cli     active
ro    /Photos-old               /Old/Photos               config  active
ro    -                         /Scratch/tmp              cli     orphaned
(default: rw)

$ pdmount access effective Archive/inbox/a.txt
rw  (rule: /Archive/inbox)
```

Rules can be preseeded in `<profile>.toml`:

```toml
[access]
default = "rw"

[[access.rule]]
path = "/Archive"
mode = "ro"
```

Config rules are imported at mount time. From then on the database is authoritative, and `list` marks each rule's source (`cli` or `config`).

---

## 12. Authentication and session

### 12.1 Login

- Login uses `ProtonAPISession::begin_via_web(app_version, options, on_sign_in)` (`rs2/session.rs:244`). It polls every 5 s for up to 10 minutes.
- At login, PdMount prints: *"PdMount is a third-party application, not officially supported by Proton."*
- Credentials are stored with `to_stored_credentials_with_latest_tokens()` (`rs2/session.rs:223`) in the OS keyring (service
  `pdmount`, entries `<profile>:session` and `<profile>:cache-key`). Files are used only with a flag (§12.4, D029).
- If the new session cannot be stored, `login` ends it on the server and fails.
- `pdmount logout` revokes the session where possible and deletes the stored credentials and cache key. It reports keyring
  entries it could not delete and exits non-zero.

### 12.2 Presenting the sign-in link (RQ5)

This applies to `pdmount login` and to `pdmount mount` when there is no valid session.

- The sign-in URL and user code from the `on_sign_in(url, code)` callback are **always printed** on the command line.
- The link is **also opened in the default browser** (`xdg-open`) according to `--browser` / `--no-browser`, or
  config `auth.browser = "auto" | "always" | "never"`.
  - `auto` (default): open the browser only when the process is interactive (stdin and stdout are TTYs) and `$DISPLAY` or `$WAYLAND_DISPLAY` is
    set. Otherwise (SSH, headless) the link is only printed.
  - If launching the browser fails, PdMount prints a warning; the printed link remains the fallback.
- **`mount` without a session:** `mount` runs the login flow before mounting (browser per `auth.browser`). With `--no-login`, it
  exits with an error instead (for scripts).
- **`pdmount login` while a daemon runs:** the daemon runs the sign-in and keeps its mount (D042). The command asks for the link
  (control request `login`), presents it as above and polls `status` until the daemon's login is over. A login as another
  account than the mounted one is refused.
- **From autostart or in the background:**
  - The daemon shows the link in `pdmount status` (`Sign-in:` and `Code:` under `Session:`), which reads it over the 0600 control
    socket. The log carries only the code and a hint to run `pdmount login` or `pdmount status`; the desktop notification
    (`notify-send`, only with a session bus; at most one per 30 s) says "Sign-in needed for profile <p>: run `pdmount login`".
    Neither carries the link: its fragment holds half of the fork secret, and `notify-send`'s argv is readable by other users (D071).
  - The mount stays up in `needs_login` and does not exit. The flow lasts 10 min; `pdmount login` starts a new one on demand.

```
$ pdmount login --no-browser
PdMount is a third-party application, not officially supported by Proton.
Open this link to sign in:  https://account.proton.me/…
Code: ABCD-EFGH
Waiting for sign-in… done. Logged in as <account>.
```

### 12.3 Token lifecycle

- **Rotation:** `token_credential.subscribe_tokens_refreshed()` (`rs2/auth.rs:284`) persists every rotation immediately, in a task of
  its own (a keyring prompt may take 120 s). A rotated refresh token that is lost would force a re-login. A 60 s timer stays as a
  fallback.
- **Reactive refresh (`with_reauth` around every `Backend` call):** when the adapter classifies a response as 401, it calls `token_credential.get_refreshed_access_token(rejected)`
  (`rs2/auth.rs:335`, single-flight; `token_credential` is public at `rs2/session.rs:37`) and retries once. Clones share token state
  (`rs2/auth.rs:22-73`), so the drive client never needs rebuilding.
- **Proactive refresh:** an auth task owning the session (`&mut`, behind a tokio Mutex) calls `ensure_authenticated()` (`rs2/session.rs:503`)
  every 30 min while online; an expired session sets `needs_login`, connection errors are left to the health state (§9.6).
- **Refresh failure** (P9 surfaces it; `subscribe_refresh_token_expired()`) moves the daemon to `needs_login`:
  - local reads of cached data and all local writes continue;
  - network-bound requests return EIO;
  - the link presentation of §12.2 starts;
  - a login for the mounted account replaces the backend's session (`replace_session`), kicks the probe (§9.6) and the queue drains.
    A mount can start in `needs_login` when it has local state and a known root (D042).

### 12.4 Secrets at rest

- The SDK secret and entity cache is `EncryptedCacheRepository::new(SqliteCacheRepository::open_file(..), key)`
  (`sdk/cache/encrypted.rs:57`, `sdk/cache/sqlite.rs:52`), with a random 32-byte key in the keyring. The secret cache must persist,
  or keys cannot be decrypted after a resume.
- The secret cache holds at most 5000 entries and drops those read least recently. The account key passphrases (`account:passphrase:*`)
  stay out of it, in a key store with no limit and the same encryption (`sdk-keys.db`; D085). One stored by a version before D085 is not
  moved: that profile needs one new login (D088). Active keys that cannot be unlocked all the same count as `Unauthorized`, so the state is `needs_login` (§12.3) and nothing parks.
- The disk block cache is encrypted per block (§6.2).
- **Secret storage modes (D029).** Two mutually exclusive global flags select the mode:

  | Mode | Flag | Secrets go to |
  |---|---|---|
  | keyring | none | the keyring only. An unavailable keyring fails the command, naming the cause and both flags |
  | allow-file | `--allow-file-secrets` | the keyring; 0600 files (§13.1) only when the keyring is unavailable |
  | force-file | `--force-file-secrets` | 0600 files only; the keyring is never called |

  - **Keyring answers:** found, missing, or unavailable. Unavailable covers no Secret Service, a dismissed prompt
    (Cancel), a timeout and any other error. Only "missing" counts as no entry.
  - **Waiting:** a locked collection waits for its unlock prompt; `dbus-secret-service` waits up to a year. Each keyring
    call runs on a helper thread and is abandoned after 120 s (`PDMOUNT_KEYRING_TIMEOUT_SECS` overrides it for tests). After 2 s, a note on stderr says that an unlock dialog may
    be open, possibly behind other windows. Once the keyring is unavailable, the process does not call it again, except
    for the daemon's last token save at shutdown.
  - **Reads:** a secrets file, if present, is the newest copy, because every successful keyring write deletes it. It is
    read first, in every mode. Outside force mode it is then moved into the keyring and deleted, unless another daemon of
    the profile is running (the pid file, §13.1), in which case it is left in place.
  - **Cache key:** creating a new cache key deletes the SDK caches and the block cache, which the old key encrypted.
    The SDK secret cache goes with them, so in force mode the session and the cache key must both be in files. In
    allow-file mode, an unavailable keyring with no key file creates a key only when none of these caches exist;
    otherwise the command fails, because the key that encrypted them may be in the keyring.
  - **Daemon:** `mount` reads the stored session in the foreground process before it starts the daemon, so an unlock
    dialog appears while the user waits at the terminal. The daemon gets the same flag. It stores tokens only when they
    have changed (§12.3).
  - **Tests:** tests use force mode, except the keyring tests (`scripts/repro/keyring-d024.sh`).
- Staging is plaintext with mode 0700 (§17).
- The SDK fork never logs secrets or response bodies (P16, `docs/sdk-patches.md`). Upstream 0.4 logs the key passphrase
  (`rs2/account.rs:226`), the cached address keys (`rs2/api/cache.rs:249`), the serialized passphrase (`rs2/secret.rs:63`, `:67`),
  the `/users` body (`rs2/users.rs:83`) and the session probe body at debug; the drive SDK logs share, thumbnail-block, link-move,
  copy, link-details and trash bodies.
- Defence in depth: the log filter (`main.rs` `log_filter`) drops every `proton_sdk_rs2` record above WARN after the `RUST_LOG`
  or `-v` directives, so no directive, however specific, shows its debug records.

### 12.5 Identification

- `x-pm-appversion`: `external-drive-pdmount@<semver>-alpha` via `AppVersionConfiguration` (`rs2/utils.rs:47`).
- No Proton logos or branding.
- One session per profile. Two profiles never share a refresh token.

---

## 13. Storage

### 13.1 Directories

| Path | Content |
|---|---|
| `~/.config/pdmount/<profile>.toml` | configuration |
| `~/.config/pdmount/<profile>.pdignore` | local ignore rules |
| `~/.local/share/pdmount/<profile>/state.db` | SQLite store (WAL) |
| `~/.local/share/pdmount/<profile>/staging/` | staging files and snapshots (0700) |
| `~/.local/share/pdmount/<profile>/staging/<ino>.state` | staging sidecar: base revision and materialized blocks (§7.1) |
| `~/.cache/pdmount/<profile>/blocks/` | encrypted block cache |
| `~/.local/share/pdmount/<profile>/sdk-secrets.db` | SDK secret cache (encrypted, capped) |
| `~/.local/share/pdmount/<profile>/sdk-keys.db` | account key passphrases (encrypted, §12.4). Losing it forces a new login, so it is not in the cache directory |
| `~/.local/share/pdmount/<profile>/credentials.json`, `cache.key` | session and cache key in the file modes of §12.4 (0600) |
| `~/.local/share/pdmount/<profile>/proton-root.json` | volume, root folder and account name from the last online mount, for mounting offline (§9.6) or waiting for a login (§12.3) |
| `~/.local/share/pdmount/<profile>/device-id` | device identity (§10.2). Stays a file so `reset --keep-login` keeps it |
| `~/.cache/pdmount/<profile>/sdk-entities.db` | SDK entity cache (encrypted, rebuildable) |
| `$XDG_RUNTIME_DIR/pdmount/<profile>.sock` | control socket (0600); `~/.local/share/pdmount/<profile>/control.sock` when the session has no runtime directory |
| `<profile>.pid` beside the control socket (`control.pid` in the fallback) | process id and mountpoint of the mount daemon, for `unmount --force` and for `status` after a crash |
| `$XDG_STATE_HOME/pdmount/<profile>.log` | log; a background `mount` moves one above 16 MiB to `<profile>.log.1`, replacing that; 0600 in a 0700 directory, set again at every background `mount` |

### 13.2 Schema

```sql
meta(key TEXT PRIMARY KEY, value BLOB)
  -- root_link, ino_reserved (inode numbers up to it are handed out; reserved 16 384 at a time, durably),
  -- access_version, access_config_imported:/<path>, relist (folders to list again, space-separated inode numbers, D069).
  -- The schema version is `PRAGMA user_version`.

inodes(ino INTEGER PRIMARY KEY, kind INTEGER, parent_ino INTEGER NULL, name TEXT NULL,
       link_id TEXT UNIQUE NULL, mode INTEGER, mtime_ns INTEGER, ctime_ns INTEGER, atime_ns INTEGER,
       size INTEGER, listed INTEGER, local_only INTEGER, deleted INTEGER, nlookup_persist INTEGER,
       content_gen INTEGER, meta_gen INTEGER, uploaded_content_gen INTEGER, uploaded_meta_gen INTEGER,
       state INTEGER, remote_newer INTEGER, deferred_error INTEGER NULL)
  -- remote_newer (0/1): a newer remote revision exists than the dirty content's base (§10.2); column from migration 1, written since M3.6.
  -- UNIQUE(parent_ino, name) WHERE parent_ino IS NOT NULL
  -- mode, mtime_ns, atime_ns (0: never set) and size are the local state; tombstones (deleted = 1) have no parent and name.
  -- Rows are rewritten in two steps (names and links cleared first), so renames and link adoptions in one batch
  -- never collide with the unique indexes. Migration 2 fills `mode` of M1 rows from the remote mode or the default.

remote(link_id TEXT PRIMARY KEY, parent_link TEXT, name TEXT, name_hash TEXT, kind INTEGER,
       active_rev TEXT NULL, size INTEGER, mtime_ns INTEGER, mode INTEGER NULL, sha1 BLOB NULL,
       trashed INTEGER, seen_at INTEGER)

staging(ino INTEGER PRIMARY KEY, base_rev TEXT NULL, size INTEGER, materialized BLOB,
        snap_gen INTEGER NULL, snap_meta_gen INTEGER NULL)
  -- not used: the staging sidecar holds this (§7.1)

journal(ino INTEGER PRIMARY KEY, kind INTEGER, state INTEGER, attempts INTEGER, next_at INTEGER,
        last_error TEXT NULL, parked_reason TEXT NULL, updated_at INTEGER)
  -- not used: the journal is derived from the inode rows at mount (§8.2)

own_revisions(link_id TEXT, rev TEXT, created_at INTEGER, PRIMARY KEY(link_id, rev))

revision_blocks(link_id TEXT, rev TEXT, idx INTEGER, plain_size INTEGER, sha256 BLOB,
                PRIMARY KEY(link_id, rev, idx))

-- pin = 1: never evicted, not counted against cache.max_bytes (§6.2)
cache_blocks(link_id TEXT, rev TEXT, idx INTEGER, bytes INTEGER, last_access INTEGER, pin INTEGER,
             PRIMARY KEY(link_id, rev, idx))

-- a file or a whole folder kept offline (D060); deleted with its node
pins(ino INTEGER PRIMARY KEY, path_at_set TEXT, created_at INTEGER)

conflicts(id INTEGER PRIMARY KEY, kind TEXT, ino INTEGER, link_id TEXT NULL, orig_name TEXT,
          conflict_name TEXT NULL, local_rev TEXT NULL, remote_rev TEXT NULL, local_device TEXT,
          remote_device TEXT NULL, created_at INTEGER, state TEXT, other_ino INTEGER NULL, detail TEXT NULL)
  -- other_ino and detail since migration 3; index conflicts_state(state); created_at in unix seconds;
  -- remote_device is unused (always NULL; nothing recorded it)

event_cursor(volume TEXT PRIMARY KEY, event_id TEXT, updated_at INTEGER)

access_rules(id INTEGER PRIMARY KEY, ino INTEGER UNIQUE, link_id TEXT NULL, path_at_set TEXT,
             mode TEXT CHECK(mode IN ('ro','rw')), source TEXT CHECK(source IN ('cli','config')),
             status TEXT CHECK(status IN ('active','orphaned')), created_at INTEGER, updated_at INTEGER)
```

**Migrations:**
- Numbered SQL files, applied in one transaction at startup.
- A downgrade is refused.
- `inodes.state`, `inodes.nlookup_persist`, `remote.trashed` and `revision_blocks.sha256` are not used.
- `pdmount reset --keep-login` deletes the store, staging and block cache; the session, the SDK caches, `proton-root.json` and the device identity stay. It is refused while the journal has pending rows.

---

## 14. CLI, control socket, operations

### 14.1 Argument convention

Options come first, then verb or mode arguments, then **the path last**, with at most one positional path. `--profile <name>` is a global option
(`pdmount --profile work status`; also accepted after the subcommand), as is `-v/--verbose`. The global options `--allow-file-secrets` and
`--force-file-secrets` (mutually exclusive) select the secret storage mode (§12.4).

### 14.2 Commands

| Command | Purpose |
|---|---|
| `login [--browser\|--no-browser]` | Web sign-in (§12.2); with a running daemon, the daemon runs it (D042) |
| `logout` | Remove the session and keys |
| `mount [--foreground] [--browser\|--no-browser] [--no-login] [-o ro] [--backend proton\|fake:<dir>] [<mountpoint>]` | Start the daemon and mount (mountpoint overrides config). `--backend fake:<dir>` and `pdmount fake seed\|changes\|rename\|trash\|write\|create\|mkdir\|fault\|profile` exist only in test builds (feature `fake-backend`, §15.1). `--no-login` fails instead of signing in (M3.16). `-o ro`: every write path answers EROFS, as a root `ro` rule would, without changing the rules (D019); pending work still uploads |
| `unmount [--wait\|--force] [<mountpoint>]` | `--wait` (the default): upload what is pending first, up to 10 minutes, and list parked items (§8.2). `--force`: see §14.4 |
| `status` | Online/offline/needs_login, queue depth, oldest pending, parked items, conflicts, cache use, sign-in link if pending. M2: session, quota, mount, `Queue: N pending, M parked`, inodes, cache. M3.14: `Conflicts:  N open` when N > 0. M4.12: `Queue: N pending (oldest 3m), M parked`, `Events: last poll 12 s ago[, refreshing]`, `read-only` on the mount line. Without a mount it resumes the session itself, but not while the daemon is still shutting down (a resume could rotate the tokens the daemon is about to save) |
| `sync [--timeout <s>] [<path>]` | Remote barrier (§5.6): upload now and wait until nothing under `<path>` is pending. Exit 1 with parked entries or at the timeout (default 600 s) |
| `access set ro\|rw <path>` / `access unset <path>` / `access list [<path>]` / `access effective <path>` | §11.5 |
| `pin <path>` / `unpin <path>` / `pin list` | Keep offline (§5.3), through the daemon. `list`: path, files, bytes fetched of known total, state (`fetching`, `complete`, `waiting-offline`, `waiting-space`). `unpin` takes the path of the pin itself and names it when given a path inside. `status` adds `Pinned: N paths, X of Y fetched` when N > 0 |
| `queue list [<path>]` / `queue show [--id <ino>] [<path>]` / `queue retry\|park\|drop [--id <ino>] [--all] [<path>]` | Inspect and control the journal (§8.2, D061). Ids are inode numbers; a path selects its entry or, for a folder, every entry under it. `drop` prints what it would discard and changes nothing without `--yes` (exit 1) |
| `conflicts list [--all] [<path>]` / `conflicts show <id\|path>` / `conflicts resolve keep-both\|keep-local\|keep-remote <id\|path>` | §10.8 |
| `cache stats` / `cache gc [--target <bytes>]` / `cache wipe [<path>]` | Block cache maintenance, through the daemon. `stats`: disk bytes against the cap, blocks, pinned bytes, protected revisions, pool slots. `gc`: evicts the oldest blocks down to the target (default 80 % of `cache.max_bytes`) and prints what it freed. `wipe`: evicts every block that may go, also from RAM, or only those of files at or below the path. Pinned and protected blocks stay (§6.2) |
| `stats [--json] [--reset]` | Counters, gauges and latency histograms (§5.8) |
| `reset --keep-login` | Rebuild local state (refused while the queue is non-empty, while mounted, and while the daemon is still shutting down) |
| `trash empty --yes` | Delete everything in the account's trash for good (Proton only; refused while the profile is mounted or its daemon is still shutting down, since the daemon holds the session). Real-account test runs use it when storage runs low (D036) |
| `doctor` | Check `/dev/fuse`, fusermount3, `/etc/fuse.conf`, keyring (reachable, default collection locked or not, read without prompting; the effective secret storage mode), config, login, clock skew, disk space, pdmount and app version; says whether autostart is enabled for the profile |
| `autostart [enable\|disable]` | XDG autostart entry `$XDG_CONFIG_HOME/autostart/pdmount-<profile>.desktop` that runs `pdmount --profile <p> [<secret-storage flag>] mount` at desktop login; without an argument, shows whether it is enabled (D059). The `Exec` line names the running binary, quoted by the Desktop Entry spec |
| `service [install\|uninstall] [--manager openrc\|systemd]` | M5: user service for the profile, OpenRC or systemd; without an argument, shows its state (D074, `docs/plans/m5.md`). Refused while `autostart` is enabled for the profile, and the other way round |

### 14.3 Control socket

JSON-lines request/response over a Unix socket at `$XDG_RUNTIME_DIR/pdmount/<profile>.sock` (0600, peer uid checked). `access`, `conflicts`, `pin`, `unpin`,
`queue`, `sync`, `cache`, `stats` and `unmount` work through it; `login` and `status` use it when a daemon answers, and `mount` to see
that one is ready or already runs. `logout`, `doctor`, `autostart`, `reset` and `trash empty` do their work without it; the last two use it only to refuse while a daemon runs.

### 14.4 Unmount and hang safety

- No `AutoUnmount` unless `user_allow_other` is present (`fuser/session.rs:163-171`). `doctor` explains the `/etc/fuse.conf` change.
- **Autostart (D059):** no service manager supervises the daemon. A crashed daemon is not restarted; its stale mount is
  cleared with `unmount --force` (below). At logout the daemon ends with the session's SIGTERM if the session manager
  sends one, and otherwise keeps running until `unmount`.
- **User service (M5, D074):** an OpenRC user service or a systemd user unit supervises `mount --foreground`, restarts it after a
  crash, clears a stale mount at start, and stops it with SIGTERM at session end or shutdown, with a stop timeout of
  `shutdown_grace` + 30 s.
- **Flush (M5, D075; replaces step 2 below from M5.1):** SIGTERM, SIGINT or SIGHUP uploads what is queued at that moment, without
  the debounce, for up to `timeouts.shutdown_grace` (90 s from M5.1). No wait while offline or in `needs_login`, none for parked
  entries; rate limits and backoff apply. The next mount warns if entries were left. With D076, a flush that runs
  out of time leaves `.pdmount/<device.id>.pending` in the root of each tree (§14.5) that has entries left and has the marker turned
  on, and other devices and members with that tree mounted show it.
- Shutdown on SIGTERM, SIGINT or `unmount` (D062):
  1. The journal stops starting steps. New opens are not refused.
  2. Unmount.
  3. Wait up to `shutdown_grace` (30 s) for running uploads and inline steps.
  4. Commit the store.

  Pending work persists and resumes at the next mount, also an upload the grace cut off.
- **Starting:** `pdmount mount` re-executes itself as `pdmount mount --foreground` in its own process group, with stdout and
  stderr appended to the log file (D026), and returns once the control socket answers `ping` (90 s limit). It refuses to
  start while the profile's daemon answers or while the mountpoint is a mount (a dead pdmount mount is reported as stale).
- **Unmounting:** `pdmount unmount` asks the daemon, which runs `fusermount3 -u`. A busy mount stays mounted and the error is
  reported. On SIGTERM/SIGINT the daemon unmounts lazily if needed. Refreshed session tokens are saved every 60 s and at
  exit, even when the store commit at exit fails.
- After `kill -9`: `pdmount unmount --force`:
  1. Runs `fusermount3 -uz`.
  2. Stops a still-running daemon (pid file, checked against `/proc/<pid>/cmdline`): SIGTERM, then SIGKILL after `shutdown_grace` + 30 s,
     which outlasts the daemon's own 10 s wait for the session, its grace, its store commit and its last token save. Its exit
     closes `/dev/fuse` and so ends the connection.
  3. If `/sys/fs/fuse/connections/<minor>` still exists, writes to its `abort`. That file is root-owned on most systems; the
     command then prints the root command instead.

  The connection id is the minor device number from `/proc/self/mountinfo`, which never touches the dead mount. A reboot is
  never required (exit criterion of M1).
- **Copies of the mount in other mount namespaces:** a sandbox that starts while mounted (Flatpak's bubblewrap, `unshare -m`)
  copies the mount. An unmount in the daemon's namespace does not end the FUSE connection then, and the session threads
  keep running. The daemon therefore treats "gone from its own mount table" as unmounted; a table it cannot read (EMFILE,
  say) is read again, never taken as unmounted. It waits at most 10 s for the session, then exits; closing `/dev/fuse` ends the connection. The copies then report ENOTCONN until their sandbox exits.
  `unmount` returns only after the daemon process is gone (pid file), and `mount` refuses to start while it still runs.

### 14.5 Further roots (M5, D078–D081)

One daemon and one session serve several mountpoints, one per root: My Files stays on its mountpoint with its paths, store and
behaviour unchanged, and `shared`, `computers` and `photos` are each configured with their own.

- **Trees:** a device and a share are each a tree (volume, root link, write gate) with its own engine, store, journal and staging,
  the same code as My Files. A mount with several trees has a virtual read-only top level, with one folder per tree. All trees share
  the session, the transfer and rate limits, and one event poller per volume (60 s on other users' volumes, as the official client).
- **Writes (M5.14):** Computers, and shares where the node's role is `Editor` or `Admin`, go through the My Files write path. `Viewer`
  shares and shared single files return EROFS (D019). Losing the right with changes pending parks them and closes the gate; staged
  data is kept, under `orphaned/<tree>/` once the share is gone. Rename between trees returns EXDEV.
- **Photos:** a read-only virtual tree over the timeline and albums, not an engine tree: it has no folder operations (D080). Year and
  month folders come from timeline pages; names and sizes are fetched when a month is opened. Proton's stored thumbnails go to
  `~/.cache/thumbnails` in the background, so a file manager need not open the originals (D081; `photos.thumbnail_cache` turns it off).
- **Bulk-read guard (all trees, D081):** many different files opened in quick succession become a scan: below interactive priority,
  paced, no readahead, a limited share of the block cache. Opening a single file stays interactive.

What the API allows in practice is settled by M5.9 (`docs/plans/m5.md`).

---

## 15. Testing strategy

### 15.1 Layers

| Layer | Content |
|---|---|
| Unit | Name validation, conflict naming, ignore matcher, errno mapping, block offset math (with and without `BlockSizes`), access resolution |
| Fake backend | Proton model: name uniqueness (2500), `CurrentRevisionID` and `OriginalHash` preconditions, trash, quota, an event log with `more`/`refresh`, several client handles on one drive (`with_client`; a client has a UID and may leave out the SHA-1 and the mode like the official apps, `pdmount fake write\|create --app`). Revisions record the client UID, claimed SHA-1 and creation order (`revisions()`). Pause points (`pause_at`) stop the next upload, folder creation, rename or trash of a client just before its commit, so a test can let another client commit first; uploads are two-phase (draft, then commit), `set_commit_checks(false)` models the server as M3.1 found it (the base revision is checked only at draft creation, D043), and `set_name_hash_enforced(false)` a server that ignored `OriginalHash` (it does enforce it, §10.4; the test documents the alternative). `all_revision_sha1s()` lists the content still reachable. Fault injection: latency, one-shot failures per operation, and `faults.json` in the fake's directory for running daemons (`{"rules":[{"kind":"offline"|"unauthorized","until_ms":…,"client":"pdmount-<device id>"}]}`; every call checks it, re-read when the file changes; `pdmount fake fault offline\|unauthorized [--for <s>] [--client <uid>] <dir>` and `fault clear`; the daemon's client UID is `pdmount-<id>` from `pdmount status`'s "Device" line; the fake has no sign-in, so a daemon in `needs_login` probes every 2 s and goes online when the rule is gone, D047). **Network profile** (M4.2): `profile.json` in the fake's directory (`{"rtt_ms":80,"up_bytes_per_sec":20000000,"down_bytes_per_sec":20000000}`; `pdmount fake profile [--rtt <ms>] [--up <B/s>] [--down <B/s>] <dir>`, `fake profile clear <dir>`; in memory `set_profile`), re-read when it changes in the same per-call pass as `faults.json`. Every call costs one RTT; `list_children` one per 150 children and `get_nodes` one per 150 ids (at least one); uploads three (draft, blocks, commit) plus `size / up`; block fetches one plus `len / down`. Each direction is one pipe per process, shared by its concurrent transfers (a transfer starts when the one before it ends), so two daemons on one fake each have their own pipes. The cost is slept after the fault checks and before the call reads or changes the drive, outside the state lock. The fake keeps a child index per folder and a running quota total, and reads blobs after releasing its lock, so it stays cheap under load. Planned: partial batch failures, crash points. Persistable to a directory, so two daemons can share one fake (`--backend fake:<dir>`, test builds only): `snapshot.json` plus an append-only `journal.jsonl` (one line per change, written before the call returns, so a `kill -9` of the daemon loses nothing), compacted into the snapshot at 8 MiB; an `epoch` file tells other handles to reload. Growth is bounded: the last 16 revisions per file (`PDMOUNT_FAKE_KEEP_REVISIONS`), 1024 trashed nodes, 10 000 events and 50 000 changes are kept; dropped revisions delete their blobs. Changes and events keep absolute sequence numbers across trimming (`pdmount fake changes --after <n>`) |
| Property (proptest) | Two tests in `engine/proptests.rs`. `random_changes_converge_to_the_model`: one device against a POSIX model, with restarts, crashes and another device's changes at clean points. `two_devices_and_an_app_converge` (M3.17): two engines (`dev-a`, `dev-b`) and an app on one fake, operations interleaved with pending local changes (no drain before a remote change), restarts and crashes of either device, explicit event syncs. Invariants listed below. Regression cases of both are replayed in `regression_cases` and `concurrent_regression_cases` (the latter with a debounce long enough that steps meet in the written order, one sequence per M1, M3–M7 interleaving; M2 needs a pause at the commit and is `m2_edit_edit_interleaved_at_commit`) |
| Mounted, CI | Local CI script (`scripts/ci.sh`) on the dev machine with fuse3, over the fake backend; hosted CI deferred (D023). `--full` runs every scenario below, the default a subset, `--quick` none (D084). pjdfstest and fsx run separately. **pjdfstest subset** (`tests/pjdfstest.sh`; pjdfstest built from source at the commit named in `tests/pjdfstest.list`): open (incl. O_EXCL), rename, unlink, mkdir, rmdir, truncate, ftruncate, chmod, utimensat, with the file-type loops cut to regular files and directories; link, symlink, mknod and mkfifo give EPERM by design (§8.7, scenario N1) and are left out. The failures of each test file must equal the list's: cases that need other users (the agent is not root, and without `user_allow_other` only the mounting user reaches the mount, D020), unsupported file types, directory `nlink` 1, and ENOSPC from the quota check (§7.8) for a huge truncate. **fsx** (`tests/fsx.sh`; xfstests' `ltp/fsx.c` built standalone): two instances at once with mapped reads and writes, 256 KiB and 8 MiB files; 6 h for the M2 exit check; without `-c`, which needs root for `drop_caches`. fsx, pjdfstest and soak never run against the real account (D030) |
| Benchmarks | `scripts/bench/m4.sh [git\|untar\|read\|slo\|gc\|all]` (D057): the M4 exit rows over the fake backend with the 80 ms / 20 MB/s network profile, profile state on the disk, the drive in tmpfs, local comparisons on the same filesystem; report in `target/bench/m4-<date>/report.md`. The `slo` row checks the daemon's own p99 latencies (`pdmount stats --json \| scripts/bench/slo.py`, §5.8) against §5.7, as the median of 5 rounds; a round with a bare `fdatasync` p99 above 10 ms before or after it does not count (`scripts/bench/fdatasync.py`, D086); the "first byte" and "first listing" rows of §5.7 are backend-call counts checked by engine tests. Client-side tools: `scripts/bench/latency.sh`, `scripts/bench/read.sh`. Not part of `scripts/ci.sh` |
| Real account | Two profiles of one account on one machine (two devices), each with its own login. Scenario scripts only, under `pdmount-test/` (D028), one run at a time, stopping at the first HTTP 429; datasets kept modest (D030). Manual; long runs only with the user's approval |
| Soak | `tests/soak.sh`, over the fake backend with two devices on one drive (D030). 3 h (D048; `SOAK_DURATION`). Loops: rsync of a mutated tree every 10 min (about 5 % of the files added, changed or deleted), a git commit every 30 s on A (`gc` every 50 commits), edits on B every 60 s and of A's rsync target every 10 min (conflicts expected), an official-app write (`fake write --app`) every 5 min, a sampler every 60 s (VmRSS, queue, counters). Faults go through the fake's fault file, not `ip link`: the drive unreachable for all clients for 5 min every 40 ± 10 min (`fake fault offline`), A's session rejected for 5 min every 60 min from minute 30 (`fake fault unauthorized`, then `clear`). Pass: (a) no EIO outside the fault windows plus 70 s (loop failures and `answered EIO` warnings of both daemon logs), (b) zero parked jobs, (c) the largest RSS of the last third of the run at most 1.25 × that of the window from 1/6 to 1/3 of the run, per daemon, (d) `rsync -ani --checksum` between the two mounts prints nothing and `git fsck --full --strict` is clean on both, (e) `handler_panics` and `hot_path_network_calls` are 0 |

**Property test invariants:**
- After every step, the local view matches a POSIX model. Steps include restarts, crashes and another device's changes
  (write, create, rename, trash) made on the fake at clean points; those arrive through the event stream (§9).
- After quiescing:
  - the fake tree equals the model: names, bytes, mtime, mode;
  - the journal is empty;
  - a fresh core started from an empty database sees the same tree;
  - **no content ever acknowledged is unreachable** (it is at a name, in a conflict copy, in history, or in the trash);
  - two cores converge to identical trees.
- Two devices and an app (M3.17), which have no model, since conflict handling decides the outcome:
  - a local operation fails only with ENOENT, EEXIST, ENOTDIR, EISDIR, ENOTEMPTY or EINVAL, never EIO;
  - the tree is consistent after every step (`Tree::check_invariants`);
  - after quiescing (up to 20 rounds of drain and event sync, until both queues are empty and the drive's change log stops
    growing): nothing is parked; both cores and the fake show the same names, kinds, bytes and (where the drive records
    one) modes; every content a device wrote and did not delete itself, and every content the app uploaded, is in
    `all_revision_sha1s()`; a fresh core from an empty database sees the same tree.

### 15.2 FUSE semantics that rsync and git depend on

| Semantic | rsync | git | Design |
|---|---|---|---|
| `O_CREAT\|O_EXCL` | `.name.XXXXXX` temps | `*.lock`, `tmp_obj_*`, `tmp_pack_*` | §8.1 |
| Atomic rename over an existing file | temp → final | `index.lock`→`index`, ref locks, `packed-refs.lock`, `config.lock` | §7.6, §8.3 |
| Temps never reach the server | — | — | §7.3 debounce |
| setattr mtime | `-t` (on the temp before rename; dirs after) | object/pack freshening `utime(NULL)` | §8.5 |
| setattr mode | `-p` | 0444 objects/packs, `core.filemode` probe | §8.5, F10 |
| fsync / fsyncdir | `--fsync` | `core.fsync` | §5.6 |
| `link()` → EPERM fallback | `-H` only | object finalization falls back to rename | §8.7 |
| symlink → EPERM | `-l` gives per-file errors | `core.symlinks=false` | §8.7 |
| Unlink of open/mmapped files | `--delete` | gc removes packs | §8.4 orphans |
| mmap reads | — | packs, idx, index | §5.4 (no DIRECT_IO) |
| In-place partial overwrite + ftruncate | `--inplace` | — | §7.2 |
| `O_APPEND` | — | reflogs | kernel i_size, coherent attrs |
| Correct st_size and st_mtime from the API | quick check | index stat cache | F1 / P4 |
| Stable inode numbers across remounts | — | index stat cache | §8.8 |
| Files over 40 MiB | large files | packs | F2 / P1 |
| readdir stable under mutation; rmdir | `--delete` | prune of `objects/xx` | snapshot per opendir; §8.4 |
| Write denied in `ro` subtree → EROFS | error report | error report | §11.2 |

### 15.3 Scenarios

The R, G, N and A scripts, `sha` and `g6_read` take `VERIFY_MODE`; `m*` and `e2` run live or skip other modes, and `queue` (§8.2, §5.6), `ops` (§14.4) and `kill9` (M1 exit, §16) are live only:

| Mode | Meaning |
|---|---|
| `live` | Verify on the same mount |
| `remount` | `unmount --wait`, `mount`, verify |
| `crash` | `kill -9` during the run with the queue non-empty, `unmount --force`, `mount`, wait for the queue to drain, verify |
| `fresh` | `reset --keep-login` (store, staging and block cache empty), mount, verify. The tree and content come from the API; the SDK's caches stay |
| `second` | A second profile (separate login, separate state; the fake: `fake-scenarios-2`, device `dev-b`) mounts the same account (or the same fake) and verifies. Details below |

**`second` mode** (`lib.sh`: `PROFILE2`, `MNT2`, `pd2`, `mount_second`, `device_uid`). Device B is mounted before the scenario's
changes, so they reach it as events (and, first, A and B wait for each other's earlier events with a marker each way, so the
backlog of the scenarios before is not counted). After the scenario's writes `verify_cycle` waits for A's queue, writes a
marker file `.marker-<scenario>-<n>` in A's `pdmount-test/`, waits for A's queue again and polls B for it. **The marker must
show on B within 40 s of its commit** (`poll_interval` + 10 s, D037; the clock starts when the writer's queue is empty, D050), plus 15 s for each further page of 50 changes the scenario made (fake only: listings and
lookups are paced, D027, and a page takes B about 12 s; D047). Then B's queue is awaited and `PROFILE`, `MNT`, `TEST_ROOT` and
`REMOTE` switch to B: every check after `verify_cycle` reads what B received through its event stream. A's access rules
(local state) are removed before the switch, and the access scenarios check only what was uploaded, like `fresh`; A5 is skipped.
Checks on B may write (`git status` refreshes the index), so the next scenario starts with a marker from B to A, too.

With `BACKEND=fake`, everything a run writes (fake drive, dataset, mountpoint, work directories, and the test profile's
state, staging, cache and log through the XDG variables) lives under `SCENARIO_ROOT`, by default the tmpfs
`/dev/shm/pdmount-tests`, to spare the disks. The test profile gets a small cache (`CACHE_MAX`, default 1 GiB) and the fake
keeps 4 revisions per file. Real-account runs keep the local dataset copy in `~/.cache/pdmount-scenarios/m1`.

**rsync**

| ID | Steps | Pass condition |
|---|---|---|
| R1 | `rsync -a src/ M/dst/` (mixed tree: 1k small files, nested dirs, a 100 MiB file, modes 0644/0755/0600) | `rsync -ani src/ M/dst/` and `rsync -ani --checksum src/ M/dst/` both print nothing. **In `fresh` and `second` modes, this is the acceptance test that size and mtime are read correctly from the API (F1/P4).** Dir times: `--omit-dir-times` in fresh/second until a folder update API exists (§8.6) |
| R2 | Modify content and mtime of k files, re-run | Exactly k files itemized; one new remote revision each |
| R3 | Delete files and dirs in src, `rsync -a --delete` | Gone in all modes and in the Proton trash. No temp names were ever created remotely (fake op log / listing) |
| R4 | `rsync --inplace --no-whole-file` edit in the middle of a file over 40 MiB | sha256 matches in all modes |
| R5 | `touch` a subset only, re-run | mtimes correct on `second` (metadata-only revisions) |
| R6 | `rsync -a M/dst/ out/`, then re-run | Second run transfers nothing. `out/` equals `src/` by checksum |
| R7 | Interrupt rsync mid-transfer, re-run | Final state correct. No `.*.XXXXXX` remains locally or remotely |

**git**

| ID | Steps | Pass condition |
|---|---|---|
| G1 | `git init` in the mount | `core.filemode=true`, `core.symlinks=false`, `core.ignorecase=false` |
| G2 | `git clone` (https and a local path) into the mount | `git fsck --full --strict` clean; `git status` clean |
| G3 | 200-commit loop: adds, modifications, renames, `chmod +x` | `git log` hashes identical in every verify mode |
| G4 | `git gc --aggressive`, `git repack -a -d`, `git prune` (pack > 40 MiB, 0444 modes, unlink while mapped, `objects/xx` removal) | fsck clean, `git count-objects -v` sane |
| G5 | Checkout and branch switching between divergent branches (mass rewrite, deletions, mode flips) | `git status` clean; modes preserved on `second` |
| G6 | `git init --bare M/r.git`; push from outside (quarantine migration = cross-directory renames); `git clone M/r.git` and `git clone file://M/r.git`; push more; fetch | Clones and fetches work. `fsck` clean on `second` |
| G7 | Round trip after G3–G5 in `fresh` and `second` | `git fsck --full --strict` clean, `git status` clean, `git rev-parse HEAD` identical |

**Negative and access control**

| ID | Steps | Pass condition |
|---|---|---|
| N1 | `link`, `symlink`, `renameat2(RENAME_EXCHANGE)`, `renameat2(RENAME_NOREPLACE)` onto an existing file | EPERM, EPERM, EINVAL, EEXIST |
| A1 | `pdmount access set ro <dir>` while mounted | create/write/truncate/unlink/rmdir/rename/chmod/touch in the subtree give EROFS; `stat` shows no write bits; `rsync -a` into it fails with "Read-only file system"; `git commit` in a repo inside it fails cleanly |
| A2 | `access set rw <dir>/sub` under a `ro` parent | Writes in `sub` succeed; writes elsewhere in `<dir>` still fail |
| A3 | `access set rw <dir>` (or `unset`) at runtime | Everything succeeds without a remount; original modes reappear |
| A4 | `access list`, `access list <dir>`, `access effective <file>` | Output matches the rules and their deciding rule |
| A5 | Rename the `ro` directory from the second client; delete another ruled directory | Rule follows the rename (`list` shows the new path, `active`); the deleted one shows `orphaned` |
| A6 | Open a file for writing, write, `access set ro` on its parent, write again, close | Second write → EROFS; the first write's data still uploads (verified on `second`) |

**Event sync** (the fake's CLI is the second client, so `BACKEND=fake` only)

| ID | Steps | Pass condition |
|---|---|---|
| E1 | While mounted, another client renames a directory, trashes a file and writes a new revision of a file read before | All three visible without a remount within 40 s (`poll_interval` + 10 s, D037), measured after the daemon has caught up with the earlier scenarios' events (a file made by the other client shows first: `event_barrier`); the old names are gone from the kernel's caches
| E2 | (`live`) The drive is unreachable for A for 90 s while `rsync` writes into the mount; then the session is rejected (`fault unauthorized`) and a file and a folder are made; then `fault clear` | `status` shows `Connection: offline`, then `online`, then `needs login`, then `online` again (the fake daemon probes while `needs_login` lasts, standing in for a sign-in). Cached data reads offline; a file never fetched fails within 3 s (`timeouts.offline_read`). Local writes are accepted in both states. After `fresh`, everything is on the drive, nothing parked and the file reads its new content; the state survives the verify mode |

**Multi-device** (`live` mode, two profiles A and B on one fake or, for the cases marked, on two real profiles; other modes skip them). A file held open for writing on one device keeps its change local while the other device's change arrives; the scenario keeps it open until a marker written after the change, in the same directory, shows on that device (the listing that shows the marker shows the change), then closes it (`reach_b_while_held`). After both queues drain, a marker goes each way (`sync_devices`) before `ls`, `cat` and `pdmount conflicts list <dir>` are checked on both mounts.

| ID | Script | Interleaving | Pass condition |
|---|---|---|---|
| M1 | `m1.sh` (fake, real) | Edit/edit, caught by the pre-check: B holds `f` open and has written; A writes `f` and uploads; B closes | `f` is A's content on both; exactly one conflict copy named per §10.7 holds B's; B lists one `edit_edit` |
| M2 | (engine test `m2_edit_edit_interleaved_at_commit`) | Edit/edit, interleaved at commit | Same as M1 (commit-time layer); needs a pause at the commit, which a daemon cannot be given |
| M3 | `m3.sh` (fake, real) | Remote delete vs local edit: B holds `f` open; A deletes `f` | `f` exists again on both with B's content; B lists `remote_delete_local_edit` |
| M4 | `m4.sh` (fake) | Local delete vs remote edit: A, offline, deletes `f`; B writes `f` | `f` is B's content on both; A lists `local_delete_remote_edit`; `f` never went to the fake's trash |
| M5 | `m5.sh` (fake) | Rename/rename: A, offline, renames `f` to `g`; B renames it to `h` | Only `h` on both; A lists `rename_rename` |
| M6 | `m6.sh` (fake, real) | Remote folder delete with a local pending child: B holds `d/new` open; A deletes `d` (with `a`, `b`) | `d/` holds only `new` on both |
| M7 | `m7.sh` (file part: fake, real; folder and app parts: fake) | File: B holds a new `c.txt` open while A creates one. Folder: A (offline) and B both make `dd` with different children. App: an official client (`fake write --app`) writes a new revision of a clean file | File: A's `c.txt`, one conflict copy with B's content, B lists `create_create`. Folder: one `dd` with both children. App: the content is adopted on both, no conflict listed |

**Session end and notifications** (M5, M6; fake with the network profile unless marked; plans in `docs/plans/m5.md`, `m6.md`).

| ID | Interleaving | Pass condition |
|---|---|---|
| X1 | Write files, SIGTERM at once, within the debounce | All on the fake drive before the daemon exits; exit well under the grace |
| X2 | Queue larger than the grace can upload, SIGTERM | The daemon exits within the grace; the rest uploads at the next mount; no loss |
| X3 | SIGTERM while offline; empty queue | Exit with no flush wait |
| X4 | Remount after X2 | One warning notification and log line; none after X1 |
| X5 | By hand, Gentoo and Debian: logout and reboot with a queue that fits, crash, locked keyring | Changes visible on the other device, or a documented limit from M5.0; restart after a crash; `needs_login` without a hang |
| X6 | Private bus with a stub notification service: conflicts of each state; bus absent, then present | `open` ones named and shown once, escaped; `notice` ones not shown; missed ones shown when the bus appears |
| X7 | Fake: several roots with My Files; a remote change in each; a write to a read-only root; a share revoked. Real, read-only: the user's own shared item, device and photo | Every root lists and reads with the right sha256; remote changes appear without recursive listing; writes return EROFS; a revoked share disappears without an error on the others; the My Files scenarios are unchanged |
| X8 | Fake: the R, G, N and A scenarios and M1, M3, M6, M7 on a share with `Editor` and on a device, in `live`, `remount`, `crash`, `fresh`; a `Viewer` share; the role lowered and the share revoked with changes pending; `mv` between trees | Same pass conditions as in My Files; `Viewer` writes return EROFS; nothing staged is lost, and it is listed and kept; `mv` copies after EXDEV |
| X9 | Real, with the user's exception to D028: a small rsync and git run in a test share from another account and in a test device | Content is right on the owner's side and in the official client; the owner's account sees the commits' author as this account |
| X10 | Fake with the network profile: open a folder of 2,000 images in a file manager with thumbnails on, then open one image; the same with the thumbnail cache off; the same folder in My Files | With the cache on, no originals are downloaded for thumbnails; in every case, no HTTP 429 from the fake's rate rule, the opened image's first byte stays within the §5.7 read SLO, and reads on another mount stay responsive |

---

## 16. Milestones

Exit criteria are functional tests. A milestone is done when its listed tests pass.

| Milestone | Scope | Exit criteria |
|---|---|---|
| **M0 Foundations** | Workspace; SDK fork with P1, P2, P4, P8; `Backend` trait and fake; local CI script that builds `gopenpgp-sys` (Go ≥ 1.22, clang, protoc); `login`/`logout`/`status`/`doctor` | `pdmount login` opens the browser **and** `login --no-browser` prints the link; both complete sign-in. `status` shows the account and quota. Fake-backend unit tests pass. `doctor` passes on the Gentoo machine |
| **M1 Read-only mount** | Inode table, store, listing, block cache (P3), readahead and prefetch, notify queue, `catch_unwind`, deadlines, unmount tooling; writes return EROFS | **R6** and **G6 read side** (clone, fsck) on data uploaded by the official client, in `live` and `fresh` modes. This confirms API size, mtime, and content. sha256 of 5 MB / 41 MB / 2 GB files equals the web download. `hot_path_network_calls == 0`. `kill -9` → `unmount --force` → remount without a reboot |
| **M2 Writes, single device, access control** | Staging, journal, §7–§8, `fsync=local`, `.pdignore`, quota errors, metadata revisions, P5, P6, P10; access rules (§11); event engine for clean inodes with refresh and echo suppression (§9.1–§9.5, §7.5; D033) | **R1–R7, G1–G7, N1, A1–A6, E1** pass in `live`, `remount`, `crash`, `fresh` on the fake (CI), and all but A5 and E1 (which need a second client) in `live`, `remount`, `fresh` on the real account. pjdfstest subset green and fsx 6 h clean, both on the fake (D030); journal property tests at 10k cases |
| **M3 Multi-device and sync robustness** | Remote changes to dirty inodes, the full conflict engine, offline mode, auth refresh and `needs_login`, P9, P11, P12; verify server precondition behaviour (§10.4) | **M1–M7** on the fake, and the cases marked "real" in §15.3 (M1, M3, M6, the file part of M7) on two real profiles: M2, M4, M5 and the folder and app parts of M7 need fault injection or a pause at the commit (D051). Every R/G/A scenario in `second` mode on two real profiles. Zero data loss. Remote changes visible within `sync.poll_interval` + 10 s of the writer's commit, 40 s at the default (D037, D050). 3 h soak on the fake passes (D030, D048) |
| **M4 Performance and operations** | SLO instrumentation, benchmarks, P7 parallel uploads, pins, writeback-cache experiment, `cache gc`, queue tooling, autostart entry (D059), documentation | §5.7 SLOs met (median of 5 rounds on a quiet disk, D086). Over the fake backend with an 80 ms RTT / 20 MB/s profile: `git status` on warm git.git ≤ 2× local btrfs; untar of 5k small files ≤ 15× local (time to acknowledgement; D065); cached sequential read ≥ 500 MB/s; `cache gc` keeps the cache ≤ 1.05 × the cap, and ≤ the cap after gc (definitions: D057) |
| **M5 User services, flush and further roots** | OpenRC and systemd user services (D074); flush on SIGTERM, warning at the next start (D075); pending marker (D076); Shared-with-me, Computers and Photos (D078); read-write for Computers and writable shares, Photos read-only (D080); bulk-read guard and Photos thumbnails (D081) | **X1–X4**, **X7**, **X8** and **X10** on the fake; **X5** by hand on Gentoo (OpenRC) and Debian (systemd); **X7** read-only on the real account; **X9** if the user grants the D028 exception; pjdfstest subset and a short fsx on a share tree on the fake; no regression in the §16 M2 scenarios on the fake |
| **M6 Conflict notifications** | Named, actionable D-Bus notifications; missed ones shown later (D077). A resolution UI is not decided | **X6**; by hand on the user's desktops: **Show** opens the folder; a conflict on the other device while logged out appears at the next login |
| **M7 Upstream and crypto migration** | Upstream patches; track Proton's new crypto model; rebase the fork | Fork diff reduced to unmerged patches; migration plan documented; full matrix re-run |

---

## 17. Risks

| Risk | Impact | Mitigation |
|---|---|---|
| Proton cryptography migration (end 2026 / early 2027, `cs/README.md:23-28`) | Old clients stop interoperating | The `Backend` trait isolates crypto. Keep the fork small. `doctor` checks versions. Drain the queue before upgrading |
| Community SDK maturity (pre-1.0, single maintainer; F1–F9) | Bugs and API churn | Pinned fork, patches upstreamed, contract tests against the fake encoding protocol assumptions |
| A version restore by another client lands while our revision draft of that file is open (`CurrentRevisionID` is not checked at commit, §10.4) | Our commit replaces the restored version without a conflict copy | Needs a restore of the same file during the seconds of an upload; the restored revision stays in the history (D043) |
| A local delete is uploaded while another device writes a new revision of the file (trash has no server precondition, §10.5) | The new revision goes to the trash with the file | The pre-check (`active_rev == base_rev`) narrows the window to the seconds between it and the trash call; the trash is recoverable |
| The daemon crashes between a conflict and the group commit of its record (§10.8) | The conflict copy exists, but `conflicts list` does not show it | The copy carries the conflict name (§10.7) and is never lost; only the record and its notice are |
| The soak runs 3 h instead of 24 h (D048) | Slow memory or disk growth and rare long-run faults may go unseen before the M3 exit | Fault loops run often enough to recur several times in 3 h; the RSS check compares windows relative to the run length; a longer run needs only `SOAK_DURATION` |
| A client crashes with a revision draft open | Other clients cannot upload that file until the crashed client uploads it again (P11 removes its own stale draft) or the server collects the draft | Other devices treat the refusal as transient and retry with backoff (§10.4) |
| Rate limits under git/rsync workloads (thousands of small uploads) | 429 lockouts | Debounce and coalescing, 4 upload workers, adaptive backoff honouring `Retry-After`, prefetch budgets |
| Metadata-only revisions re-upload content | Bandwidth, revision history growth | 30 s coalescing; `upload.meta_revision` policy; visible in `queue list` |
| Plaintext staging and local-only files | Local data exposure | Mode 0700; recommend full-disk encryption; block cache encrypted |
| No remote atomic rename-over when the source is already remote | Short window with the name missing remotely | Rare in practice (temps are never uploaded); logged |
| Two concurrent git writers on one working tree | Repository-level inconsistency | Documented single-writer rule (§10.9); conflict copies as a last resort |
| `AutoUnmount` requires `user_allow_other` | Stale mount after a crash | `unmount --force`, `doctor` guidance |
| FUSE_INTERRUPT not supported by fuser | Stuck requests | Deadlines on every slow path; Reply drop → EIO |
| Terms of service | Account action | Honest app version, event-based sync, backoff, third-party notice, no Proton branding |

---

## Appendix A. Error mapping

| Condition | errno / behaviour |
|---|---|
| `NotFound` (2501) | ENOENT; the cached node is invalidated |
| `AlreadyExists` (2500) | create: conflict naming (§10.6); journal: re-reconcile |
| `PermissionDenied` (2011) | EACCES |
| `Quota` (200001, 200002, 200100, 200101) | ENOSPC (deferred to flush/fsync/close for async uploads) |
| `FileTooLarge` (200003) | EFBIG |
| A write or truncation of a remote file larger than `MAX_STAGED_SIZE` (4 TiB, §7.1) | EFBIG |
| `NameConflict` (code 2000, "out of date": a rename or move with a stale `OriginalHash`) | The journal fetches the node again (§8.3); never an errno |
| `InvalidMove` (code 2000, "sub-folder": a folder moved into itself or a descendant) | EINVAL (not retryable) |
| Commit refused because the draft was removed (code 2511) | `Transient`: the retry creates a new draft |
| `TooManyChildren` (200300) | EMLINK (parked) |
| Nesting too deep (200301) | ENOSPC (parked, plus a conflict record) |
| `RateLimited` (429), `Transient` (5xx, timeouts) | Retried with backoff; EIO only when an interactive deadline expires |
| Connection refused, DNS or connect failure (`reqwest::Error::is_connect`, also inside `reqwest_middleware::Error`) | `Offline` (retryable, never parks a journal item) |
| Request timeout; a connection that breaks before the answer is complete (`is_request`, `is_body`, `is_decode`); block transfer 408, 425, 5xx | `Transient` |
| Block transfer 429 (`TooManyRequestsException`, `HttpTransferError`) | `RateLimited` with the `Retry-After` delay |
| HTTP 401, or `Code` 401 in a 2xx body (`ApiError`, block transfer) | `Unauthorized` |
| `Offline`, uncached read | EIO after `timeouts.offline_read` |
| `Unauthorized` after refresh failure | `needs_login`; EIO for network-bound requests |
| Active user keys that no stored passphrase unlocks (the SDK's message, no error type; D085) | `Unauthorized` |
| `Integrity` (hash or signature mismatch; a block that decrypts to another size than claimed) | EIO; nothing cached; ERROR log |
| Invalid UTF-8 / name > 255 bytes | EILSEQ / ENAMETOOLONG |
| link, symlink, mknod, chown to another user | EPERM |
| `RENAME_EXCHANGE`, `RENAME_WHITEOUT` | EINVAL |
| Write in a `ro` subtree | EROFS |
| setxattr / missing xattr | ENOTSUP / ENODATA |
| fallocate | EOPNOTSUPP |
| Local disk full | ENOSPC |
| Panic in a handler | EIO (Reply drop); the mount stays up |

## Appendix B. Configuration reference

```toml
# ~/.config/pdmount/<profile>.toml
mountpoint = "~/ProtonDrive"

[device]
name = "laptop"                 # default: hostname

[auth]
browser = "auto"                # auto | always | never

[access]
default = "rw"                  # rw | ro
# [[access.rule]] path = "/Archive"  mode = "ro"

[cache]
max_bytes = "10GiB"
ram_bytes = "32MiB"             # at least 16MiB; the block pool, reserved at startup (§6.2)
prefetch_small_files = "256KiB"
prefetch_dir_budget = "16MiB"

[upload]
concurrency = 4                 # 1-4: files uploaded at once (D033, D058)
debounce = "2s"
max_dirty_age = "10m"
meta_revision = "always"        # always | max_size:<bytes> | never

[fsync]
mode = "local"                  # local | remote
remote_paths = []               # globs of paths from the mount root that use remote fsync, e.g. "/important/**"; an invalid one is a config error

[kernel]
attr_ttl = "60s"
entry_ttl = "60s"
negative_ttl = "10s"

[sync]
poll_interval = "30s"           # event polling; at least 30s, the official clients' interval (D033)
probe_interval = "30s"          # connectivity probe while offline or signed out; at least 10s (D041)

[timeouts]
metadata = "20s"
block = "60s"
offline_read = "2s"
shutdown_grace = "30s"          # how long a stopping daemon waits for running uploads (D062)

[stats]
prometheus_textfile = ""        # e.g. "~/node-exporter/pdmount.prom"; empty = off (§5.8)
```
