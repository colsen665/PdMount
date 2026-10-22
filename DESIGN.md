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
| RQ8 | Scope is My Files only | — |

### 1.2 Non-goals (v1)

- Shared-with-me, Computers/Devices, Photos, and Trash as browsable trees.
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
- `pdcli/service.rs`: systemd user unit

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
`NestingTooDeep`, `RevisionConflict{current}`, `NameConflict`, `RateLimited{retry_after}`, `Transient`, `Unauthorized`, `Integrity`,
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

- **Read (warm):** kernel `read`, then vfs, then cache RAM hit, then reply. No disk access, no network.
- **Read (cold):** kernel `read`, then cache miss, then a slow-path task: `block_map` (memoized), then `fetch_block`, then verify, then
  decrypt-at-rest encode to disk, then insert into the RAM LRU, then reply. Readahead schedules the next blocks.
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
| Slow | first listing of an unlisted directory, read miss, materializing a partially overwritten remote block, `fsync` in `remote` mode, `rmdir`/rename-over of an unlisted directory | tokio task holding the Reply; bounded by a semaphore (64) | metadata 20 s, block 60 s; when offline, `offline_read_timeout` (2 s) |

When a deadline expires, the request replies EIO and logs it. The background fetch continues so that the cache warms.

### 4.3 State and locking

- **Inode table** (RAM, authoritative for FUSE): `HashMap<Ino, Node>` plus per-directory `BTreeMap<Name, Ino>` child maps.
  Each node holds its local state, its last known remote state and its generations (§8.2). The whole table is under one
  `RwLock`, held for microseconds and never across `.await`; it is loaded completely from the store at mount. Sharding
  is an M4 option if profiling shows contention.
- **Per-inode staging lock**: the staging file and its sidecar (§7.1) of a changed file have their own mutex, so file I/O
  never holds the table lock. Order: a staging lock may be taken before the table lock, never while holding it, and
  before the map of staged inodes, never while holding that. A write or truncation changes the staging and the node
  (size, `content_gen`) under the staging lock, and an upload commit compares the generations and takes a clean
  staging out of the map under it (§7.4 7–8); otherwise a write between the two is lost (fsx, M2.9). Access
  rules (§11) have their own lock, taken before the table lock. No lock is held across `.await` or while calling the
  Notifier. A staging file is closed only after the map of staged inodes is unlocked: on btrfs, closing a file that
  was truncated to 0 bytes flushes it and waits for a commit in progress.
- **SQLite** (WAL): one writer thread owns the write connection. Mutations are sent as batches, with group commit: everything
  queued when the writer wakes goes into one transaction. The writer never waits for the disk: it commits with
  `synchronous=NORMAL` and automatic checkpoints are off. Callers that need durability (fsync, fsyncdir, `access set`,
  inode reservations, staging removal) are acknowledged by a sync thread once it has fsync'd the WAL after their
  commit, which is what `synchronous=FULL` does. The same thread runs a `PASSIVE` checkpoint once no commit has come for
  1 s, or after 20 000 writes under steady load: on btrfs a checkpoint slows every file creation in the file system for
  as long as it runs (600 ms measured after a burst). A passive checkpoint never catches up with a writer that keeps
  committing, so under steady load the WAL never starts over (14 GB in a few hours of fsx, M2.9). When 64 MiB or more
  is left in it after a passive checkpoint, the writer runs a `TRUNCATE` checkpoint before its next batch, which
  copies the rest and empties the file. It runs on the writer's connection because on another one it made writes in
  the writer's transactions fail with `SQLITE_BUSY` (lost rows); between two batches a failed checkpoint loses
  nothing. Readers use a separate read connection.
- **Replies wait for the commit** (D032): a successful reply to a request that changes state (create, mkdir, unlink,
  rmdir, rename, setattr, open, write, flush, release) is sent only once the batch holding the rows it queued has
  committed (`synchronous=NORMAL`, no fsync). The fuser thread does not wait: the Reply moves to the writer, which sends
  it after the commit. A request that queued nothing, or whose rows are already committed, is answered at once; on the
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
- **Network guard:** handlers marked hot run inside a `HotPath` scope. Any `Backend` call within that scope increments
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
| Listing prefetch | On a user readdir, background-list subdirectories to depth 2 | 32 listings per trigger, prefetch priority |
| Small-file prefetch | Files ≤ 256 KiB in a just-listed directory are fetched into the block cache | 16 MiB per directory |
| Sequential readahead | A per-handle detector prefetches the next 2–4 blocks (8–16 MiB) | readahead permits |
| Pins (`pdmount pin <path>`) | Every file under the path is downloaded and kept, and new revisions are fetched on UpdateContent | Reported separately from the cache cap |

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
| `FOPEN_KEEP_CACHE` | when the inode's revision equals the one last served to the kernel (`fuser/ll/flags/fopen_flags.rs:12`) | Page cache survives reopen |
| `FOPEN_CACHE_DIR` | on listed directories (`:16`) | Directory pages are cached |
| `FOPEN_DIRECT_IO` | never | git mmaps packs and the index |
| `FUSE_WRITEBACK_CACHE` | **off in v1** (`:42`) | See §5.4.1 |

Capabilities are added with `KernelConfig::add_capabilities` (`fuser/lib.rs:342`). An unsupported flag is logged and
the mount continues.

#### 5.4.1 Why the writeback cache is off in v1

With writeback cache, the kernel owns `size`/`mtime`, sends its own mtime setattrs on flush (interfering with `rsync -t`), turns `O_WRONLY`
into `O_RDWR` (forcing block materialization for page reads), and holds dirty pages that complicate remote invalidation. Throughput
is upload-bound anyway. M4 evaluates it behind `kernel.writeback_cache = true`. It becomes the default only if the full rsync/git/access/multi-device
suite passes with it.

### 5.5 Coherence without polling the kernel

Each remote change that is applied to the store produces invalidations (§9.5): `inval_entry` for names, `inval_inode` for data
and attributes, and `delete` for removals (which also notifies inotify watchers). Local changes go through the kernel and need no
invalidation, except access-rule changes (§11.4).

### 5.6 fsync semantics

| Mode | Meaning | Use |
|---|---|---|
| `local` (default) | `fdatasync(staging)` plus a durable store commit of the inode's generation and journal entry, then reply. Data survives a crash or power loss and is guaranteed to upload later | git fsyncs refs and packs on every commit. A remote round trip per fsync would make each commit cost several network RTTs |
| `remote` (global, or per glob via `fsync.remote_paths`) | Block until the generation captured at fsync time is uploaded and committed. Upload errors are returned (ENOSPC, EIO) | Workflows that need "on the server when fsync returns" |

- `fsyncdir` commits the store.
- A crash of the daemon (`kill -9`, panic, OOM kill) loses nothing a request was answered for: replies wait for the store
  commit (§4.3), and writes to staging files are in the page cache. Only a power loss or kernel crash is limited to what
  was fsync'd.
- `flush` (close) never blocks on uploads. It returns any error deferred from a previous upload of that inode (§7.8).
- `pdmount sync [<path>]` is the explicit remote barrier. It returns when every pending item under the path is uploaded.

### 5.7 Latency budgets (SLOs)

These are measured in the daemon from request receipt to reply, on SSD, warm process. Kernel cache hits never reach the daemon.

| Operation | p99 target |
|---|---|
| getattr, lookup, readdir(plus) of a listed dir | < 1 ms |
| create, mkdir, unlink, rmdir, rename, setattr | < 2 ms |
| write into a materialized range | < 1 ms per request |
| read hit, RAM / disk (including at-rest decrypt) | < 1 ms / < 5 ms |
| fsync (`local`) | < 10 ms |
| first byte, small file, prefetched | 0 network RTT |
| first byte, small file, not prefetched | 1 API RTT (block map) + 1 storage RTT |
| first listing of a directory | ⌈children / page size⌉ API RTTs, once |

### 5.8 Instrumentation

- HDR histograms per FUSE operation and per Backend call; counters for cache hit/miss/evict, queue depth and oldest age, and
  `hot_path_network_calls`.
- `pdmount stats` prints them over the control socket. An optional Prometheus textfile is written every 15 s.
- Tracing spans per FUSE request id, with fields for ino, op, and duration. Slow requests (over 50 ms on the hot path) are logged at WARN.
- A slow-path request answered EIO is logged at WARN with its operation and cause, since EIO tells the caller nothing.
  Other failures are logged at DEBUG, except the expected ENOENT, EEXIST and ENOTEMPTY, which are not logged.

---

## 6. Read path

### 6.1 Block map

- Proton stores content in plaintext blocks of 4 MiB (`sdk/node/revision.rs:34`), encrypted per block. Offsets come from
  `Common.BlockSizes` in the revision XAttr (`sdk/node/file/download.rs:165-170`). If that field is absent, offsets assume a uniform 4 MiB size and are
  validated against the decrypted lengths.
- `BlockMap` = content key plus a list of `{index, url, token, sha256, plain_size}` (P1 pagination, P3 public type). It is kept in RAM
  with a 10 min TTL, because URLs and tokens expire. It is refreshed when a block download reports an expired target (`sdk/error.rs:38-40`).
- Sizes and hashes per revision are persisted in `revision_blocks`; URLs are not.

### 6.2 Cache layers

| Layer | Key | Content | Limit | Eviction |
|---|---|---|---|---|
| RAM LRU | `(link, rev, idx)` | decrypted plaintext blocks | 256 MiB | LRU |
| Disk | `(link, rev, idx)` → `cache/blocks/xx/<blake3(key)>` | nonce ‖ XChaCha20-Poly1305 (key = `blake3::derive_key("pdmount block cache v1", cache master key)`, AAD = block key) | `cache.max_bytes` (10 GiB), 90 %/80 % watermarks | LRU by `last_access` (batched updates every 60 s) |

Revisions are immutable, so cache entries never need invalidation, only eviction.

**Never evicted:**
- Blocks of pinned paths.
- Base-revision blocks of dirty inodes whose staging is not fully materialized.
- Blocks referenced by an open handle's current read window (soft pin).

### 6.3 Integrity

Each fetched block is checked against `BlockDto.hash_digest`, and the manifest signature is verified once per block map (P2). On mismatch:
EIO, the block is not cached, and the event is logged at ERROR. Disk-cache entries that fail AEAD authentication are deleted and re-fetched
transparently. Local corruption is treated as a cache miss, never as a fatal error.

### 6.4 Scheduling

- **In-flight dedup:** a `HashMap<BlockKey, Shared<oneshot>>` means readers, readahead, prefetch, and materialization share one download.
- **Priority classes** with separate permits, in order: interactive read, materialization, upload, readahead, prefetch.
- **Transfer limits (D033), kept slightly below the official SDKs:**
  - Downloads: at most 5 blocks in flight over all classes. The C# SDK allows 6 block transfers per direction, shared by
    all files (`cs/client/cs/src/Proton.Drive.Sdk/ProtonDriveClient.cs:28`, `Nodes/TransferQueue.cs:39-41`); the JS
    SDK 5 files (`download/queue.ts:17`) with up to 10 blocks each (`download/fileDownloader.ts:22`). Readahead (2) and
    prefetch (1) together leave interactive reads at least 2.
  - Uploads: `upload.concurrency` (4) jobs prepare at once, but the SDK sends one file at a time, because PdMount does
    not set P7's `max_concurrent_file_transfers` (default 1), with up to 6 blocks in flight (the community SDK's fixed
    default, the same as C#'s). The official limits are 5 files with 5 blocks each and 40 MiB in total (JS,
    `upload/queue.ts:11,20`, `upload/streamUploader.ts:69`) and 6 files with 6 blocks in total (C#). When P7 is used
    (M4), files stay below 5 and blocks below 6.
- **Seeding:** after a successful upload, the snapshot is split into blocks and inserted under the new revision. When git reads a pack it
  just wrote, nothing is downloaded.

### 6.5 Reads of dirty inodes

Materialized ranges come from staging. Unmaterialized ranges come from the base revision through the cache. Reads past the staging size
return EOF.

---

## 7. Write path

### 7.1 Staging

- One sparse file per inode: `~/.local/share/pdmount/<profile>/staging/<ino>`, directory mode 0700. Its length is always
  the file's size.
- A sidecar `staging/<ino>.state` beside it (JSON, replaced atomically) holds:

  | Field | Meaning |
  |---|---|
  | `base_rev` | Remote revision the edits derive from (the upload precondition); NULL for new files |
  | `base` | The revision whose content is still partly used, and how many of its bytes (truncation cuts it) |
  | `materialized` | The 4 MiB blocks of the base already copied into staging |

  The sidecar is replaced before a write that depends on it is acknowledged, so data and bookkeeping agree after a
  crash of the daemon. Both reach the disk with `fsync` (§5.6). The `staging` table of §13.2 is not used.
- The generations live in the inode row (§13.2):

  | Field | Meaning |
  |---|---|
  | `content_gen`, `meta_gen` | Generation counters (u64) |
  | `uploaded_content_gen`, `uploaded_meta_gen` | Generations last confirmed uploaded |

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
5. Conflict pre-check: `get_nodes([link])`. If `active_rev != base_rev`, run the conflict procedure (§10.4). M2 relies on the
   server precondition instead: a `RevisionConflict` whose current revision has the snapshot's SHA-1 is adopted (an
   upload that reached the server before a crash), any other parks the item until the conflict engine (M3).
6. Upload:
   - Existing node: `upload_revision(link, base_rev, snap, meta)` (`get_file_revision_uploader`, `sdk/client.rs:718`).
   - New node: `upload_new(parent_link, name, snap, meta)` (`get_file_uploader`, `sdk/client.rs:687`).
   - In both cases, `meta` = `{mtime, additional_metadata: [("pdmount.mode", "0644")]}` (F10).
7. In one store transaction:
   - Set `base_rev = remote_rev = new_rev`.
   - Insert the new revision into `own_revisions`.
   - Set `uploaded_*_gen = snap_*gen`.
   - **Mark clean only if `content_gen == snap_gen` and `meta_gen == snap_meta_gen`.** Otherwise the inode stays dirty and the job is
     re-queued on the new base.
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

- File: `<mountroot>/.pdignore` (synced like any file) plus `~/.config/pdmount/<profile>.pdignore` (local). Gitignore syntax;
  matcher adapted from `pdcli/pdignore.rs`. **The default list is empty.**
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
- Parked jobs show in `pdmount status` (count) and, with paths and reasons, in the `unmount` output; `pdmount queue list`
  and `queue retry` follow with the queue tooling (M4). Parked state is kept in RAM: a new local change of the inode, or the
  next mount, retries it.

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
  `link_id` is known. Waiters use per-inode notify; no remote ids are captured at enqueue time.
- **Listings are merged, not copied:** a node follows its remote placement and attributes only while it has no pending
  local change of that kind; local nodes the server does not know are kept; a node removed remotely while something below
  it changed locally is kept and created again (M3 resurrects only the minimal chain, §10).
- **Name reservation:** before creating or moving a node to `(P, N)` remotely, any remote occupant that the local state has moved or
  deleted is processed first. Cycles (a ↔ b swaps) are broken through a temporary `.pdmount-swap-<ino>` name.
- **Backoff and parking:** transient errors back off exponentially (1 s → 5 min with jitter). Permanent errors (typed, P8) park
  the item with a reason, at once for quota, size, permission and unsupported errors, after 5 attempts for the others.
  Nothing is retried in a tight loop, and parked items never grow without bound (one entry per inode).
- **Where the journal lives:** the entries (schedule, attempts, parked reasons) are in RAM; what is pending is derived from
  the inode rows at mount (new nodes, tombstones, generations, placements that differ from the remote ones). The `journal`
  table of §13.2 is not used.
- **Stale plans:** every local change bumps the entry's generation; a plan made from an older generation does not defer,
  block or remove the entry, so a change made while the executor plans is never dropped.
- **Draining:** `unmount` (`--wait` is the default) makes everything eligible at once and waits up to 10 minutes for the
  queue to empty; what remains continues at the next mount. `reset --keep-login` refuses while changes are pending.

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

### 8.4 unlink and rmdir

- `unlink` removes the name. If the inode is open, it becomes an **orphan** (`parent_ino = NULL`). Staging and cache pins are kept until the
  last release and `nlookup == 0` (forget); meanwhile `fstat` reports `nlink` 0. A remote node gets a Trash work item; the per-node result map is checked
  (`sdk/client.rs:1053`). A never-uploaded file cancels its pending work.
- `rmdir` returns ENOTEMPTY if any child exists, including pending and local-only children. An unlisted directory is listed first, so a directory
  with unknown remote children is never trashed blindly.
- Deletion is always to the Proton trash (recoverable). Nothing in v1 deletes permanently.

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
| `getxattr` / `listxattr` | ENODATA, except read-only `user.pdmount.{state,link_id,rev,conflict,access}` | Introspection |
| `fallocate` | EOPNOTSUPP | glibc `posix_fallocate` falls back |
| `copy_file_range` | ENOSYS (fuser default) | The kernel falls back to a generic copy |
| locks (`getlk`/`setlk`, flock) | Not implemented; the kernel provides local locking | Not shared across devices |

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

Errors never end the loop. After a failed poll the next one waits `poll_interval` times 1, 1, 2, 3, 5, 8, 13 (then 13 again,
6.5 min at 30 s) plus up to 1 s of jitter, as the official event manager does (`cs/client/js/src/internal/events/eventManager.ts:4,132`);
a 429's `Retry-After` is honoured when longer. A successful poll resets the sequence. After 3 consecutive connection
failures, the shared state becomes `offline` (§9.6).

### 9.3 Applying events

Events carry only ids (F12). Applying a page (D034):

1. Keep the events whose link the tree knows, or whose parent is a listed directory. Others are dropped: a later listing
   fetches fresh state.
2. Resolve those links with `get_nodes`, fresh from the API, 50 links per request as in the official SDKs (F14, D035, P13).
3. A node whose parent, name and (for files) active revision equal the tree's remote state is an **echo** (§7.5) and needs
   nothing.
4. For every other node, list again the listed directories it is in now and was in before (its local parent and the directory
   of its known remote parent). A directory a node left is listed after the one it moved into, so a node moved between two
   listed directories keeps its inode and its listed subtree; a cycle (two nodes swapped) is broken in event order.

So remote changes take the same path as listings (`Tree::apply_listing_at`, §8.2), whose rules the property tests check:

| Change (event kind on the real API, M2.10) | Clean inode | Dirty / pending inode (until M3's conflict engine) |
|---|---|---|
| Create (Create) | Inserted. `inval_entry(parent, name)` drops any cached negative dentry | A new remote node with a locally used name is not shown (§10.6 in M3) |
| Rename, move (UpdateContent) | Applied. `inval_entry` on the old and new names, `inval_inode` on the directories | Local namespace intent wins; the remote state becomes the new remote base |
| New revision (UpdateMetadata) | Rev, size, mtime and mode updated, the content epoch bumped (no `FOPEN_KEEP_CACHE` at the next open), `inval_inode` | Local content kept; the upload's base-revision precondition parks the conflict (§10.4) |
| Trash (UpdateContent) | Subtree removed, `notifier.delete(parent, ino, name)` (`fuser/notify.rs:102`) | Kept and uploaded again (resurrection, §10.5) |

### 9.4 Refresh

When `refresh = true`, the engine keeps serving the current state. It takes a fresh `latest_event_id` and follows the event
stream from it at once, while a background task lists every listed directory again (one at a time, paced, D027) through the same
rules as §9.3. Only when that pass succeeds is a cursor stored again, so a crash during a refresh finds the old cursor and the
next mount refreshes again. A refresh asked for while one runs makes it run once more, because listings made before the newer
cursor may have missed changes. A failed pass is retried after 30 s.

### 9.5 Kernel invalidation queue

- One task owns a clone of `fuser::Notifier` (`Clone`, `fuser/notify.rs:52-53`). It receives `{InvalEntry, InvalInode, Delete}`
  messages from the store writer for every change that did **not** originate from a kernel request.
- ENOENT from the kernel (nothing cached) is ignored.
- The task never runs on a FUSE thread and never holds core locks while calling into the kernel.

### 9.6 Offline mode

| Aspect | Behaviour |
|---|---|
| Entered | 3 consecutive connection failures (sync engine or executor) |
| Left | A successful probe |
| Cached reads | Served |
| Uncached reads | Fail fast with EIO after `offline_read_timeout` (2 s) |
| Metadata of listed directories | Served |
| First listing of an unlisted directory | EIO |
| Writes and namespace operations | Accepted and queued |
| Visibility | `pdmount status` shows `offline` and the queue depth |

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

### 10.4 Edit/edit detection, in three layers

1. **Event:** an UpdateContent event for a dirty inode sets `remote_newer` and sends a notification. Local content is untouched.
2. **Pre-check:** before upload, `get_nodes([link])`. If `active_rev != base_rev` → conflict.
3. **Commit:** `upload_revision` sends `CurrentRevisionID = base_rev` (F11). A rejection is mapped to
   `RevisionConflict` (P11) → conflict. As a post-check, the revision list must show `base_rev` as the predecessor of our revision. If a
   foreign revision was interleaved, its content is preserved as a conflict copy.

Exactly when the server enforces `CurrentRevisionID` and `OriginalHash` is **unverified** (TODO.md Q1, Q6). It is an explicit M3
task: it is tested against the real API, and the layers above remain sufficient if the server does not enforce them.

### 10.5 Policy: keep both; the first committer keeps the name

| Situation | Resolution |
|---|---|
| Edit/edit | The server version stays at the name. Local content is uploaded as a new file with the conflict name. Locally, the dirty inode is renamed to the conflict name (open fds follow it), and the server version appears at the original name as a new inode |
| Remote delete, local edit | **Resurrect**: upload the local content as a new file at the same path, recreating missing ancestors with new links (conflict name if the path is now occupied) |
| Local delete, remote edit | Before trashing, check `active_rev == base_rev`. If not, **do not trash**: the newer remote revision reappears locally and a `LocalDeleteRemoteEdit` record is written. Trash has no server precondition, so a small window remains. The trash is recoverable, which mitigates it |
| Remote delete, local clean rename | The delete wins (content is in the Proton trash). Recorded |
| Rename/rename (same node, different targets) | `OriginalHash` precondition. On rejection the first committer wins: the inode moves to the remote name, and a `RenameRename` record is written |
| Move cycles rejected by the server | Keep the server's placement, record, log |
| Remote folder trashed, local pending descendants | Resurrect a minimal chain of folders containing only the pending items. Clean descendants are removed locally |
| Local folder delete, remote has unknown/newer children | Re-enumerate the folder uncached before trashing. Trash only the children known and unchanged, and keep the folder with the rest. Recorded |

### 10.6 Create/create

- **Files:** the server's `AlreadyExists` (2500) on create makes the local file the conflict copy. The remote file appears at the name.
- **Folders:** adopt the remote folder (bind the local inode to its `link_id`) and merge children recursively. The same rules apply per child.

### 10.7 Conflict names

```
<stem> (conflict <device> <YYYY-MM-DD HH-MM-SS UTC>)<.ext>
e.g.  report (conflict laptop 2026-10-19 14-03-27 UTC).odt
```

- `<device>` is the device whose content goes into the copy (`device.name`, or `remote` if unknown).
- The extension is split at the last dot, except for leading-dot names (`.bashrc` → `.bashrc (conflict …)`).
- On a collision, ` 2`, ` 3`, … is appended inside the parentheses. The stem is truncated at a UTF-8 boundary to keep the name ≤ 255 bytes.
- Names are deterministic for a fixed clock and device, which property tests rely on.

### 10.8 Surfacing

- Each conflict gets a row in the `conflicts` table: kind, ino, link, original name, conflict name, both revisions, devices, time, and state.
- CLI: `pdmount conflicts list [<path>]`, `conflicts show <id|path>`,
  `conflicts resolve keep-both|keep-local|keep-remote <id|path>`. `keep-local`/`keep-remote` trash the other copy (recoverable).
- WARN-level log; desktop notification via `org.freedesktop.Notifications` (rate-limited, one summary per 30 s); xattr
  `user.pdmount.conflict` on both files.

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
increments `meta.access_version`. Memoizing per inode is an M4 option if profiling shows the walk.

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

$ pdmount access effective /Archive/inbox/a.txt
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
- **`mount` without a session:** `mount` runs the login flow before mounting. With `--no-login`, it exits with an error instead (for
  scripts).
- **Under systemd or in the background:**
  - The daemon writes the link to the log, shows it in `pdmount status`, and sends a desktop notification carrying the link when a
    notification daemon is available.
  - The mount stays up in `needs_login` and does not exit. `pdmount login` restarts the 10 min flow on demand.

```
$ pdmount login --no-browser
PdMount is a third-party application, not officially supported by Proton.
Open this link to sign in:  https://account.proton.me/…
Code: ABCD-EFGH
Waiting for sign-in… done. Logged in as <account>.
```

### 12.3 Token lifecycle

- **Rotation:** `token_credential.subscribe_tokens_refreshed()` (`rs2/auth.rs:99`) persists every rotation immediately. A rotated refresh token that
  is lost would force a re-login.
- **Reactive refresh:** when the adapter classifies a response as 401, it calls `token_credential.get_refreshed_access_token(rejected)`
  (`rs2/auth.rs:144`, single-flight; `token_credential` is public at `rs2/session.rs:37`) and retries once. Clones share token state
  (`rs2/auth.rs:22-73`), so the drive client never needs rebuilding.
- **Proactive refresh:** an auth task owning the session (`&mut`, behind a tokio Mutex) calls `ensure_authenticated()` (`rs2/session.rs:503`)
  every 30 min.
- **Refresh failure** (P9 surfaces it) moves the daemon to `needs_login`:
  - local reads of cached data and all local writes continue;
  - network-bound requests return EIO;
  - the link presentation of §12.2 starts.

### 12.4 Secrets at rest

- The SDK secret and entity cache is `EncryptedCacheRepository::new(SqliteCacheRepository::open_file(..), key)`
  (`sdk/cache/encrypted.rs:57`, `sdk/cache/sqlite.rs:52`), with a random 32-byte key in the keyring. The secret cache must persist,
  or keys cannot be decrypted after a resume.
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
    The SDK secret cache goes with them, so in force mode the session and the cache key must both be in files.
  - **Daemon:** `mount` reads the stored session in the foreground process before it starts the daemon, so an unlock
    dialog appears while the user waits at the terminal. The daemon gets the same flag. It stores tokens only when they
    have changed (§12.3).
  - **Tests:** tests use force mode, except the keyring tests (`scripts/repro/keyring-d024.sh`).
- Staging is plaintext with mode 0700 (§17).
- The SDK logs the account passphrase at debug level (`rs2/secret.rs:63`, `:67`). PdMount filters `proton_sdk_rs2::secret` to
  WARN in every log configuration.

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
| `~/.local/share/pdmount/<profile>/sdk-secrets.db` | SDK secret cache (encrypted). Losing it forces a new login, so it is not in the cache directory |
| `~/.local/share/pdmount/<profile>/credentials.json`, `cache.key` | session and cache key in the file modes of §12.4 (0600) |
| `~/.local/share/pdmount/<profile>/device-id` | device identity (§10.2). Stays a file so `reset --keep-login` keeps it |
| `~/.cache/pdmount/<profile>/sdk-entities.db` | SDK entity cache (encrypted, rebuildable) |
| `$XDG_RUNTIME_DIR/pdmount/<profile>.sock` | control socket (0600); `~/.local/share/pdmount/<profile>/control.sock` when the session has no runtime directory |
| `<control socket>.pid` | process id and mountpoint of the mount daemon, for `unmount --force` and for `status` after a crash |
| `$XDG_STATE_HOME/pdmount/<profile>.log` | log (rotated) |

### 13.2 Schema

```sql
meta(key TEXT PRIMARY KEY, value BLOB)
  -- root_link, ino_reserved (inode numbers up to it are handed out; reserved 16 384 at a time, durably),
  -- access_version, quota_used, quota_max. The schema version is `PRAGMA user_version`.

inodes(ino INTEGER PRIMARY KEY, kind INTEGER, parent_ino INTEGER NULL, name TEXT NULL,
       link_id TEXT UNIQUE NULL, mode INTEGER, mtime_ns INTEGER, ctime_ns INTEGER, atime_ns INTEGER,
       size INTEGER, listed INTEGER, local_only INTEGER, deleted INTEGER, nlookup_persist INTEGER,
       content_gen INTEGER, meta_gen INTEGER, uploaded_content_gen INTEGER, uploaded_meta_gen INTEGER,
       state INTEGER, remote_newer INTEGER, deferred_error INTEGER NULL)
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

cache_blocks(link_id TEXT, rev TEXT, idx INTEGER, bytes INTEGER, last_access INTEGER, pin INTEGER,
             PRIMARY KEY(link_id, rev, idx))

pins(ino INTEGER PRIMARY KEY, path_at_set TEXT, created_at INTEGER)

conflicts(id INTEGER PRIMARY KEY, kind TEXT, ino INTEGER, link_id TEXT NULL, orig_name TEXT,
          conflict_name TEXT NULL, local_rev TEXT NULL, remote_rev TEXT NULL, local_device TEXT,
          remote_device TEXT NULL, created_at INTEGER, state TEXT)

event_cursor(volume TEXT PRIMARY KEY, event_id TEXT, updated_at INTEGER)

access_rules(id INTEGER PRIMARY KEY, ino INTEGER UNIQUE, link_id TEXT NULL, path_at_set TEXT,
             mode TEXT CHECK(mode IN ('ro','rw')), source TEXT CHECK(source IN ('cli','config')),
             status TEXT CHECK(status IN ('active','orphaned')), created_at INTEGER, updated_at INTEGER)
```

**Migrations:**
- Numbered SQL files, applied in one transaction at startup.
- A downgrade is refused.
- `pdmount reset --keep-login` rebuilds everything except credentials. It is refused while the journal has pending rows.

---

## 14. CLI, control socket, operations

### 14.1 Argument convention

Options come first, then verb or mode arguments, then **the path last**, with at most one positional path. `--profile <name>` is a global option
placed before the subcommand (`pdmount --profile work status`). The global options `--allow-file-secrets` and
`--force-file-secrets` (mutually exclusive) select the secret storage mode (§12.4).

### 14.2 Commands

| Command | Purpose |
|---|---|
| `login [--browser\|--no-browser]` | Web sign-in (§12.2) |
| `logout` | Remove the session and keys |
| `mount [--foreground] [--browser\|--no-browser] [--no-login] [-o ro] [--backend proton\|fake:<dir>] [<mountpoint>]` | Start the daemon and mount (mountpoint overrides config). `--backend fake:<dir>` and `pdmount fake seed\|changes\|rename\|trash` exist only in test builds (feature `fake-backend`, §15.1). M2: `--foreground` and `--backend` only; `-o ro` follows |
| `unmount [--wait\|--force] [<mountpoint>]` | `--wait` (the default): upload what is pending first, up to 10 minutes, and list parked items (§8.2). `--force`: see §14.4 |
| `status` | Online/offline/needs_login, queue depth, oldest pending, parked items, conflicts, cache use, sign-in link if pending. M2: session, quota, mount, `Queue: N pending, M parked`, inodes, cache |
| `sync [<path>]` | Remote barrier: wait until everything under `<path>` is uploaded |
| `access set ro\|rw <path>` / `access unset <path>` / `access list [<path>]` / `access effective <path>` | §11.5 |
| `pin <path>` / `unpin <path>` | Keep offline (§5.3) |
| `queue list [<path>]` / `queue show <id\|path>` / `queue retry\|park\|drop <id\|path\|all>` | Inspect and control the journal. `drop` requires `--yes` and discards local changes |
| `conflicts list [<path>]` / `conflicts show <id\|path>` / `conflicts resolve keep-both\|keep-local\|keep-remote <id\|path>` | §10.8 |
| `cache stats` / `cache gc [--target <bytes>]` / `cache wipe [<path>]` | Block cache maintenance (pinned and dirty-base blocks are never wiped) |
| `stats` | Latency histograms and counters (§5.8) |
| `reset --keep-login` | Rebuild local state (refused while the queue is non-empty) |
| `trash empty --yes` | Delete everything in the account's trash for good (Proton only; refused while the profile is mounted, since the daemon holds the session). Real-account test runs use it when storage runs low (D036) |
| `doctor` | Check `/dev/fuse`, fusermount3, `/etc/fuse.conf`, keyring (reachable, default collection locked or not, read without prompting; the effective secret storage mode), clock skew, disk space, SDK and crypto version |
| `service install\|uninstall` | systemd user unit |

### 14.3 Control socket

JSON-lines request/response over a Unix socket at `$XDG_RUNTIME_DIR/pdmount/<profile>.sock` (0600, peer uid checked). Every CLI command except `login`
(when no daemon runs), `mount`, and `doctor` talks to the daemon through it.

### 14.4 Unmount and hang safety

- No `AutoUnmount` unless `user_allow_other` is present (`fuser/session.rs:163-171`). `doctor` explains the `/etc/fuse.conf` change.
- systemd user unit: `ExecStopPost=fusermount3 -uz <mountpoint>`, `TimeoutStopSec=300`, `Restart=on-failure`.
- Shutdown on SIGTERM:
  1. Stop accepting new opens.
  2. Wait up to `shutdown_grace` (30 s) for in-flight uploads.
  3. Commit the store.
  4. Unmount.

  Pending work persists and resumes at the next mount.
- **Starting:** `pdmount mount` re-executes itself as `pdmount mount --foreground` in its own process group, with stdout and
  stderr appended to the log file (D026), and returns once the control socket answers `ping` (90 s limit). It refuses to
  start while the profile's daemon answers or while the mountpoint is a mount (a dead pdmount mount is reported as stale).
- **Unmounting:** `pdmount unmount` asks the daemon, which runs `fusermount3 -u`. A busy mount stays mounted and the error is
  reported. On SIGTERM/SIGINT the daemon unmounts lazily if needed. Refreshed session tokens are saved every 60 s and at exit.
- After `kill -9`: `pdmount unmount --force`:
  1. Runs `fusermount3 -uz`.
  2. Stops a still-running daemon (pid file, checked against `/proc/<pid>/cmdline`): SIGTERM, then SIGKILL after 10 s.
     Its exit closes `/dev/fuse` and so ends the connection.
  3. If `/sys/fs/fuse/connections/<minor>` still exists, writes to its `abort`. That file is root-owned on most systems; the
     command then prints the root command instead.

  The connection id is the minor device number from `/proc/self/mountinfo`, which never touches the dead mount. A reboot is
  never required (exit criterion of M1).
- **Copies of the mount in other mount namespaces:** a sandbox that starts while mounted (Flatpak's bubblewrap, `unshare -m`)
  copies the mount. An unmount in the daemon's namespace does not end the FUSE connection then, and the session threads
  keep running. The daemon therefore treats "gone from its own mount table" as unmounted. It waits at most 10 s for the
  session, then exits; closing `/dev/fuse` ends the connection. The copies then report ENOTCONN until their sandbox exits.
  `unmount` returns only after the daemon process is gone (pid file), and `mount` refuses to start while it still runs.

---

## 15. Testing strategy

### 15.1 Layers

| Layer | Content |
|---|---|
| Unit | Name validation, conflict naming, ignore matcher, errno mapping, block offset math (with and without `BlockSizes`), access resolution |
| Fake backend | Proton model: name uniqueness (2500), `CurrentRevisionID` and `OriginalHash` preconditions, trash, quota, an event log with `more`/`refresh`, several client handles with independent cursors (one plays the "official app"). Fault injection: latency and bandwidth profiles, 401/429/5xx, offline windows, partial batch failures, crash points. Persistable to a directory, so two daemons can share one fake (`--backend fake:<dir>`, test builds only): `snapshot.json` plus an append-only `journal.jsonl` (one line per change, written before the call returns, so a `kill -9` of the daemon loses nothing), compacted into the snapshot at 8 MiB; an `epoch` file tells other handles to reload. Growth is bounded: the last 16 revisions per file (`PDMOUNT_FAKE_KEEP_REVISIONS`), 1024 trashed nodes, 10 000 events and 50 000 changes are kept; dropped revisions delete their blobs. Changes and events keep absolute sequence numbers across trimming (`pdmount fake changes --after <n>`) |
| Property (proptest) | Random sequences of create, write, truncate, rename (all flags, over existing, swaps), unlink while open, mkdir, rmdir, setattr, access set/unset, crash/restart, and concurrent mutations from a second fake client. Invariants listed below |
| Mounted, CI | Local CI script (`scripts/ci.sh`) on the dev machine with fuse3, over the fake backend; hosted CI deferred (D023). Every scenario below. **pjdfstest subset** (`tests/pjdfstest.sh`; pjdfstest built from source at the commit named in `tests/pjdfstest.list`): open (incl. O_EXCL), rename, unlink, mkdir, rmdir, truncate, ftruncate, chmod, utimensat, with the file-type loops cut to regular files and directories; link, symlink, mknod and mkfifo give EPERM by design (§8.7, scenario N1) and are left out. The failures of each test file must equal the list's: cases that need other users (the agent is not root, and without `user_allow_other` only the mounting user reaches the mount, D020), unsupported file types, directory `nlink` 1, and ENOSPC from the quota check (§7.8) for a huge truncate. **fsx** (`tests/fsx.sh`; xfstests' `ltp/fsx.c` built standalone): two instances at once with mapped reads and writes, 256 KiB and 8 MiB files; 6 h for the M2 exit check; without `-c`, which needs root for `drop_caches`. fsx, pjdfstest and soak never run against the real account (D030) |
| Real account | Two profiles on one machine, each with its own login. Scenario scripts only, under `pdmount-test/` (D028), one run at a time, stopping at the first HTTP 429; datasets kept modest (D030). Manual; long runs only with the user's approval |
| Soak | Over the fake backend (D030). 24 h: rsync cron, git commit loop, second-client edits, network flaps (`ip link set down`), forced token expiry. Pass: zero EIO outside injected outages, zero parked jobs, stable RSS |

**Property test invariants:**
- After every step, the local view matches a POSIX model. Steps include restarts, crashes and another device's changes
  (write, create, rename, trash) made on the fake at clean points; those arrive through the event stream (§9).
- After quiescing:
  - the fake tree equals the model: names, bytes, mtime, mode;
  - the journal is empty;
  - a fresh core started from an empty database sees the same tree;
  - **no content ever acknowledged is unreachable** (it is at a name, in a conflict copy, in history, or in the trash);
  - two cores converge to identical trees.

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

Every script under `tests/scenarios/` takes `VERIFY_MODE`:

| Mode | Meaning |
|---|---|
| `live` | Verify on the same mount |
| `remount` | `unmount --wait`, `mount`, verify |
| `crash` | `kill -9` during the run with the queue non-empty, `unmount --force`, `mount`, wait for the queue to drain, verify |
| `fresh` | `reset --keep-login` (empty local state), mount, verify. Everything comes from the API |
| `second` | A second profile (separate login, separate state) mounts the same account (or the same fake) and verifies |

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
| E1 | While mounted, another client renames a directory, trashes a file and writes a new revision of a file read before | All three visible without a remount within 75 s (two poll intervals and the pacing); the old names are gone from the kernel's caches and the file reads its new content; the state survives the verify mode |

**Multi-device** (deterministic interleavings via fake injection points; selected cases repeated on two real profiles)

| ID | Interleaving | Pass condition |
|---|---|---|
| M1 | Edit/edit, caught by the pre-check | Exactly one conflict copy, named per §10.7; both contents present |
| M2 | Edit/edit, interleaved at commit | Same as M1 (commit-time layer) |
| M3 | Remote delete vs local edit | File resurrected with the local content |
| M4 | Local delete vs remote edit | Not trashed; remote version present; conflict recorded |
| M5 | Rename/rename of the same node | First committer's name wins; a record exists; no content lost |
| M6 | Remote folder delete with local pending children | Minimal chain resurrected containing only the pending items |
| M7 | Same-name create (file and folder) on two devices; official-app client edits without PdMount metadata | File: conflict copy. Folder: merged. App edits adopted without spurious conflicts |

---

## 16. Milestones

Exit criteria are functional tests. A milestone is done when its listed tests pass.

| Milestone | Scope | Exit criteria |
|---|---|---|
| **M0 Foundations** | Workspace; SDK fork with P1, P2, P4, P8; `Backend` trait and fake; local CI script that builds `gopenpgp-sys` (Go ≥ 1.22, clang, protoc); `login`/`logout`/`status`/`doctor` | `pdmount login` opens the browser **and** `login --no-browser` prints the link; both complete sign-in. `status` shows the account and quota. Fake-backend unit tests pass. `doctor` passes on the Gentoo machine |
| **M1 Read-only mount** | Inode table, store, listing, block cache (P3), readahead and prefetch, notify queue, `catch_unwind`, deadlines, unmount tooling; writes return EROFS | **R6** and **G6 read side** (clone, fsck) on data uploaded by the official client, in `live` and `fresh` modes. This confirms API size, mtime, and content. sha256 of 5 MB / 41 MB / 2 GB files equals the web download. `hot_path_network_calls == 0`. `kill -9` → `unmount --force` → remount without a reboot |
| **M2 Writes, single device, access control** | Staging, journal, §7–§8, `fsync=local`, `.pdignore`, quota errors, metadata revisions, P5, P6, P10; access rules (§11); event engine for clean inodes with refresh and echo suppression (§9.1–§9.5, §7.5; D033) | **R1–R7, G1–G7, N1, A1–A6, E1** pass in `live`, `remount`, `crash`, `fresh` on the fake (CI), and all but A5 and E1 (which need a second client) in `live`, `remount`, `fresh` on the real account. pjdfstest subset green and fsx 6 h clean, both on the fake (D030); journal property tests at 10k cases |
| **M3 Multi-device and sync robustness** | Remote changes to dirty inodes, the full conflict engine, offline mode, auth refresh and `needs_login`, P9, P11, P12; verify server precondition behaviour (§10.4) | **M1–M7** and every R/G/A scenario in `second` mode on two real profiles. Zero data loss. Remote changes visible within `sync.poll_interval` + 10 s, 40 s at the default (D037). 24 h soak on the fake passes (D030) |
| **M4 Performance and operations** | SLO instrumentation, benchmarks, P7 parallel uploads, pins, writeback-cache experiment, `cache gc`, queue tooling, systemd unit, documentation | §5.7 SLOs met. Over the fake backend with an 80 ms RTT / 20 MB/s profile: `git status` on warm git.git ≤ 2× local btrfs; untar of 5k small files ≤ 3× local (time to acknowledgement); cached sequential read ≥ 500 MB/s; `cache gc` holds the cap ±5 % |
| **M5 Upstream and crypto migration** | Upstream patches; track Proton's new crypto model; rebase the fork | Fork diff reduced to unmerged patches; migration plan documented; full matrix re-run |

---

## 17. Risks

| Risk | Impact | Mitigation |
|---|---|---|
| Proton cryptography migration (end 2026 / early 2027, `cs/README.md:23-28`) | Old clients stop interoperating | The `Backend` trait isolates crypto. Keep the fork small. `doctor` checks versions. Drain the queue before upgrading |
| Community SDK maturity (pre-1.0, single maintainer; F1–F9) | Bugs and API churn | Pinned fork, patches upstreamed, contract tests against the fake encoding protocol assumptions |
| Server precondition behaviour unverified (`CurrentRevisionID`, `OriginalHash`) | Conflict detection weaker than designed | Three detection layers (§10.4); explicit M3 verification task |
| Rate limits under git/rsync workloads (thousands of small uploads) | 429 lockouts | Debounce and coalescing, 4 upload workers, adaptive backoff honouring `Retry-After`, prefetch budgets |
| Metadata-only revisions re-upload content | Bandwidth, revision history growth | 30 s coalescing; `upload.meta_revision` policy; visible in `queue list` |
| Plaintext staging and local-only files | Local data exposure | Mode 0700; recommend full-disk encryption; block cache encrypted |
| No remote atomic rename-over when the source is already remote | Short window with the name missing remotely | Rare in practice (temps are never uploaded); logged |
| Two concurrent git writers on one working tree | Repository-level inconsistency | Documented single-writer rule (§10.9); conflict copies as a last resort |
| `AutoUnmount` requires `user_allow_other` | Stale mount after a crash | systemd `ExecStopPost`, `unmount --force`, `doctor` guidance |
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
| `TooManyChildren` (200300) | EMLINK (parked) |
| Nesting too deep (200301) | ENOSPC (parked, plus a conflict record) |
| `RateLimited` (429), `Transient` (5xx, timeouts) | Retried with backoff; EIO only when an interactive deadline expires |
| `Offline`, uncached read | EIO after `offline_read_timeout` |
| `Unauthorized` after refresh failure | `needs_login`; EIO for network-bound requests |
| `Integrity` (hash or signature mismatch) | EIO; nothing cached; ERROR log |
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
ram_bytes = "256MiB"
prefetch_small_files = "256KiB"
prefetch_dir_budget = "16MiB"

[upload]
concurrency = 4
debounce = "2s"
max_dirty_age = "10m"
meta_revision = "always"        # always | max_size:<bytes> | never

[fsync]
mode = "local"                  # local | remote
remote_paths = []               # globs that use remote fsync

[kernel]
attr_ttl = "60s"
entry_ttl = "60s"
negative_ttl = "10s"
writeback_cache = false

[sync]
poll_interval = "30s"           # event polling; at least 30s, the official clients' interval (D033)

[timeouts]
metadata = "20s"
block = "60s"
offline_read = "2s"
shutdown_grace = "30s"
```
