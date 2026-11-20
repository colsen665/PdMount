# PdMount — Design

| | |
|---|---|
| Status | Draft 1, design phase. Current status and next steps: [TODO.md](TODO.md) |
| Decisions | [docs/decisions.md](docs/decisions.md) (who decided what, and why) |
| Date | 2026-10-19 |
| Scope | Proton Drive "My Files", mounted read-write on Linux through FUSE |
| Language / libraries | Rust (edition 2024), fuser 0.18, proton-drive-sdk / proton-sdk-rs2 0.5 (patched fork on 0.5.1, M6.0.1; 0.4 before), tokio, rusqlite |

Source citations use these prefixes:

| Prefix | Location |
|---|---|
| `fuser/` | `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/fuser-0.18.0/src/` |
| `sdk/` | `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/proton-drive-sdk-0.5.1/src/` (upstream's crate, not the fork) |
| `rs2/` | `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/proton-sdk-rs2-0.5.1/src/` (upstream's crate, not the fork) |
| `pdcli/` | `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/pdcli-0.4.0/src/` |
| `cs/` | `<official SDK checkout>/` (official Proton Drive SDK, C# and JS, used as the behaviour reference; clone of `https://github.com/ProtonDriveApps/sdk` at tag `cs/v0.33.0` = `05c91159`, which contains `js/v0.23.0`; M6.0.2) |

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
| RQ8 | Scope is My Files read-write; from M5, writable shares read-write, and other shares and Photos read-only, as further roots (D078–D081); Computers out of M5's scope (D122) | §14.5 |

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

Found in 0.4.0, the version the fork started from; the evidence cites 0.5.1. Upstream 0.5.0 fixed F2 and part of F8; the
rest are fixed in the fork (`docs/sdk-patches.md`, which also lists what else upstream changed). All of them are hidden
behind the `Backend` trait.

| # | Finding | Evidence | Patch |
|---|---|---|---|
| F1 | File nodes report `storage_quota_consumption` as `claimed_size` and the revision `creation_time` as `claimed_modification_time`. The revision XAttr is never decrypted, so st_size and st_mtime are wrong | `sdk/node/mod.rs:1393-1425` | P4 |
| F2 | 0.4.0 listed only the first 10 blocks, so files over 40 MiB were truncated. Fixed upstream in 0.5.0, which pages the listing inside `create_download_state` but has no public listing and refuses an unsorted page; P1 now only provides the sorted, public listing | `sdk/node/revision.rs:233-270`, `sdk/node/download.rs:18-19` | P1 |
| F3 | Block SHA-256 is not verified; there is no manifest signature check | `sdk/node/download.rs:547-561` | P2 |
| F4 | `download_range` rebuilds revision and block state on every call | `sdk/node/file/download.rs:173-212` | P3 |
| F5 | One whole-file transfer at a time per client (`Semaphore::new(1)`) | `sdk/node/transfer.rs:19` | P7 |
| F6 | Upload returns only `NodeUid`, not the new revision | `sdk/node/file/upload.rs:134-139` | P6 |
| F7 | Errors are `anyhow` strings | `sdk/api/file.rs:163-185` | P8 |
| F8 | No 401 refresh in requests, and a failed refresh keeps the old tokens and reports success. Partly fixed upstream in 0.5.0: a failed refresh is logged, fires `refresh_token_expired` on code 10013 and no longer broadcasts the old pair, but still returns `Ok` | `rs2/auth.rs:194-220` | P9 |
| F9 | Folder mtime is ignored on create and never decoded | `sdk/node/folder.rs:121`, `:111` | P5 |
| F10 | Revision XAttr carries custom keys (flattened `additional_metadata`), so POSIX mode can be stored remotely | `sdk/client.rs:800`, `:829`; `sdk/api/attr.rs:53-56` | (P4 to read) |
| F11 | New-revision drafts send `CurrentRevisionID` (an optimistic-concurrency base), and rename/move send `OriginalHash` | `sdk/node/draft.rs:194-197`; `sdk/api/links.rs:562`, `:638` | P11 |
| F12 | Events contain only ids and type (`Delete=0, Create=1, UpdateMetadata=2, UpdateContent=3`, `link_id`, `parent_link_id`, `is_trashed`), plus `more` and `refresh` flags | `sdk/api/events.rs:143-214` | — |
| F13 | Login is only the browser session fork; `on_sign_in(url, code)` callback | `rs2/session.rs:240-250` | — |
| F14 | `get_node` and `enumerate_nodes` (one `get_node` per id, 8 at once) answer from the SDK's entity cache, which revision commits and other devices' changes do not update; `get_node_uncached` evicts the entry and fetches it again; `get_nodes_uncached` (P13) does that for many links, 50 per `get_details`. Folder listings (`NodeBatchLoader`, 50 links per `get_details`) are always fresh but do not refresh that cache. Seen on the real API in M2.10 (D035). 0.5.0's new `get_node_hierarchy` (a node and its ancestors) reads through the same cache, one `get_node` each | fork `8c7ff8b`: `client.rs:556-563`, `node/operations.rs` (`get_node_metadata`, `enumerate_nodes`), `node/mod.rs` (`NodeBatchLoader`); `sdk/client.rs:601` (`get_node_uncached`), `:716-738` (`get_node_hierarchy`) | — |
| F15 | Rename and move fetch the node's name hash fresh right before sending it as `OriginalHash`, so the server's check never fails; decoded nodes drop the hash | fork `7c92076`: `node/operations.rs` (`rename`, `move_single`) | P14 |
| F16 | Link details (`v2/volumes/{id}/links`) carry the trash time as `TrashTime`; the SDK reads `Trashed`, so every node looks untrashed | fork `7c92076`: `api/links.rs` (`LinkDto`) | P15 |

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
│   ├── pdmount-core/src/      no fuser dependency; all logic; unit + property tests
│   │   ├── engine.rs          hot/slow operations for the FUSE adapter, prefetch, refresh walk; `Engine::open` returns the invalidation receiver (§9.5)
│   │   ├── engine/            `impl Engine` by job (module map in `engine.rs`): open, accessors, background, shutdown, cache_glue, namespace, io, listing, staging, access, ignore, inos, prefetch, scan (the bulk-read guard's side); marker (pending marker, §14.4), pins (§5.3), queue (queue tooling, §8.2), surface (conflicts, `user.pdmount.*` xattrs), rights (write access a share can lose, §14.5), orphan (a gone share's queued changes, §14.5); proptests, tests
│   │   ├── events.rs          sync engine: applying event pages, echo filter, refresh (§9, D034)
│   │   ├── poller.rs          one event poller per volume for the trees on it; intervals, the volume in use (§9.2, §14.5, D110)
│   │   ├── vfs.rs             inode tree, dentries, attrs, name rules; derives the kernel invalidations (`Effects::finish`)
│   │   ├── access.rs          access rules, effective-mode resolution (§11)
│   │   ├── store/             SQLite schema (`000n.sql` migrations), single writer, group commit
│   │   ├── rows.rs            inode rows in the store (`inodes`, `remote`)
│   │   ├── cache.rs, cache/   block cache (module map in `cache.rs`): layout, read, ram, disk (encrypted), budget (profile-wide cap, D110), block_cache (facade: dedup, scheduler)
│   │   ├── pool.rs            block memory reserved at startup (§6.2)
│   │   ├── staging.rs         per-inode staging files, materialization bitmap, snapshots
│   │   ├── journal.rs         desired-state reconciler, executor, dependencies, backoff, parking
│   │   ├── conflicts.rs       conflict naming and the conflict registry (§10.7, §10.8)
│   │   ├── pdignore.rs        `.pdignore` (local-only rules)
│   │   ├── device.rs          device identity
│   │   ├── gate.rs            pacing of speculative API work: prefetch, refresh (§5.3, D027)
│   │   ├── scan.rs            the bulk-read guard: scan detection, the scan reads' turns, their cache share (§6.4, D111)
│   │   ├── slow.rs            reads a user waits on for 5 s, reported per file for the slow-download notification (§6.4, D137)
│   │   ├── health.rs          online / offline / needs_login, the `Monitored` backend wrapper (§9.6)
│   │   ├── singleflight.rs    shared in-flight work (listings, block maps, blocks)
│   │   ├── metrics.rs         histograms, counters, hot-path network guard
│   │   ├── lock_order.rs      ranked locks, checked in debug builds (module doc of engine.rs)
│   │   ├── config.rs, paths.rs, units.rs   config file, profile paths (§13), duration and size units
│   │   └── errors.rs          CoreError → errno
│   ├── pdmount-backend/src/
│   │   ├── lib.rs             trait Backend, RemoteNode, BlockInfo, BackendError
│   │   ├── proton.rs          SDK adapter: block-map memo, error classification, one token refresh and retry on 401
│   │   └── fake.rs            in-memory Proton model: clones share a drive, events, faults, latency
│   ├── pdmount-fuse/src/      `lib.rs`: fuser::Filesystem impl → core; catch_unwind; reply dispatch. `router.rs`: the inode router (§14.5). `mount.rs`: `mount()`, `mount_trees()` and the `pdmount-notify` threads (§9.5). `convert.rs`: type conversions, reply helpers
│   └── pdmount/src/           binary
│       ├── main.rs            CLI (§14); `fake` subcommands in fake-backend builds
│       ├── daemon.rs, daemon/, mounts.rs   mount daemon, mount, unmount, reset (module map in `daemon.rs`: launch, run, backend, handler, stop, client, process, reset); mount-table lookups
│       ├── roots.rs           further roots: their trees, engines and mounts in the daemon (§14.5, D110)
│       ├── control.rs         control socket
│       ├── supervise.rs       the daemon under a service manager: readiness, double-start checks (D074, D093)
│       ├── service.rs         `service` command: user service under OpenRC or systemd (D074, D094)
│       ├── autostart.rs       `autostart` command: XDG autostart entry (D059); the service's attach entry (D102)
│       ├── bus.rs             the desktop's session bus kept for the notification client and the keyring helper; the bus chosen per service; the attach saved across restarts (D102)
│       ├── keywait.rs         `session attach`; a start before the keyring answers: stand-in backend, retries (D106)
│       ├── flush.rs           the flush at a stop (§14.4, D075)
│       ├── logind.rs          logind shutdown delay lock (D091)
│       ├── logind_stub.rs     stand-in for logind on a private bus (fake-backend builds, scenario X11)
│       ├── notify_stub.rs     stand-in for a notification server on a private bus (fake-backend builds and unit tests, scenarios X4, X11-X13, X16, X20; M6.1)
│       ├── resume.rs          warning at start when the last session left changes (D075)
│       ├── incoming.rs        pending markers of other devices: one notification per tree and pass, capped `status` lines (D076, D126)
│       ├── downloads.rs       the slow-download notification: limits, sums, the close at the wait's end (§14.4, D137)
│       ├── auth.rs, session.rs, credentials.rs, secret_cache.rs   session, sign-in, keyring and file secrets, SDK secret cache (§12)
│       ├── status.rs, stats.rs, doctor.rs, notify.rs   `status`, `stats`, `doctor`; `notify.rs`: the D-Bus notification client (D138, M6.1)
│       ├── access.rs, cache.rs, conflicts.rs, pins.rs, queue.rs, trash.rs   command clients of the control socket (`trash empty` runs without a daemon)
│       └── probe.rs           Q1/Q6 API probe (test builds)
├── tests/
│   ├── scenarios/             r*.sh, g*.sh, a*.sh, n1, e*, x*, m*, queue, ops, kill9, … (VERIFY_MODE); run-all.sh
│   ├── pjdfstest.sh           runner: the subset, expected failures from pjdfstest.list
│   ├── pjdfstest.list
│   ├── fsx.sh
│   └── soak.sh
├── scripts/
│   ├── ci.sh                  local CI
│   ├── bench/                 benchmarks: read, latency, M4 exit benchmarks
│   ├── repro/                 probes and reproductions (keyring, session end)
│   └── testdata/              the M1 dataset
└── docs/
```

**Adapted from pdcli (MIT, with attribution in file headers):**
- `pdcli/credentials.rs`: keyring storage with a 0600 file fallback
- `pdcli/pdignore.rs`: matcher only; the default list is emptied
- `pdcli/service.rs` (systemd user unit): rewritten as `service.rs`, which also supports OpenRC and runs the daemon in the foreground under the manager (D074, D093, D094); the XDG autostart entry (`autostart.rs`, D059) remains for desktop login

### 3.2 Backend trait

The core talks only to this trait, implemented by `proton::ProtonBackend` (SDK adapter) and `fake::FakeBackend`.

```rust
#[async_trait]
pub trait Backend: Send + Sync + 'static {
    async fn account(&self) -> Result<AccountInfo>;                              // username, root (cached)
    fn upload_block_size(&self) -> u64;      // plaintext block size of uploads (default 4 MiB); seeds the cache after an upload
    async fn root(&self) -> Result<RemoteNode>;                                  // the tree's root (My Files' for the main backend)
    async fn list_children(&self, folder: &LinkId) -> Result<Vec<RemoteNode>>;  // paginated inside
    async fn list_children_checked(&self, folder: &LinkId) -> Result<Listing>;  // {children, degraded}: children that do not decrypt named apart (D132); default: none
    async fn get_nodes(&self, ids: &[LinkId]) -> Result<Vec<Option<RemoteNode>>>;
    async fn blocks(&self, file: &LinkId, rev: &RevisionId) -> Result<Vec<BlockInfo>>; // {index, plain_size?}
    async fn fetch_block(&self, file: &LinkId, rev: &RevisionId, idx: u32) -> Result<Bytes>; // verified plaintext; default over fetch_block_into (probes, tests)
    async fn fetch_block_into(&self, file: &LinkId, rev: &RevisionId, idx: u32, buf: &mut [u8])
        -> Result<usize>;                    // into a pool slot (§6.2); required, every backend and wrapper implements it
    async fn upload_new(&self, parent: &LinkId, name: &str, src: UploadSource, meta: FileMeta)
        -> Result<(LinkId, RevisionId)>;
    async fn upload_revision(&self, file: &LinkId, base: &RevisionId, src: UploadSource, meta: FileMeta)
        -> Result<RevisionId>;                                                    // Err(RevisionConflict)
    async fn create_folder(&self, parent: &LinkId, name: &str, mtime: SystemTime) -> Result<LinkId>;
    async fn rename(&self, node: &LinkId, new_parent: Option<&LinkId>, new_name: &str, expect: Option<&NameHash>)
        -> Result<()>;
    async fn trash(&self, nodes: &[LinkId]) -> Result<Vec<Result<()>>>;
    async fn revisions(&self, file: &LinkId) -> Result<Vec<RevisionMeta>>;       // oldest first; default Unsupported (tests, diagnostics)
    async fn latest_event_id(&self) -> Result<EventId>;
    async fn poll_events(&self, since: &EventId) -> Result<EventPage>;          // {next, more, refresh, events}; the backend's volume, called by its poller (§9.2)
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
| `unverified` | The signatures that did not verify at decode (fork P27): parts, claimed signer and digest (§6.3, D132); `None` when all did |

`BackendError` is a typed enum: `NotFound`, `AlreadyExists`, `PermissionDenied`, `QuotaExceeded`, `FileTooLarge`, `TooManyChildren`,
`NestingTooDeep`, `RevisionConflict{current}`, `NameConflict`, `InvalidMove`, `RateLimited{retry_after}`, `Transient`, `Unauthorized`, `Integrity`,
`Offline`, `Unsupported`, `Other`.

A backend serves one tree: a volume and a root folder on it (D080, D110). `ProtonBackend::new` resolves My Files;
`ProtonBackend::tree(volume, root)` makes the backend of a further tree over the same session, client and transfer
limits, and `account().root` and `root()` name that tree's root. `FakeBackend::tree` does the same on the fake.

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
        │   └── sync engine (events) ──► invalidation channel ──► (§9.5)    │
        └────────────┬─────────────────────────┬────────────────────────────┘
                     │ Backend trait           │
        ┌────────────▼──────────┐   ┌──────────▼─────────┐
        │ proton adapter (SDK)  │   │ fake (tests)       │
        └────────────┬──────────┘   └────────────────────┘
                     ▼
              Proton Drive API
```

The channel's receiver goes to `pdmount-fuse`, whose notify thread calls `fuser::Notifier` (§9.5).

### 3.4 Data flows

- **Read (warm):** kernel `read`, then vfs, then cache RAM hit, then reply from a slice of the block's pool slot (§6.2). No disk
  access, no network.
- **Read (cold):** kernel `read`, then cache miss, then a slow-path task: `block_map` (memoized), then `fetch_block_into` a pool
  slot (§6.2), then verify, then seal it in place for the disk cache and open it again, then insert into the RAM LRU, then
  reply from the slot. Readahead schedules the next blocks.
- **Write:** kernel `write` → access check → `pwrite` to staging, bump `content_gen`, reply. Then `release`
  → debounce → journal executor → snapshot → upload → commit to store (clean only if the generation is unchanged).
- **Remote change:** sync loop → `poll_events` → resolve nodes → one store transaction (apply and advance the cursor)
  → invalidation channel → notify thread → `inval_entry`/`inval_inode`/`delete` (§9.5).

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
  table, then leaf locks that take nothing while held (journal entries, the conflict registry, `.pdignore` rules); queueing
  a store write (a short mutex and an unbounded channel send, no wait) is below all of them (the journal's `queued` rows,
  D105). A
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
- **Kernel notifications** go through a single notify thread, fed by a channel (§9.5). They are never sent from a FUSE handler thread, because invalidating
  an inode with a request in flight on the same channel can deadlock.

### 4.4 Runtime layout

```
main
 ├─ tokio multi-thread runtime (workers = num_cpus, max 8)
 │   ├─ sync engine task
 │   ├─ journal executor (upload workers = 4, namespace worker = 1 ordered)
 │   ├─ cache scheduler (block downloads: 5 in total per session; interactive 5, readahead 2, prefetch 1; D033)
 │   ├─ auth task (proactive refresh, needs_login)
 │   ├─ logind task (system bus: delay lock, PrepareForShutdown; §14.4) and, at a stop, the flush task
 │   └─ control socket server
 ├─ store writer thread and store sync thread (§4.3)
 ├─ `pdmount-notify` thread (kernel invalidations, §9.5; owned by pdmount-fuse)
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
  made while unmounted arrive as events and reach the kernel through the invalidations (§9.5). The listed tree is
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
| Sequential readahead | A per-handle detector (`readahead.rs`) prefetches the next 2 blocks (8 MiB) once the handle's reads, each starting where the one before ended, cross into the next block, and 4 (16 MiB) once they crossed two (D135). Reads within one block start nothing: a player reads the head, then jumps to the index at the end, and the kernel's 128 KiB reads are adjacent at once, so a readahead started there competed with that jump (first frame twice as late). The crossing read starts the readahead before it waits for its own block. None for scan reads (§6.4) | readahead permits |
| Pins (`pdmount pin <path>`, D060) | Every file at or below the path is downloaded and kept, and new revisions are fetched as the tree learns of them. One background task looks at the tree when woken (pin, unpin, a listing or event that changed nodes, a rename, a removal, an upload), at the start and every 5 minutes: it pins the current revision of each clean remote file, lets go of the others, lists unlisted pinned folders depth first, then fetches what is missing: a file's block list in a background turn, its blocks at prefetch priority without one (D120), bounded by the prefetch download slot (§6.4). Failures back off like the journal, without parking; it waits while offline or needing a login. A pin follows its inode; a file replacing a pinned one takes the pin over (atomic save); deleting the node deletes the pin | Not counted against `cache.max_bytes`, reported apart. `pin` is refused when what is known of the path does not fit in the free disk space less 1 GiB; the fetcher stops below 1 GiB free, with a WARN |

Background API work (listing prefetch, small-file prefetch, the event loop and refresh listings, §9) is paced (D027): one request at a time,
at most two starts per second, and none while a foreground listing or read is in flight. With further roots these count over the
session, all trees together (§6.4, "Per session"). Proton rate-limits bursts with
HTTP 429, and the SDK's retry back-off exceeds the 20 s metadata deadline, so unpaced prefetch turns into EIO for users.

How the gate works (`gate.rs`, D120): a turn is a place in one queue, held for the spacing and the request only. Work waits for
the foreground to end before it queues, and again, out of the queue, when a request came meanwhile, so a waiting guess never
holds up anything behind it. A turn holds metadata requests only: block downloads (readahead, a pin's blocks, Photos'
thumbnails, whose URL request starts in a turn) are bounded by the download permits (§6.4) instead, so no turn lasts a
download of many MiB. The event sync is not a guess: each of its steps (a poll, a `get_nodes` batch, a listing of an event
page, a fresh cursor) waits for the foreground at most 2 s (`EVENT_PATIENCE`), then takes its place in the queue and the
spacing like any turn, so remote changes arrive under a steady load in any tree, at the same pacing. Refresh listings (§9.4)
stay speculative. The refresh task is one of the engine's background tasks, so a stop ends it; it ends by itself when the
tree's root is gone or refused (a revoked share), with the refresh still owed (no cursor stored) and the tree left to the
daemon to remove.

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
  scans begun, scan reads and blocks evicted for the scan share (`scans`, `scan_reads`, `scan_evictions`, §6.4), backend
  calls, `rate_limited` (backend calls answered HTTP 429), uploads, expired deadlines, handler panics and
  `hot_path_network_calls`.
- **Gauges,** computed when read: `queue_pending` and `queue_parked` (journal entries), `queue_oldest_secs` (how long the
  oldest entry that is not parked has been queued, counted across restarts, D105), `staged_open_files` (staging descriptors open in the LRU, §7.1).
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
  validated against the decrypted lengths. The fork reads each XAttr field on its own, so one that does not decode drops
  only itself, and sorts the block sizes of revisions created before 2025-01-01 in descending order, as the JS SDK does
  for a Windows client that wrote them unsorted (P4 and P3 amended, M6.0.4.11).
- Sizes are the uploader's claim, so they are bounded before they size anything (S-5, S-6):
  - A file's size is the sum of its claimed block sizes when the revision has them (saturating); a claimed `Size` that
    differs is logged at DEBUG and ignored. Without block sizes, the claimed `Size` is used, and staging bounds it (§7.1).
    Without either the size is unknown (the fork no longer takes the encrypted storage size for it, P4 amended): the
    adapter shows the storage size, an upper bound, and logs it once per node at WARN; the last block then reads as EIO
    rather than the file showing empty.
  - A layout (`cache/layout.rs` `Layout::new`) refuses, as `Integrity` (EIO), a declared block above 4 MiB, an empty block
    before the last, and declared sizes that do not add up to the file size. A block fetch refuses an expected length
    above 4 MiB before it takes a buffer.
- **Block lists by range** (M6.0.11, D135; fork P30; `pdmount-backend/src/block_lists.rs`, shared by the adapter and the
  fake). The layout comes from the node's claimed block sizes when the revision read is the node's active one and has
  them (the SDK's node cache, no listing); otherwise from the whole listing, verified, with the sizes of the listed
  revision (P30 `listed_block_sizes`). A block read lists only its page: 20 blocks from the page's first index (the JS
  SDK's `BLOCKS_PAGE_SIZE`, `cs/client/js/src/internal/download/apiService.ts:7`; C# and the fork use 10), each listed
  block with its storage target, its hash and the content key. A target is used for 10 min after its listing (the
  storage token lives 30 min, P2); an expired one is listed again with its page alone, under that page's lock, so the
  file's other downloads go on. A page that answers fewer blocks than asked for, not past the end, is followed by a
  listing from the block itself, so a server that caps the page size still finds every block.
- **The manifest still comes first** (S-4, P17): the listed hashes are authenticated by the revision's manifest
  signature, which covers every block, so the first read of a revision lists the other pages in order, one at a time,
  beside its own download, and gives its bytes only once the manifest verified and lists the hash the block was
  checked with. The listing ends at the claimed number of blocks (without one, at a short page; a listing cut short
  fails the manifest). The verified hashes stay while the adapter keeps the revision (256 revisions, idle ones dropped
  first), so a page listed again after its 10 min needs no new verification: its hashes must equal them. A large
  file's first read thus waits for max(its page and download, the whole listing), where it waited for the whole
  listing and then the download: 9 pages for a 650 MiB video instead of 17, the download beside them. A manifest that
  does not verify fails the read (`Integrity`, EIO) and is listed again at the next read, as before.
- A block download that meets an expired target (404, `sdk/error.rs:38-40`) refreshes that block's target itself,
  through a one-block listing (P2). A block the listing no longer has, or whose fresh target is missing as well, is an
  integrity error (§6.3). The official clients also refresh one block's target per 404 (M6.0.9, parity R6).
- Sizes per revision are persisted in `revision_blocks`; hashes and URLs are not.

### 6.2 Cache layers

| Layer | Key | Content | Limit | Eviction |
|---|---|---|---|---|
| RAM LRU | `(link, rev, idx)` | decrypted plaintext blocks, in block pool slots | 32 MiB (`cache.ram_bytes`, at least 16 MiB), reserved at startup | LRU per pool class, when a slot is needed |
| Disk | `(link, rev, idx)` → `cache/blocks/xx/<blake3(key)>` | nonce ‖ XChaCha20-Poly1305 (key = `blake3::derive_key("pdmount block cache v1", cache master key)`, AAD = block key) | `cache.max_bytes` (10 GiB), 90 %/80 % watermarks on the unpinned bytes, of all trees together (§14.5) | LRU by `last_access`, kept ordered in memory; RAM hits count too (stored in batches every 60 s) |

Revisions are immutable, so cache entries never need invalidation, only eviction.

The disk layer can get its key after the start (D106): a daemon that starts before the keyring answers opens the index
(`cache_blocks`) without a cipher. Eviction, pins and the `cache` commands work; reads miss (never "corrupt", so nothing is
deleted), and downloads go to RAM only, nothing is sealed or seeded. `BlockCache::unlock` sets the key once.

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
hashes is verified once per block map, when it is built (P17); a listed block without a hash fails it too. A block whose hash
does not match is fetched once more inside the SDK (a bit flip on the way, as the JS SDK retries it); a block still missing
after its target was refreshed is an integrity error too ("File contents are incomplete", C#'s `DataIntegrityException`;
P2 amended, M6.0.4.12). The fork retries a failed connect, a timeout and a retryable status itself, but not a
connection that breaks while the block's body is read; the adapter fetches such a block once more, as the JS SDK
retries any failed block once, and a second cut is `Transient` (EIO for the read; M6.0.9, parity R6). A hash mismatch
is not retried by the adapter again. On mismatch: EIO, the
block (or, for the manifest, every block of that revision) is not cached, and the event is logged at ERROR. Disk-cache entries that fail AEAD authentication are deleted and re-fetched
transparently. Local corruption is treated as a cache miss, never as a fatal error.

Node metadata signatures (the name's, the passphrase's, a folder's hash key's and a file's content key's) are verified by
the SDK when a node is decoded (fork P27); a failure is kept on the node as an unverified author, not an error (D132
amended by the user, M6.0.6; D134 for the choices it left open):

- *Found:* the adapter reports a node's failed parts with the node (`RemoteNode::unverified`): `name`, `key` (the
  passphrase, or a folder's hash key: the key a folder's content is encrypted to) and `content key`, the signer each
  claims, and a digest, one token per failed part: SHA-256 over the part, its claimed signer and what PdMount sees of what
  the signature covers (the decrypted name for the name; the link for the key and the content key, which do not change
  for a node's life; the SDK does not give the signed packets). It logs such a node at WARN once per node and process
  (at most 10 000 remembered). Whenever the tree applies a decoded node (a listing, an event's node, a fetch after a
  change) its remote state carries the failures; the tree keeps them in its store's `signatures` table next to the
  node's `remote` row (§13.2), with when it was first seen, and drops the row when the node verifies again or its remote
  row goes. A remote state the executor builds from its own change keeps the failures it had until the next decode. A
  node is checked only when it is decoded: one in a folder never listed on this device, or unchanged since its listing,
  is not checked again until a refresh (§9.4) or `signatures check` lists its folder. The Photos tree does the same with
  the photos whose names it fetched.
- *Mode per root:* `signatures = "refuse" | "warn"` (Appendix B): the profile's key is My Files', each `[[roots]]` entry
  has its own; `refuse` by default for My Files, Photos and a test tree on the user's own volume, `warn` for shared roots
  and a test tree on another user's volume. In `refuse` mode a found node is listed under its name and `stat` works,
  but opening it (files and folders), renaming, moving and deleting it, replacing it by a rename, and changing its mode,
  size or mtime answer EACCES (Appendix A), until it is accepted; nothing is created in, moved into or renamed within a
  folder whose `key` part failed (the new name and content would be encrypted to that key). A node whose name alone
  failed takes new entries. `warn` only records and announces it. Photos is read-only: there only the open is refused.
- *Accept:* `pdmount signatures accept <id>` (§14.2) stores the node's digest as accepted on this device and profile; the
  node is handled as verified while each of its failed parts' tokens is among the accepted ones. A part that verifies
  later needs nothing; a new part, another claimed signer or (for the name) another name asks again. The id is the first
  8 hex digits of the BLAKE3 hash of the node's link; an id that names two nodes is refused.
- *Reported:* `pdmount signatures list [--json]` (id, root, state, path with an unverified name marked, failed parts,
  claimed signer, first seen); a desktop notification per tree at most every 10 minutes with the count of newly found
  nodes and the command, never names (unverified remote text); a `Signatures:` line in `status` for My Files and each
  further tree when there are any; an INFO log line per batch found besides the adapter's WARN.
- *`pdmount signatures check [<path>]`:* lists every folder under the path in My Files, or every folder of every tree
  without one, from the server through the tree (D054: a fresh listing), one listing per background turn as a refresh
  (§9.4, D027), and counts the folders listed, the nodes decoded and the unverified ones; the folders it lists stay
  listed. In Photos it reads every photo's node again, a batch per turn. It runs in the daemon; the command shows its
  progress every 10 s and a summary, exit 1 when a folder could not be listed. On the real account it counts as a
  long-running test (D030).

Every signature (manifests, node metadata, memberships, address key tokens) is verified at server time, the time the
SDK dates keys and signatures with, not at the device clock (fork P29, D133): on a device whose clock is slow, what was
signed in its last seconds verifies instead of failing (EIO for a manifest, an unverified author for metadata).
Server time is the device clock plus the SDK's offset, one per process, taken only from the answers of the Drive API
host (its `x-pm-date`, else `Date`; not block storage or public links). The first offset after a start is adopted only
within a day of the device clock, else it is refused with a WARN in the log and the device clock is used; a later
change of more than a minute is adopted only when a second answer confirms it, a smaller one of 2 s or more at once,
and the time never goes back by up to a minute (fork P19 as amended). `doctor`'s clock line shows the offset (§14.2).

### 6.4 Scheduling

- **In-flight dedup:** a `HashMap<BlockKey, Shared<BoxFuture<…>>>` running a spawned task means readers, readahead, prefetch, and materialization share one download.
- **Four priority classes** with separate permits: interactive (reads and foreground materialization), readahead (also materialization and base reads for uploads), prefetch, and scan (1 permit, below).
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
  - *Per session* (D110): Proton's rate limits are per session and user (`cs/README.md:40`), and a daemon's trees share
    one session and one drive client (`ProtonBackend::tree`, so the client's queues above are already shared). PdMount's
    own limits are counted the same way: one `pdmount_core::limits::SessionLimits` per daemon, made by My Files' engine
    and passed to every further tree (`EngineOptions::limits`, and `PhotosOptions::limits` for Photos), holds the download permits (5 in total, per class as
    above), the `upload.concurrency` permits (an upload takes one of its tree's and one of the session's) and the gate of
    the background turns (§5.3), so a request a user waits on in any tree holds back the background work of all. Photos'
    readahead takes the same two readahead permits as the engines' (D135). An
    engine opened without one (tests) has its own. Not shared: the FUSE slow-path limit (64, per mount, as the kernel's
    `max_background`) and each tree's journal steps other than uploads (one at a time per tree). A batch of Photos'
    thumbnails holds the session's prefetch slot while it downloads (D120).
- **Seeding:** after a successful upload, the snapshot is split into blocks and inserted under the new revision. When git reads a pack it
  just wrote, nothing is downloaded.
- **Bulk-read guard (D081, D111; `crates/pdmount-core/src/scan.rs`, `[scan]` in Appendix B).** A thumbnailer, a content-type
  sniffer or a recursive `grep` opens many files and reads the start of each; as interactive reads they would take every download
  slot, readahead and the block cache. One guard per session sees the opens of every tree (My Files and the further roots, §14.5):
  - *Detection:* only opens for reading alone count, and only those a program asked for (not the engine's own reads, not
    `O_CREAT` of an existing file). A scan begins
    when `scan.opens` (10) different files were opened within `scan.window` (5 s), counted over all trees, and ends at an open after
    which at most one file was opened within the window. A handle is classed when it is opened: one opened while no scan runs is
    interactive for good, so opening one photo after a scan stays fast.
  - *Scan reads* are the reads through a handle opened during a scan until `scan.first_bytes` (8 MiB, two blocks) were read through
    it; the rest of the file is read interactively, so a large file copied whole is held back only at its start. A scan read that
    needs the network (its block list or a block that is neither cached nor being downloaded) first takes a turn of the guard's
    gate (`gate.rs`): one at a time over the session, starts `scan.interval` (100 ms) apart, and none while a request a user waits
    on (an interactive read, a listing, a write that fetches) is in flight in any tree. A turn waits at most a third of
    `timeouts.block` (20 s), then the read goes anyway, so the guard slows a scan but never makes it time out. The download takes the
    scan permit and a download slot; scan reads do not hold back the tree's background work (§5.3) and get no readahead. An
    interactive read that joins a scan download still waiting for the scan permit raises it: the download waits for an
    interactive permit instead, so a user's read never queues behind the scan (D120).
  - *Cache share:* blocks that scan reads stored on disk are tracked by the guard, by age; above `scan.cache_share` (10 %) of
    `cache.max_bytes` the oldest of them are evicted, whichever tree holds them (pinned and protected blocks excepted). An
    interactive read of such a block takes it out of the scan blocks, and so does any download of it other than a scan read's, a
    pin of its revision, and its leaving the disk by another path (the cap, `cache gc`, a wipe, a corrupt file; D120), so the
    share counts only scan blocks on disk. The share is not kept across a restart: earlier scan blocks
    are ordinary blocks then. The RAM pool is shared as before (§6.2).
  - The guard is per session, not per process: during a scan, a file another program opens is classed as part of it.
    `scan.opens = 0` turns the guard off.
- **Slow downloads (D137; `crates/pdmount-core/src/slow.rs`).** A read a user waits on (not a scan read) that has waited
  5 s on the slow path, in any tree (an engine's `Engine::read`, Photos' `read`), reports its file to the session's
  `SlowReads` (on the guard, so one subscription sees every tree; a file is its tree's seat and its inode): a `Began`
  with the file's name as the tree shows it and its size, once per file while any of its reads still waits, and an
  `Ended` when the last of them got its bytes or failed (also when it is cancelled). The wait measured is the read's own,
  layout and blocks: a read served from the cache, or by a readahead that arrives in time, never reaches 5 s. One timer
  per slow-path read (`tokio::time::timeout` around the read's future, which goes on after it fires), none for a scan
  read and none while nothing subscribes; the hot path (`read_local`) is unchanged. Only reads through the kernel are
  measured: the engine's own reads of a file (`read_whole`, for `.pdignore`) go around the timer, as nobody waits on them.
  The daemon's notification is §14.4.

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
   rejects a mismatch at commit with `ContentSizeMismatchIntegrityException` (`sdk/node/revision.rs:740`).
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
   - Existing node: `upload_revision(link, base_rev, snap, meta)` (`get_file_revision_uploader`, `sdk/client.rs:824`).
     A `RevisionConflict` (layer 3, §10.4) fetches the node again: still at `base_rev` means another client's draft is open
     (retry later); otherwise the decision of step 5 is made with the snapshot at hand. `NotFound` → `resolve_vanished`.
   - New node: `upload_new(parent_link, name, snap, meta)` (`get_file_uploader`, `sdk/client.rs:793`).
   - In both cases, `meta` = `{mtime, additional_metadata: [("pdmount.mode", "0644")]}` (F10).
   - The SDK decrypts each encrypted block with the verification session key and compares it with the plaintext before
     the block goes up, and encrypts a failing block once more (P20). A block that fails twice ends the upload with
     `Integrity`, which the journal retries with backoff and parks after five attempts (Appendix A).
   - The SDK can upload a file or revision under 128 KiB in one request instead of draft, blocks and commit (P21), when
     the client's feature flag provider turns `DriveSmallFileUpload` on. PdMount leaves the provider at its default, so
     the flag is off: the official SDKs take the flag from the application, and its server source and the endpoints'
     conflict codes are still to be checked on the real account. On a clean conflict the SDK falls back to the draft
     path, so the errors above stay the same.
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
- An uploaded file moved into a local-only folder is not local-only (it has a remote), but nothing inside a local-only
  folder is ever planned (D121): it stays at its old remote place, and its later edits are staged and never uploaded.
  `user.pdmount.state` shows `clean` until it is edited and `pending` after, for as long; it is not in the queue, so
  `pdmount status` does not count it. When a share goes, its staged content is exported as `not-uploaded` (below).
- A local-only file renamed to a non-ignored name becomes a normal pending upload. If it is renamed over an existing remote target, it becomes a
  revision of the target (§7.6). pdcli instead trashed the original (`docs/pdcli-evaluation.md` §2 #4).
- Matchers are compiled once per `.pdignore` content generation.
- **Whose rules (D124):** in My Files, and in a further tree on the user's own volume, the rules are the profile's file
  followed by the tree root's `/.pdignore`. In a tree on another user's volume (every share, §14.5) only the profile's
  file applies: the share's `/.pdignore` is an ordinary file that syncs like any other, so its writers (the owner, any
  editor) decide nothing about what of the user's stays local-only, and a `!` line there cannot undo the user's rules
  (`EngineOptions::remote_ignore`).
- **When a share goes (D114):** its local-only files, and the staged edits of uploaded files in its local-only folders,
  are exported to `orphaned/` with its queued changes (§14.5, "Revocation with changes queued"), as `local-only` and
  `not-uploaded`; their content is only in the staging, so nothing deletes it first.

### 7.8 Quota and errors

- Local disk full: `write` returns ENOSPC directly.
- Pre-check in `write`: if pending upload bytes plus quota used (from `storage_info`, cached 60 s) exceed the maximum, return ENOSPC.
- Remote quota failure during upload (200001, 200002, 200100) parks the job and sets `deferred_error = ENOSPC` on the inode. The next
  `flush`, `fsync`, or `close` on that inode returns it, then clears it. 200003 (file size limit) gives EFBIG.
  The drafts carry the upload's size (`IntendedUploadSize`, P23: a new file's rounded down to 100 000 bytes), so the
  server can refuse at the draft, before any block goes up; the refusal is handled the same way.
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
  A step can also find a dependency (a folder's trash, §8.4): the entry records it and is planned again when that entry
  finishes a step or leaves, otherwise after a back-off by its waits in a row (1 s → 5 min; D116). Waits are not failed
  attempts, and a wait found while the entry changed, or was retried or forced, changes nothing (D121). An entry made
  only because something waits for it, and that leaves at its first look without a change or a step, wakes nothing:
  its node is as the waiting entry saw it, and may have nothing to do ever (D121). An entry that starts to wait for such
  a waiting entry makes it look again at once, since it may be what that entry waits for (D117).
- **Listings are merged, not copied:** a node follows its remote placement and attributes only while it has no pending
  local change of that kind; local nodes the server does not know are kept; a node removed remotely while something below
  it changed locally is reported as vanished and, once the server confirms it is gone, only the minimal chain of pending
  items and their folders is created again (§10.5, D040).
- **Name reservation:** before creating or moving a node to `(P, N)` remotely, any remote occupant that the local state has moved or
  deleted is processed first. Cycles are broken through a temporary `.pdmount-swap-<ino>` name in the node's remote folder
  (D117): a node with a remote place whose occupant (or, while its folder has no link yet, that folder) leads back to it,
  following for each node the folder it goes into while that has no link or else the occupant of its target name, moves
  there first. This covers a swap of two names, a rotation of three or more, and a folder created at the old name of a
  node moved into it. A cycle through a deleted folder is broken by the folder's trash (§8.4). A temporary name never
  becomes a node's local name, also when the reply to the move aside is lost (§8.3).
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
  file's entry is listed, and selected, at the place of its remote file; a deleted folder moved aside to a temporary name (§8.4)
  under its own name (D121). An entry the planner is about to remove is left out.
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
| Within the same parent (remote) | `rename_node` (`sdk/client.rs:1170`) |
| Across parents (remote) | `move_node(.., Some(name))` (P10; `sdk/node/operations.rs:364`), one call |

Before each remote rename or move the executor fetches the node fresh (`get_nodes`, one call) and decides (D040):

- **Gone or trashed:** the node is settled like any vanished one (§10.5), then planned again.
- **At its own `.pdmount-swap-<ino>` in the folder it is known in remotely:** this device's move aside (§8.2) took effect,
  but its reply was lost (a timeout, a 5xx after the commit, the SDK's resend answered with `NameConflict`) or a crash
  did not commit its row. Only the node's remote state takes the temporary name; its local placement stays, the move
  aside counts as done, and the plan goes on to the local name. No record is written. A listing (§9) that shows the node
  there does the same: the node does not follow to its own temporary name (D117 amended, V-7).
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

The server answers all of the refusals above with one code, 2000. The adapter tells them apart by the node's state, as
C#'s `NodeMoveOperation` reads the node again (M6.0.9, parity N2): it reads the node fresh (gone or trashed:
`NotFound`; another name hash than the one sent: `NameConflict`), for a move the target folder and its ancestors up to
the root (the node among them: `InvalidMove`; the target gone: `NotFound`), then the target folder's children (another
node with the new name: `AlreadyExists`); none of these is `NameConflict`. Only when one of these reads fails, or a node
on the way cannot be decrypted, does the English message decide ("out of date", "sub-folder"), as before.

The server enforces the hash (M3.1, Q1b), so a race inside the pre-check's round trip is caught too. Were it not enforced,
a rename race would be last writer wins; a test documents that case.

### 8.4 unlink and rmdir

- `unlink` removes the name. If the inode is open, it becomes an **orphan** (`parent_ino = NULL`). Staging and cache pins are kept until the
  last release and `nlookup == 0` (forget); meanwhile `fstat` reports `nlink` 0. A remote node gets a Trash work item; the per-node result map is checked
  (`sdk/client.rs:1179`). A never-uploaded file cancels its pending work.
- `rmdir` returns ENOTEMPTY if any child exists, including pending and local-only children. An unlisted directory is listed first, so a directory
  with unknown remote children is never trashed blindly.
- Deletion is always to the Proton trash (recoverable). Nothing in v1 deletes permanently.
- **Trash pre-check (§10.5):** the executor fetches the batch's nodes fresh before trashing (one `get_nodes` per batch).
  A file whose active revision differs from the one the local delete was based on (or that is marked `remote_newer`) is
  not trashed: the tombstone is forgotten, the parent is listed so that the file reappears, and `LocalDeleteRemoteEdit` is
  recorded. A folder is listed uncached (`list_children`): a child that moves out locally but has not moved remotely yet,
  or a child folder deleted locally and not trashed yet (its own check lists it), makes the folder wait for that
  child's entry (§8.2; a deep tree goes one level per executor turn, D116). If a local node is placed where such a
  folder still is remotely, the folder first moves to `.pdmount-swap-<ino>` in its remote folder and is trashed under that
  name: the node may be the very child the folder waits for, or wait for one (D117). An unknown child, a child that cannot be
  decrypted (`list_children_checked`, D132), a child file that changed, or a
  child moved into a local-only folder (§7.7: it never leaves this folder remotely, and the trash would take it along; D121) keeps the
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

On first mount, the engine calls `latest_event_id()` (`sdk/client.rs:1236`) and persists the cursor **before** the initial root
listing, so no change can fall into a gap. The cursor is stored in `event_cursor`. It is queued to the store after the rows a
page changed, so a crash never stores it without them, and is not stored at all while a refresh runs (§9.4). Every mount
resumes the stream from the stored cursor, so changes made while unmounted arrive as events; a profile without a cursor
refreshes. Every tree (§14.5) keeps its volume's cursor in its own store; trees on one volume share its poller (§9.2).

### 9.2 Loop

```
loop {
    page = poll_events(cursor)                    // sdk/client.rs:1247
    if page.refresh { cursor = start_refresh(); continue }   // §9.4, in the background
    apply(page.events)                            // §9.3; invalidations sent
    cursor = page.next; store(cursor) unless refreshing
    if page.more { continue }                     // drain immediately
    sleep(sync.poll_interval)                     // 30 s, as the official clients (D033)
}
```

**One poller per volume (D080, D110).** The loop runs in `pdmount_core::poller`: a `Poller` reads one volume's
stream once per pass and hands each page to every tree on that volume, each applying it as below. A pass locks the
cursors of all its trees in tree order and polls from the first tree's; a tree whose cursor differs (added later, a
crash between two stores) refreshes from it (§9.4), as does a tree that failed to apply a page while others succeeded;
only a page every tree failed is retried. An engine without a daemon's pollers (no `[[roots]]`, tests) runs a poller
with itself as the only tree, which is the loop above. Intervals (D112): the user's own volume (My Files', devices')
every `sync.poll_interval`; other users' volumes every 10 min, except the one whose tree was used last through a
mount (a lookup, an open or an opendir in it), which polls at `sync.poll_interval` until 5 min pass without such a use
(`QUIET_PERIOD`, the official `setBackground`; parity E2) and then goes back to 10 min. When the volume in use changes,
or is used again after the quiet period, its poller recomputes its wait at once, so it polls at once if its last pass
is older than `sync.poll_interval`; a poller whose wait ends after its volume went quiet waits on to the 10 min.
Every poller polls at its start, and at once when a tree is added.
The Rust SDK has the official clients' cadence since 0.5.0 as `EventScheduler` (`sdk/event_scheduler.rs:17-19`: the own
volume every 30 s, other scopes every 10 min, up to 1 s of jitter, one shared scope in the foreground at a time, the next
poll after the callback has finished). PdMount keeps its own pollers, whose intervals match it (D107); `docs/sdk-parity.md`
§5 compares the details (E1–E8).
While a Shared mount is mounted, one more task polls the core event stream every `sync.poll_interval`
(`Pollers::watch_core`, M5.12, D112); a `DriveShareRefresh` calls `Pollers::share_refresh`, which reads the listing again (§14.5).

Every API call of the loop (poll, `get_nodes`, listings) takes a background turn (D027), so user requests go first.
The interval runs from the end of a pass, not from its start, as in the official event manager
(`cs/client/js/src/internal/events/eventManager.ts:114-123`), so polls start `poll_interval` plus the length of the previous
pass apart. A remote change therefore shows after up to `poll_interval` plus two passes; the SLO is timed from the writer's
commit (D037, D050).

Every wait of a volume poller, after a successful pass too, adds up to 1 s at random, as the official `EventScheduler`
does (`cs/client/js/src/internal/events/eventScheduler.ts:3`, `:107-108`), so a daemon's pollers do not stay in step
(parity E1). The draw is made once a pass, so a wait recomputed for the volume in use keeps it. Under an interval of 30 s,
which only tests set (the configuration's floor is 30 s), it is at most a thirtieth of the interval.

Errors never end the loop. After a failed poll the next one waits `poll_interval` times 1, 1, 2, 3, 5, 8, 13 (then 13 again,
6.5 min at 30 s), the steps of the official event manager (`cs/client/js/src/internal/events/eventManager.ts:4`), plus the
same jitter, which the event manager does not add (its "FIXME jitter", `eventManager.ts:131`);
a 429's `Retry-After` is honoured when longer. A successful poll resets the sequence. Connection failures are counted
with those of every other caller in the shared health state (§9.6); once it is not `online`, the loop waits for `online`
instead of backing off and polls at once when it returns.

A page that every tree fails to read or apply is not retried for ever (D130). Its failures are counted by its cursor; a
failure counts when the error is the page's, a backend error that waiting does not change (`BackendError::is_permanent`:
`Other`, `NotFound`, `Integrity` and the like). Transport failures, 429 and 5xx (`Transient`, `RateLimited`), no
connection and no session (`Offline`, `Unauthorized`), and errors that are not the backend's (the store, a deadline)
neither count nor reset the count. At the third failure (`STUCK_PAGE_LIMIT`) the pass gives the page up: it logs a
warning naming the volume, takes a fresh cursor and refreshes every tree of the volume from it (§9.4), as for a server's
`refresh`, and goes on. The pages between the old and the fresh cursor are skipped; the refresh's listings cover them.
Before the fix a node whose parent folder could not be decrypted failed every `get_nodes` that named it (P13), so the
page came back at every poll and the tree stopped following remote changes, across restarts (M6.0.3 V-3).

A node that does not decrypt by itself is degraded too, not an error, in `get_nodes` and in folder listings (fork P26,
D132), so neither a page nor a refresh's listings fail on it. A listing names such children apart
(`Backend::list_children_checked`): a known child named there keeps its entry as it was, since degraded is not absent
(D119), and an unknown one is not shown. A folder deleted locally whose remote copy holds one is kept (§8).

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
| Trash (UpdateContent) | Subtree removed, `notifier.delete(parent, ino, name)` (`fuser/notify.rs:102`); a file still open that is written afterwards comes back (§10.5, D128) | Reported as *vanished*; the engine asks the server and resurrects the minimal chain (§10.5). A tombstone (local delete) whose file got a new revision is revived instead |

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
cursor may have missed changes. A failed pass is retried after 30 s. A pass whose listing of the tree's root fails with
`NotFound` or `PermissionDenied` (a revoked share) ends the task instead, with no cursor stored (`events_refreshing` stays
true); the daemon removes such a tree. The task is one of the engine's background tasks: a stop ends it, and none starts
after one (D120). The refresh listings take plain background turns; the sync's own steps take patient ones (§5.3).

### 9.5 Kernel invalidation

- The messages are `Invalidation::{Entry, Inode, Delete}` (`vfs.rs`), for every change that did **not** originate from a kernel
  request. `Engine::apply_changes` sends them on an unbounded mpsc channel after it has queued the rows; `Engine::open` returns
  the receiver. `pdmount_fuse::mount` hands it to `notify_loop`, which runs on a thread named `pdmount-notify` and owns a
  clone of `fuser::Notifier` (`Clone`, `fuser/notify.rs:52-53`). The store writer sends nothing.
- The tree derives the messages; no mutation lists them by hand. Mutations record which directory entries they touch
  (`place`/`unplace` in `vfs.rs`, a listing records all entries of its directory) and which nodes' attributes changed.
  One pass at the end (`Effects::finish`) compares each touched entry with its state before the first change. For each
  directory that was listed before the change and is still attached, it sends one message per changed name, then
  `inval_inode` on the directory (its pages are cached, `FOPEN_CACHE_DIR`). The message is `delete` when the node that
  held the name has left the namespace, and `inval_entry` otherwise (a moved node must not be marked dead). Entries
  inside a removed subtree get nothing: the `delete` of its top covers them. Then `inval_inode` goes to each node whose
  attributes changed.
- ENOENT from the kernel (nothing cached) is ignored.
- The notify thread is not a FUSE thread and holds no core locks while calling into the kernel.

### 9.6 Offline mode

One `Health` per session holds the state `online`, `offline` or `needs_login` (My Files' engine makes it; further trees
share it through `EngineOptions::health` and do not probe it themselves, D110) (`crates/pdmount-core/src/health.rs`, D041).
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
The fake backend does the same (M5.6 finding 4): an online start stores the drive's account and root in the same file
(volume `fake`); when the fake's `account` call fails with `Offline` the daemon starts in `offline` from it, and with
`Unauthorized` in `needs_login` (D042); the handle then answers `account` from the stored value without a request, as
`ProtonBackend::with_root` does. Without a stored account, or with any other error, the start fails.

---

## 10. Multi-device and conflicts (RQ3)

### 10.1 Assumptions

- Several devices mount the same account, possibly at the same time, and the official apps (web, desktop, mobile) also edit the tree.
- Concurrent edits to the same file **will** happen. The system must never lose any version that any device acknowledged to an application.

### 10.2 Device identity and tracking

- Config `device.name` (default: hostname) and `device.id` (UUID, generated on first run, stored in `meta`).
  The SDK client uid is `pdmount-<device.id>`, which becomes the `ClientUID` on drafts (`sdk/node/draft.rs:196`).
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
| Remote delete, local edit | **Resurrect**: upload the local content as a new file at the same path, recreating missing ancestors with new links (conflict name if the path is now occupied). This also holds for an edit that comes after the delete removed the clean file locally, through a handle opened before (a write or truncate on the detached node): the file comes back with the removed folders above it that are still in memory, each with only the chain to it; the topmost goes into the nearest folder still shown, under its old name or its conflict name, or into the root if that folder is no longer known (D128). A file removed by a local `unlink` or `queue drop` does not come back |
| Local delete, remote edit | Before trashing, check `active_rev == base_rev` (§8.4). If not, **do not trash**: the newer remote revision reappears locally and a `LocalDeleteRemoteEdit` record is written. Trash has no server precondition, so a small window remains: a revision written inside it is in the trash with the file. The trash is recoverable, which mitigates it |
| Remote delete, local clean rename | The delete wins (content is in the Proton trash). Recorded. This holds for a node whose remote placement is inside the deleted folder. A node renamed into the deleted folder from outside it is not deleted on the drive: its rename loses, and it returns to its remote placement (`adopt_remote_placement`; a rename step whose destination folder is gone settles the folder, not the node) |
| Rename/rename (same node, different targets) | Fresh pre-check, then the `OriginalHash` precondition (enforced, §10.4; §8.3). The first committer wins: the inode moves to the remote name, and a `RenameRename` record is written |
| Move cycles rejected by the server | Keep the server's placement, write a `MoveCycle` record (park if the inode cannot follow) |
| Remote folder trashed, local pending descendants | Resurrect a minimal chain of folders containing only the pending items. Clean descendants are removed locally |
| Local folder delete, remote has unknown/newer children | Re-enumerate the folder uncached before trashing (§8.4). Trash only the children known and unchanged, and keep the folder with the rest (`LocalDeleteRemoteChildren`). Children that were moved out locally but not yet remotely, and child folders deleted locally but not trashed yet, make the folder wait |

The tree carries the pure steps of these resolutions (`vfs.rs`, unit-tested; the listing rules and `resolve_vanished` call them since M3.8, the journal from M3.9 on, including its rename and trash steps since M3.11):
`split_conflict` (edit/edit: the local inode takes the conflict name, the server version becomes a new inode at the original name),
`yield_name` (a local rename to a free conflict name), `unbind_tombstone`, `adopt_remote_placement` (refuses cycles, unlisted
folders and taken names), `resurrect_minimal` (keeps the pending items and the folders that lead to them as nodes to create
again, removes the rest, and reports the removed nodes whose clean rename lost), and `bring_back` (a removed file written through an open handle, D128). `Changes` also carries `vanished`, `clashes`,
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
- WARN-level log, an INFO line with the command that shows the conflict (a further tree's line names the tree: `conflict
  recorded in a further tree`), and for an `open` conflict a desktop notification (next item). The xattrs of §8.7 mark both copies.
- **Desktop notifications (M6.2, D077, D139; `crates/pdmount/src/conflict_notify.rs`).** Only conflicts in state `open` notify;
  `notice` ones stay in the log and `conflicts list`. One `Announcer` per daemon takes the conflicts of every tree
  (`Announcer::follow` for My Files and each further tree, a Shared root's trees included), so the limits are the profile's:
  - *When:* a conflict is looked up when its turn comes (`Engine::conflict_view`) and sent only while still `open`. Up to three
    notifications of their own per 30 s (`notify::Burst`); the rest wait and go out as one when the window has room, summary
    "PdMount: conflicts", body "N more conflicts; see `pdmount conflicts list`" (a held conflict resolved meanwhile is not
    counted; a sum of one is that conflict's own notification; conflicts a lagging receiver missed are counted unseen). The
    sum counts as one of the three. Over D-Bus (§14.4, `notify.rs`): with no bus or no notification server nothing is sent.
  - *The text:* summary "PdMount: conflict"; body two lines: the file's path in its tree (the original name in the folder of the
    conflict copy; for `local_delete_remote_edit` the file that came back), prefixed with "My Files: " or the tree's name (a
    share's) when the profile has further roots, then what happened: `edit_edit` "Changed here and on another device; both
    copies kept.", `create_create` "Created here and on another device; both copies kept.", `name_taken` "Another device created
    a file of this name while a file here that is not uploaded had it; both copies kept, the one here renamed.",
    `local_delete_remote_edit` "Deleted here but changed on another device; the changed file was brought back." Names come from
    other devices (D070): the first line (path and tree) is cleaned by the one rule for names in notifications
    (`notify::clean_name`, §14.4: control, separator, bidirectional formatting and invisible characters, blanks that are
    letters and slash look-alikes become U+FFFD), and the body is escaped for a server with `body-markup` (`notify.rs`). With `notify.conflict_names = false` the body is the
    second line only (no file, no tree); with `notify.conflicts = false` nothing is sent (Appendix B).
  - *The **Show** action:* offered when the server has `actions` and a copy is in the tree: the conflict copy (for
    `local_delete_remote_edit`, the file that came back; for a shared single file, the file in `Shared files`). The announcer
    subscribes to `ActionInvoked` and `NotificationClosed` (`notify::subscribe`) only while notifications with **Show** are
    open (at most 64 remembered; one is forgotten at its `NotificationClosed`). The action calls
    `org.freedesktop.FileManager1.ShowItems([<file:// URI>], "")` on the bus the notification went to (a connection through
    the user check of §14.4, 10 s for a file manager that D-Bus starts); the URI percent-encodes every byte but `A-Z a-z 0-9
    - . _ ~ /`; a further mount's only tree (a `test` root with one tree) is at the mountpoint itself, not in a folder of its
    name. When a bus that passed the check answers the call with an error (no file manager: `ServiceUnknown`, a failed
    activation), `xdg-open <folder of the copy>` runs with `DBUS_SESSION_BUS_ADDRESS` set to that bus and the `DISPLAY`,
    `XAUTHORITY` and `WAYLAND_DISPLAY` of the last attach (those it did not send removed; the daemon's own without an attach,
    or once the attached bus was dropped, D102), in a process group of its own, in `/`, without `NOTIFY_SOCKET`, not waited
    for; `xdg-open` is looked up in the absolute directories of `PATH` only. Both are logged at INFO. A bus that cannot be
    reached or is not this user's, and one whose connection fails during the call or that does not answer within the 10 s,
    gets neither (logged at WARN; the latter is dropped when it no longer answers `GetId`): its address does not go to a
    program (M6.5 V-6). **Show** acts once per notification: a second `ActionInvoked` for it opens nothing (M6.5 V-9).
  - *Notified:* a conflict counts as notified when the server took the notification that names or sums it
    (`conflict_notify::notified`): the row's `notified_at` (§13.2, schema 6, D140) is set, so it is shown at most once (a
    `kill -9` between the answer and the commit may show it once more). A send the server did not take (none, a timeout, an
    error) marks nothing and leaves the window (`Burst::retract`), and is tried again 60 s later: the retry takes every followed
    tree's unshown conflicts as a service's appearance does, a sum that failed keeps its held conflicts and its count of missed
    ones for the next try, and the held ones wait for the retry. A failed retry sets the next one; a service that appears
    meanwhile ends the wait.
  - *Missed conflicts (M6.3, D140):* when the daemon starts, and whenever a notification service appears, the `open`
    conflicts that are not marked go out, each tree from its own store (My Files, shares, test trees; a tree followed while a
    service is up does so at once). The signal is `notify_watch.rs`: a task on the bus notifications are routed to (the
    attached bus, else the process's own, §14.4; when neither owns the name, whichever exists) that runs a callback when
    `org.freedesktop.Notifications` is owned or activatable at connect time and on every `NameOwnerChanged` with a new
    owner; the bus is chosen again every 60 s and at once after an attach or a drop, and a daemon with no reachable bus tries
    to connect every 60 s, quietly (debug-level log). A conflict resolved meanwhile is not shown. Up to three go out on their
    own, as live ones; more than three in one notification "PdMount: conflicts", "N conflicts were recorded while no desktop
    was listening; see `pdmount conflicts list`", counted as one of the window's three (with the window full, they are held
    for its "N more conflicts"). With `notify.conflicts = false` the conflicts are marked without a notification (at the
    start, as they are recorded, and every tree's unshown ones when a lagging receiver missed some) so that turning the
    setting on later shows nothing old, and no watcher runs. Rows from before schema 6 count as shown.

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
- **Write gate (D080, D110, D114):** each tree has one, open for My Files; a share's follows its role (§14.5). It is
  checked with the rules above: closed, every operation of the table answers EROFS, reported modes lose their write
  bits and `user.pdmount.access` reads `ro`, as under `mount -o ro`; work already accepted still uploads. It changes at
  runtime (`Engine::set_write_gate`), invalidating the tree's cached attributes; unlike `-o ro` it is not a kernel
  read-only mount.
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
  A poll that fails in transport, takes over 30 s, or answers 408, 429 or 5xx is retried at the same interval (after
  `Retry-After` when longer) instead of ending the sign-in; any other error ends it. A sign-in that ends without a session
  invalidates the fork's selector on the server (`DELETE auth/v4/sessions/forks/<selector>`), best effort (P16, D071).
- The selector authorises collecting the session's tokens, so it never appears in an error text or the log: the fork's
  errors name the path template `auth/v4/sessions/forks/<selector>`, and PdMount redacts `/sessions/forks/<segment>` in the
  sign-in error before it logs it, keeps it for `status` (`sign_in_error`) or `login` prints it (D071).
- At login, PdMount prints: *"PdMount is a third-party application, not officially supported by Proton."*
- Credentials are stored with `to_stored_credentials_with_latest_tokens()` (`rs2/session.rs:223`) in the OS keyring (service
  `pdmount`, entries `<profile>:session` and `<profile>:cache-key`). Files are used only with a flag (§12.4, D029).
- If the new session cannot be stored, `login` ends it on the server and fails.
- `pdmount logout` revokes the session where possible and deletes the stored credentials and cache key. It reports keyring
  entries it could not delete and exits non-zero.

### 12.2 Presenting the sign-in link (RQ5)

This applies to `pdmount login` and to `pdmount mount` when there is no valid session.

- **`pdmount login` with a working session (D108):** without a daemon, `login` first resumes a stored session as
  `status` does (§14.2), but may prompt for a locked keyring (D104). A session that resumes is kept: `login` prints
  `Profile <p> is already signed in as <account>` and exits 0. One the server rejects (or whose keys cannot be
  unlocked, D085), or no stored session, leads to the web sign-in. Any other failure (Proton unreachable, keyring
  unavailable) is reported, exit 1, with a pointer to `--force`. `pdmount login --force` skips the check and signs in
  anew, replacing the stored session.
- The sign-in URL and user code from the `on_sign_in(url, code)` callback are **always printed** on the command line.
- The link is **also opened in the default browser** (`xdg-open`) according to `--browser` / `--no-browser`, or
  config `auth.browser = "auto" | "always" | "never"`.
  - `auto` (default): open the browser only when the process is interactive (stdin and stdout are TTYs) and `$DISPLAY` or `$WAYLAND_DISPLAY` is
    set. Otherwise (SSH, headless) the link is only printed.
  - If launching the browser fails, PdMount prints a warning; the printed link remains the fallback.
- **`mount` without a session:** `mount` runs the login flow before mounting (browser per `auth.browser`). With `--no-login`, it
  exits with an error instead (for scripts).
- **`pdmount login` while a daemon runs:** the command sends the control request `login` (`force` with `--force`). A daemon
  waiting for the keyring tries it first (D106). Then, without `force`, a daemon that is `online` or `offline` answers
  that no sign-in is needed, with the mount's account, and `login` prints that the profile is already signed in and
  exits 0 (D108); offline is a network state, not a rejected session. In `needs_login`, or with `force`, the daemon runs
  the sign-in and keeps its mount (D042): the command presents the link as above and polls `status` until the daemon's
  login is over; the new session replaces the old one there. A login as another account than the mounted one is refused.
  The fake backend has no sign-in: there `login` answers like a Proton daemon when no sign-in is needed, and fails
  otherwise.
- **From autostart or in the background:**
  - The daemon shows the link in `pdmount status` (`Sign-in:` and `Code:` under `Session:`), which reads it over the 0600 control
    socket. The log carries only the code and a hint to run `pdmount login` or `pdmount status`; the desktop notification
    (over D-Bus, only with a session bus and a notification server; at most one per 30 s) says "Sign-in needed for profile <p>: run `pdmount login`".
    A sign-in the user asked for (`login`, `login --force`) is announced instead as "PdMount: sign-in link ready", "Sign-in
    link ready for profile <p>: open it from `pdmount login` or `pdmount status`" (an INFO log line); a running sign-in
    keeps the reason it started with (D108).
    Neither carries the link: its fragment holds half of the fork secret, and a notification's text would be on screen (D071; since M6.1 nothing is
    passed in a process's arguments).
  - The mount stays up in `needs_login` and does not exit. The flow lasts 10 min; `pdmount login` starts a new one on demand.

```
$ pdmount login
Profile default is already signed in as <account>; the mount continues (`pdmount login --force` signs in anew).
```

```
$ pdmount login --no-browser
PdMount is a third-party application, not officially supported by Proton.
Open this link to sign in:  https://account.proton.me/…
Code: ABCD-EFGH
Waiting for sign-in… done. Logged in as <account>.
```

### 12.3 Token lifecycle

- **Rotation:** `token_credential.subscribe_tokens_refreshed()` (`rs2/auth.rs:99`) persists every rotation immediately, in a task of
  its own (a keyring prompt may take 120 s). A rotated refresh token that is lost would force a re-login. A 60 s timer stays as a
  fallback.
- **Storing the tokens (D106 amended, finding 8):** a resume writes the session only when the check changed its tokens
  (compared with the session as loaded). A write that fails after the server accepted the session does not fail the
  resume: the session is active with its tokens unsaved (`LoggedIn::unsaved`). The daemon goes online with it and its
  token saver writes them later (the 60 s timer, the next rotation, the shutdown's last save); while they are unsaved,
  the timer and a rotation first probe the collection without a prompt (`keyring_state`) and write only when it is
  unlocked (in allow mode also when the keyring is unavailable, since the write then goes to the file), so a retry
  never opens an unlock dialog. The log has one WARN per failure streak and an INFO when a save ends it; `status`
  prints `Tokens:     not yet saved to the keyring (<reason>)`. A one-off command (`status`, `login`, `trash empty`)
  goes on with the session; `login` warns that the next mount may need a sign-in, since the stored tokens are the
  ones the check replaced.
- **Reactive refresh (`with_reauth` around every `Backend` call):** when the adapter classifies a response as 401, it calls `token_credential.get_refreshed_access_token(rejected)`
  (`rs2/auth.rs:144`, single-flight; `token_credential` is public at `rs2/session.rs:37`) and retries once. Clones share token state
  (`rs2/auth.rs:22-73`), so the drive client never needs rebuilding.
- **Proactive refresh:** an auth task owning the session (`&mut`, behind a tokio Mutex) calls `ensure_authenticated()` (`rs2/session.rs:503`)
  every 30 min while online; an expired session sets `needs_login`, connection errors are left to the health state (§9.6).
- **Refresh failure** (P9 surfaces it; `subscribe_refresh_token_expired()`) moves the daemon to `needs_login`:
  - local reads of cached data and all local writes continue;
  - network-bound requests return EIO;
  - the link presentation of §12.2 starts;
  - a login for the mounted account replaces the backend's session (`replace_session`), kicks the probe (§9.6) and the queue drains.
    A mount can start in `needs_login` when it has local state and a known root (D042).
- **Start without the keyring (M5.17, D106):** when the session or the cache key cannot be read because the keyring is
  unavailable (no Secret Service on the bus, a dismissed unlock dialog; in allow mode too), a profile that has mounted
  before (a known root, and `state.db` made for it: `meta.root_link` equal) mounts at once in `needs_login` instead of
  exiting, which a supervisor would answer with a respawn loop. The engine runs on a stand-in backend
  (`keywait::Deferred`: the stored account, every other call `Unauthorized`) with the disk cache closed (§6.2, its
  blocks are sealed with the cache key): the stored tree, staged content and all local writes work, clean content reads
  EIO, the queue waits. Readiness is signalled once the mount answers (§14.4).
  - *Under a service* (`--service`): the start's keyring calls wait 30 s at most in all, and a profile that has mounted
    before first probes the default collection without a prompt (`keyring_state`); unless it is unlocked, every keyring
    call of the start fails at once as unavailable (`credentials::hold_keyring`), so the start reads nothing, shows no
    dialog and is ready in seconds. A foreground `mount` without `--service` reads and may prompt.
  - *Retries* (`keywait::Wait`): at `session attach` (D102) and at `pdmount login`, which read and may prompt (`login`
    then answers "already signed in" unless the mount came up in `needs_login` or `--force` is given, D108; an attempt still running after the keyring
    timeout plus 60 s is reported as running). The timer depends on what the last attempt waits for: the keyring
    (unavailable or locked) every 60 s, only when the collection is then unlocked (probe first, no prompt); the user (no
    session, no cache key, another account's session) never, `status` and `login` say what to do; the backend (the
    start's network failed) after an exponential back-off from 60 s up to 30 min and at least Proton's `Retry-After`,
    which an attach or a login within it respects too.
  - *When it answers:* the cache key is read but never made (a daemon with its caches open makes none,
    `credentials::caches_open`; a missing one waits for the user, and nothing is deleted); the backend is opened as at a
    start (`open_backend`) with the mount's root expected, so another account's root is refused before it is stored;
    the disk cache is unlocked; a Proton session's token watch starts before the backend replaces the stand-in; the
    state becomes the backend's; then the refresh and D042's sign-in start.
  - The wait's keyring calls run on threads of their own: a stop does not wait for one waiting for an unlock dialog.
    One WARN in the log; `status` gives the reason; one desktop notification, once one can be delivered (a bus, the
    attached one or the own, with a notification server that owns or can start its name; chosen per service, §14.4).

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
  - **Waiting:** a locked collection waits for its unlock prompt; `dbus-secret-service` waits up to a year. Every other
    D-Bus call of `dbus-secret-service` 4.1 times out after 2 s ("Did not receive a reply"), which is what a Secret Service
    that is still starting, or never starts, produces. M5.16 (D101): before a call on the process's own bus or, without
    an attached bus, the service's (§14.4, D102 amended 2026-11-14), `bus::state_on` asks whether `org.freedesktop.secrets` is
    owned or activatable (`NameHasOwner`, `ListActivatableNames`; the 30 s cache of §14.4; at most the call's own
    timeout, 6 s at most). Owned: the call goes ahead and the note after 2 s names an unlock dialog. Activatable only: it
    goes ahead, the note says that D-Bus is starting the Secret Service, and a failure is reported as "the Secret
    Service did not start" with the bus address and the `Exec` line of the activation file (found in `$XDG_DATA_HOME`
    and `$XDG_DATA_DIRS`, `dbus-1/services`; on the service's bus the message says "the service's session bus"), without
    a prompt of its own. Neither bus: fail at once with "no Secret Service on the session bus", naming the own bus and,
    when it was asked, the service's, no call made; likewise "no session bus" (no `DBUS_SESSION_BUS_ADDRESS` and no
    `$XDG_RUNTIME_DIR/bus`) and a bus that does not answer. The failure counts as the keyring being unavailable (the
    flag below). Each keyring
    call runs on a helper thread and is abandoned after 120 s (`PDMOUNT_KEYRING_TIMEOUT_SECS` overrides it for tests). After 2 s, a note on stderr says that an unlock dialog may
    be open, possibly behind other windows. Once a read, a delete or a probe finds the keyring unavailable, the
    process does not call it again until a probe (`keyring_state`, which clears this) or the daemon's last token save
    at shutdown.
    A failed write does not stop later calls (D106 amended): it comes after a read worked, so the keyring is there.
  - **Timed-out writes (D106 amended):** gnome-keyring rewrites its whole file in `SetSecret`/`CreateItem`, which can
    take longer than the 2 s per call. A write that fails with `org.freedesktop.DBus.Error.NoReply` (found in the
    error's chain as a `dbus-secret-service` D-Bus error) is read back after 500 ms and compared (equality only, never
    logged); if the value differs or cannot be read, it is written once more, and a second time-out is read back too.
    Only then does it fail; any other error fails at once. This runs inside the call's timeout, in the process or in
    the keyring helper, so moving a file into the keyring gets it too.
  - **Reads:** a secrets file, if present, is the newest copy, because every successful keyring write deletes it. It is
    read first, in every mode. Outside force mode it is then moved into the keyring and deleted, unless another daemon of
    the profile is running (the pid file, §13.1), in which case it is left in place.
  - **Queries do not prompt (D104):** a command that reads secrets only to display something must not open an unlock
    dialog. `status` without a daemon asks the Secret Service whether the default collection is locked (as `doctor`
    does, without a prompt) before it resumes the session; when it is, it reads nothing and prints
    `Session:    unknown (keyring locked; unlock it or run `pdmount login`)`, still showing device and mount state,
    and exits 1. With a daemon, `status` takes everything from the control socket and reads no secret. Force mode never
    makes the probe. `stats`, `queue`, `conflicts`, `cache`, `sync`, `access`, `pin` and `doctor` need no secret from
    the keyring or, for `doctor`, probe first.
  - **Cache key:** creating a new cache key deletes the SDK caches and the block cache, which the old key encrypted.
    The SDK secret cache goes with them, so in force mode the session and the cache key must both be in files. In
    allow-file mode, an unavailable keyring with no key file creates a key only when none of these caches exist;
    otherwise the command fails, because the key that encrypted them may be in the keyring. A daemon that has opened
    its caches makes no key at all (`credentials::caches_open`, D106): a missing key is an error there (`NoCacheKey`),
    not a new key that would delete the caches under its mount.
  - **Daemon:** `mount` reads the stored session in the foreground process before it starts the daemon, so an unlock
    dialog appears while the user waits at the terminal. The daemon gets the same flag. It stores tokens only when they
    have changed (§12.3). A daemon whose keyring does not answer at its start mounts and waits for it (§12.3, D106).
  - **The desktop's bus (M5.17, D102):** `keyring` and `dbus-secret-service` take the bus from the environment only, and
    the daemon never changes its own. While `session attach` has handed it a bus other than its own (§14.4), each keyring
    call (get, set, delete and the lock probe `keyring_state`, which `doctor`, D104's `read_would_prompt` and the
    waiting daemon use) first chooses its bus by the per-service order of §14.4 (D102 amended: `org.freedesktop.secrets`
    owned on the attached bus, else owned on the own bus, else activatable on the attached, else on the own; with none,
    unavailable at once without a call). On the own bus the call runs in the process as without an attach; on the
    attached bus it runs in a child, `pdmount keyring-helper` (hidden; `/proc/self/exe`), with that bus in the
    child's environment. One JSON request goes to its stdin and one JSON reply comes from its stdout
    (`found`/`missing`/`done`/`state`/`failed`); key and value never go into argv (D071), the child writes no log and no
    stderr, and the replies' `Debug` hides values. The call runs under the same 120 s timeout and the child is killed at
    it; the request is written to its stdin from a thread of its own, so the timeout covers the write too. Before each
    run the kept address is checked (`bus::route`: it answers `GetId` and its socket's peer has the daemon's uid,
    §14.4) and dropped if not. A failed call, on either bus, clears the cached choice; on the attached bus it also checks
    the bus (`GetId`, 2 s) and drops the address when it does not answer (logout); later calls use the daemon's own bus
    until the next attach. A keyring on the own bus opens its unlock dialog on the desktop once an attach has passed the
    desktop's display to that bus (§14.4, D102 amended).
  - **Tests:** tests use force mode, except the keyring tests (`scripts/repro/keyring-d024.sh`).
- Staging is plaintext with mode 0700 (§17).
- The SDK fork never logs secrets or response bodies (P16, `docs/sdk-patches.md`). Upstream 0.5.1 logs the key passphrase
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
| `~/.local/share/pdmount/<profile>/trees/<key>/` | a further tree's `state.db` and `staging/` (§14.5, D110); `<key>`: 32 hex digits of a BLAKE3 hash of its volume and root link. My Files' paths above are unchanged |
| `~/.local/share/pdmount/<profile>/orphaned/<share> (<time>)/` | the queued changes and staged content not uploaded (local-only files, edits in local-only folders) of a share that went away with them: `files/`, `pdmount-orphaned.json`, and `state/` when some content could not be written (§14.5, D114, D124); the user deletes it |
| `~/.local/share/pdmount/<profile>/shared-names.json` | the names the Shared mount shows its shares under, by share key, so a remount keeps them (§14.5, D125); kept by `reset` |
| `~/.cache/pdmount/<profile>/trees/<key>/blocks/` | a further tree's block cache (16 MiB RAM pool; counted against the profile's `cache.max_bytes` with My Files', D110 amended, §14.5) |
| `$XDG_RUNTIME_DIR/pdmount/<profile>.sock` | control socket (0600); `~/.local/share/pdmount/<profile>/control.sock` when the session has no runtime directory |
| `<profile>.pid` beside the control socket (`control.pid` in the fallback) | process id and mountpoint of the mount daemon, for `unmount --force` and for `status` after a crash |
| `$XDG_RUNTIME_DIR/pdmount/<profile>.attach` | the desktop's bus and display kept by the last `session attach`, JSON (0600), read at the next daemon start (§14.4, D102 amended); none without a runtime directory |
| `$XDG_STATE_HOME/pdmount/<profile>.log` | log; a background `mount` moves one above 16 MiB to `<profile>.log.1`, replacing that; 0600 in a 0700 directory, set again at every background `mount` |

### 13.2 Schema

```sql
meta(key TEXT PRIMARY KEY, value BLOB)
  -- root_link, ino_reserved (inode numbers up to it are handed out; reserved 16 384 at a time, durably),
  -- access_version, access_config_imported:/<path>, relist (folders to list again, space-separated inode numbers, D069),
  -- pending_marker (link of this device's marker on the drive, D076), incoming_notified (marker revisions announced, JSON),
  -- incoming_devices (other devices whose markers were read, oldest first, at most 64, JSON; V-12).
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

queued(ino INTEGER PRIMARY KEY, queued_at_ms INTEGER NOT NULL)
  -- since migration 4 (D105): when a journal entry was queued, in unix milliseconds. The journal inserts a row when it makes
  -- the entry and deletes it when the entry leaves (uploaded, forgotten), through the store's write queue, so the rows follow
  -- the entries in order. At mount the rows are read: an entry made for a restored inode starts with the age
  -- `now - queued_at_ms`, clamped to 0 .. 365 days, so `status` and `queue list` count from when the entry was queued. Rows
  -- of inodes with no entry at mount are deleted. A lost row (a crash) only means an age counted from the mount. The age is
  -- display only: `upload.max_dirty_age`, the debounce and the back-offs keep the monotonic time of this process.

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
          remote_device TEXT NULL, created_at INTEGER, state TEXT, other_ino INTEGER NULL, detail TEXT NULL,
          notified_at INTEGER NULL)
  -- other_ino and detail since migration 3; index conflicts_state(state); created_at in unix seconds;
  -- remote_device is unused (always NULL; nothing recorded it)
  -- notified_at since migration 6 (M6.3, D140): unix seconds when the notification service took the conflict's desktop
  -- notification (§10.8); NULL: not shown yet. Migration 6 sets it on every existing row, so nothing old pops up. Written
  -- once (`UPDATE ... WHERE notified_at IS NULL`); the row's upsert (`store_conflict`) never changes it. With
  -- `notify.conflicts = false` the daemon sets it without a notification.

event_cursor(volume TEXT PRIMARY KEY, event_id TEXT, updated_at INTEGER)

access_rules(id INTEGER PRIMARY KEY, ino INTEGER UNIQUE, link_id TEXT NULL, path_at_set TEXT,
             mode TEXT CHECK(mode IN ('ro','rw')), source TEXT CHECK(source IN ('cli','config')),
             status TEXT CHECK(status IN ('active','orphaned')), created_at INTEGER, updated_at INTEGER)

signatures(link_id TEXT PRIMARY KEY, parts INTEGER, signer TEXT, digest TEXT, first_seen INTEGER,
           accepted_digest TEXT NULL, accepted_at INTEGER NULL)
  -- since migration 5 (M6.0.6, D132): a node whose signatures did not verify at its last decode (§6.3); parts are bits
  -- (1 name, 2 key, 4 content key), digest one token per failed part, first_seen and accepted_at unix seconds. Written
  -- with the node's `remote` row (`write_rows`; the Photos tree with its `photos` row), deleted when it verifies or its
  -- remote row goes; an upsert keeps first_seen and the accept. Loaded with the rows at mount (the tree's remote states
  -- carry the failures) and into the tree's memory (digest and accepted digest per link, for the refusals).
```

**Migrations:**
- Numbered SQL files, applied in one transaction at startup.
- A downgrade is refused.
- Migration 4 (M5.6, D105) creates `queued`.
- Migration 5 (M6.0.6, D132) creates `signatures`; a store from before has no found nodes until its nodes are decoded
  again.
- `inodes.state`, `inodes.nlookup_persist`, `remote.trashed` and `revision_blocks.sha256` are not used.
- `pdmount reset --keep-login` deletes the store, staging and block cache (the further trees' under `trees/` too, D110); the session, the SDK caches, `proton-root.json` and the device identity stay. It is refused while the journal has pending rows; a refusal for further trees names their `trees/<key>/` directories. `--orphan-trees` moves those trees' state to `orphaned/tree <key> (<time>)/state/`, with a manifest that `status` lists, instead of refusing: a gone share's kept state cannot be uploaded by mounting it (D118).

---

## 14. CLI, control socket, operations

### 14.1 Argument convention

Options come first, then verb or mode arguments, then **the path last**, with at most one positional path. `--profile <name>` is a global option
(`pdmount --profile work status`; also accepted after the subcommand), as is `-v/--verbose`. The global options `--allow-file-secrets` and
`--force-file-secrets` (mutually exclusive) select the secret storage mode (§12.4).

### 14.2 Commands

| Command | Purpose |
|---|---|
| `login [--browser\|--no-browser] [--force]` | Web sign-in (§12.2); with a running daemon, the daemon runs it (D042). A working session is kept: with a daemon that is not in `needs_login`, or without one when the stored session resumes, `login` says the profile is already signed in as <account> and exits 0; without a daemon, a check that fails for another reason than a rejected session exits 1. `--force` signs in anew all the same (D108) |
| `logout` | Remove the session and keys |
| `mount [--foreground] [--browser\|--no-browser] [--no-login] [-o ro] [--backend proton\|fake:<dir>] [<mountpoint>]` | Start the daemon and mount (mountpoint overrides config). `--backend fake:<dir>` and `pdmount fake seed\|changes\|rename\|trash\|write\|create\|mkdir\|unverified\|fault\|profile` exist only in test builds (feature `fake-backend`, §15.1). `--no-login` fails instead of signing in (M3.16). Only with `--foreground`, for a supervisor (M5.3, D093, §14.4): `--service openrc\|systemd`, `--notify-fd <n>` (n ≥ 3), `--clear-stale`; `$NOTIFY_SOCKET` needs no option. `-o ro`: every write path answers EROFS, as a root `ro` rule would, without changing the rules (D019); pending work still uploads |
| `unmount [--wait\|--force\|--if-stale] [<mountpoint>]` | `--wait` (the default): upload what is pending first, up to 10 minutes, and list parked items (§8.2). `--force`: see §14.4. Under a service (`--service`) the plain form refuses, exit 1, and names the service's stop command; `--force` still works and says what the supervisor then does (OpenRC restarts the daemon, the systemd unit stays stopped; D093). `--if-stale`: detach the mountpoint only if it is a stale pdmount mount, else do nothing, exit 0; the systemd unit's `ExecStopPost=` (D094). Another profile's live mount at the mountpoint is never detached: `--if-stale` and the plain form refuse it (exit 1), `--force` refuses one whose source names another profile (D127) |
| `status` | Online/offline/needs_login, queue depth, oldest pending, parked items, conflicts, cache use, sign-in link if pending. M2: session, quota, mount, `Queue: N pending, M parked`, inodes, cache. M3.14: `Conflicts:  N open` when N > 0. M4.12: `Queue: N pending (oldest 3m), M parked` (the oldest counts from when the entry was queued, also across a restart, D105), `Events: last poll 12 s ago[, refreshing]`, `read-only` on the mount line. `Tokens:     not yet saved to the keyring (<reason>)` while the session's current tokens are not stored (§12.3, D106 amended). M6.0.6: `Signatures: N unverified, M refused (pdmount signatures list)` when My Files has found nodes, and `    Signatures: …` under each further tree that has any (§6.3, D132). Without a mount it resumes the session itself, but not while the daemon is still shutting down (a resume could rotate the tokens the daemon is about to save). It never opens a keyring unlock dialog (D104): with a daemon it reads no secret; without one, a locked default collection gives `Session:    unknown (keyring locked; unlock it or run `pdmount login`)` and exit 1 (§12.4) |
| `sync [--timeout <s>] [<path>]` | Remote barrier (§5.6): upload now and wait until nothing under `<path>` is pending. Exit 1 with parked entries or at the timeout (default 600 s) |
| `access set ro\|rw <path>` / `access unset <path>` / `access list [<path>]` / `access effective <path>` | §11.5 |
| `pin <path>` / `unpin <path>` / `pin list` | Keep offline (§5.3), through the daemon. `list`: path, files, bytes fetched of known total, state (`fetching`, `complete`, `waiting-offline`, `waiting-space`). `unpin` takes the path of the pin itself and names it when given a path inside. `status` adds `Pinned: N paths, X of Y fetched` when N > 0 |
| `queue list [<path>]` / `queue show [--id <ino>] [<path>]` / `queue retry\|park\|drop [--id <ino>] [--all] [<path>]` | Inspect and control the journal (§8.2, D061). Ids are inode numbers; a path selects its entry or, for a folder, every entry under it. `drop` prints what it would discard and changes nothing without `--yes` (exit 1) |
| `conflicts list [--all] [<path>]` / `conflicts show <id\|path>` / `conflicts resolve keep-both\|keep-local\|keep-remote <id\|path>` | §10.8 |
| `signatures list [--json]` / `signatures accept <id>` / `signatures check [<path>]` | Nodes whose signatures do not verify, in every tree of the daemon (§6.3, D132): `list` shows each with its id, state (`refused`, `accepted`, `warned`), path (`[name unverified]` after a name whose own signature failed), root, failed parts, claimed signer and first sighting; `--json` prints them as one array. `accept`: handled as verified on this device while its failures stay as they are. `check`: every folder under the path in My Files, or of every tree, listed from the server and its nodes checked, paced as a refresh; progress every 10 s, then a summary; one check runs at a time, a second command follows the running one |
| `cache stats` / `cache gc [--target <bytes>]` / `cache wipe [<path>]` | Block cache maintenance, through the daemon. `stats`: disk bytes against the cap, blocks, pinned bytes, protected revisions, pool slots; with further trees, My Files' bytes and blocks and the profile's total against the cap (§14.5). `gc`: evicts My Files' oldest blocks down to the target (default: as far as takes the profile's total to 80 % of `cache.max_bytes`, which without further trees is 80 % of the cap) and prints what it freed. `wipe`: evicts every block that may go, also from RAM, in the cache of every mounted tree (My Files, Photos, each share), or with a path only the blocks of files at or below it, in the tree of the mount the path is in (M6.0.10, D136). A relative path is My Files'; an absolute one may be in any mount: below a Photos mount `<year>`, `<year>/<month>` or `<year>/<month>/<name>` (a year or month wipes every named photo below it), below a Shared mount `<share>/<path>` (the files folder and a shared file below it wipe the shared files' trees; the mount root all its trees). A path outside every mount, or one that names nothing, is refused. The reply gives the total, what each tree freed (named as `signatures list` names its root) and what is left of the profile's cache. Pinned and protected blocks stay (§6.2) |
| `stats [--json] [--reset]` | Counters, gauges and latency histograms (§5.8) |
| `reset --keep-login [--orphan-trees]` | Rebuild local state (refused while the queue is non-empty, while mounted, and while the daemon is still shutting down). `--orphan-trees`: further trees with changes not uploaded have their state moved to `orphaned/` instead of refusing (§13.1, D118) |
| `trash empty --yes` | Delete everything in the account's trash for good (Proton only; refused while the profile is mounted or its daemon is still shutting down, since the daemon holds the session). Real-account test runs use it when storage runs low (D036) |
| `doctor` | Check `/dev/fuse`, fusermount3, `/etc/fuse.conf`, keyring (reachable, default collection locked or not, read without prompting; the effective secret storage mode), config, login, clock skew, disk space, pdmount and app version; the clock line (M6.0.7, D133) gives the device clock's difference to the Proton API's `Date` (`WARN` beyond a minute) and the SDK's server time offset (§6.3): the running daemon's (`status` reply field `server_time_offset`; "the mount has adopted no server time offset" before its first Drive API answer, on the fake backend and from older daemons), `WARN` when it is more than a minute from what the check measured, or, without a daemon, the one the SDK adopts from the check's own answer ("refused" beyond a day); says whether autostart is enabled for the profile and whether a user service is installed, and warns when both are (M5.3, D093); for an installed service it warns when the stop timeout in its settings (or, on systemd, its mountpoint) differs from the config and when logind (elogind's `InhibitDelayMaxSec`) or `user@.service` would cut the flush short (M5.4, M5.5, D091, D094). M5.16 (D100, D101): for an installed service and a terminal with a session bus, the `desktop bus` line compares the terminal's bus with the service's (`unix:path=$XDG_RUNTIME_DIR/bus`) by bus ID (`GetId`): `ok` "same as the service's"; `ok` "attached" when the daemon (`status` reply field `attached_bus`) has this terminal's bus; `WARN` "not attached" (with the hint to log in again or run `pdmount session attach`) when it has none, has another one, has one that no longer answers, or no daemon runs; `WARN` when either bus cannot be asked. While a daemon runs and the buses differ, the line adds whether the Secret Service and the notification server are owned, activatable only or absent on the service's bus. No line without a service or without `DBUS_SESSION_BUS_ADDRESS` in the terminal. The keyring line names the bus it used when there was a choice ("on the service's session bus (…)" or "on this terminal's session bus", D102 amended 2026-11-14: the bus of §14.4 for a process without an attached bus, which the probe uses too) and tells a Secret Service that runs from one that D-Bus started for the probe (`NameHasOwner` first, so that it is known) and from none ("no Secret Service on the session bus", FAIL without a flag) or one that did not start ("the Secret Service did not start"), and a locked collection from an unlocked one, without a prompt. D127: a `mountpoint` line, `WARN` when another profile's config puts My Files or a root at one of this profile's mountpoints (§14.4, *Profiles and mountpoints*). M5.8: one `root` line per `[[roots]]` entry (none without roots), "<kind> at <mountpoint>: …": `ok` for an empty directory, this profile's live PdMount mount, or a missing folder whose nearest existing parent is writable (the mount creates it); `WARN` for a non-empty directory (its files are hidden while mounted) and a dead PdMount mount (cleared at the next start); `FAIL` for a file, a folder that cannot be read or created, another mount, another profile's live mount, or a mountpoint that overlaps My Files' or another root's. For a `photos` root with `photos.thumbnail_cache` on, the same line checks the thumbnail folder (`$XDG_CACHE_HOME/thumbnails`, else `~/.cache/thumbnails`; D115): `ok` when it is a writable directory or can be made, `WARN` when it is not writable or not a directory, or when it or a size folder (`normal`, `large`, `x-large`) has any group or other permission bit (thumbnails are decrypted pictures); a thumbnail problem is never worse than `WARN`, since the mount works without them. M6.1: a `notifications` line asks this terminal's session bus (3 s) whether `org.freedesktop.Notifications` is owned (`ok`), activatable (`ok`, D-Bus starts it at the first notification) or absent (`WARN`: sign-in prompts are only logged; conflicts are shown once a server appears, §10.8), and `WARN`s without a bus or when the bus does not answer; no `notify-send` is needed |
| `autostart [enable\|disable]` | XDG autostart entry `$XDG_CONFIG_HOME/autostart/pdmount-<profile>.desktop` that runs `pdmount --profile <p> [<secret-storage flag>] mount` at desktop login; without an argument, shows whether it is enabled (D059). The `Exec` line names the running binary, quoted by the Desktop Entry spec. Refused while a user service is installed for the profile (§14.4) |
| `service [install\|uninstall] [--manager openrc\|systemd]` | M5: user service for the profile, OpenRC (M5.4) or systemd (M5.5), with the profile's secret-storage flag as `autostart` does; without an argument, shows its state (D074, D094). The manager is the one found running (`/run/systemd/system`, then `/run/openrc`) unless `--manager` names it. `install` writes the unit files, enables the service and starts it; `uninstall` stops it, disables it and removes the files. Refused while `autostart` is enabled for the profile or a service of the other manager is installed (§14.4) |
| `session attach` | M5.17 (D102): run at login by the service's attach entry; sends `DBUS_SESSION_BUS_ADDRESS` (only `unix:path=` or `unix:abstract=`, checked here and in the daemon) to the profile's daemon for notifications and keyring calls. It also sends `DISPLAY`, `XAUTHORITY` and `WAYLAND_DISPLAY` when set, for the daemon's own bus (D102 amended). It prints one line: kept (with the bus the keyring and notifications now use, D102 amended, and a second line when the daemon set the display on its own bus), already the daemon's own bus (compared by bus ID; an address kept before is cleared), or no daemon running (exit 0). Without a daemon it saves the attach for the daemon's next start and says so ("its next start takes this session's bus"), unless the desktop's bus is the service's or there is no runtime directory (D102 amended 2026-11-14). An unreachable bus is refused (exit 1). Every attach makes a daemon that waits for the keyring try it again (D106). A kept bus is saved, so a restarted daemon uses it without a new attach (D102 amended) (§14.4, "`pdmount service`") |

### 14.3 Control socket

JSON-lines request/response over a Unix socket at `$XDG_RUNTIME_DIR/pdmount/<profile>.sock` (0600, peer uid checked). `access`, `conflicts`, `pin`, `unpin`,
`queue`, `sync`, `cache`, `signatures`, `stats`, `session attach` (`AttachBus`) and `unmount` work through it; `login` and `status` use it when a daemon answers, and `mount` to see
that one is ready or already runs. `logout`, `doctor`, `autostart`, `reset` and `trash empty` do their work without it; the last two use it only to refuse while a daemon runs.

### 14.4 Unmount and hang safety

- No `AutoUnmount` unless `user_allow_other` is present (`fuser/session.rs:163-171`). `doctor` explains the `/etc/fuse.conf` change.
- **Autostart (D059):** no service manager supervises the daemon. A crashed daemon is not restarted; its stale mount is
  cleared with `unmount --force` (below). At logout the daemon ends with the session's SIGTERM if the session manager
  sends one, and otherwise keeps running until `unmount`.
- **User service (M5, D074):** an OpenRC user service or a systemd user unit supervises `-v mount --foreground --service <manager> --clear-stale` (M5.4: with `--notify-fd 3` on OpenRC; `-v` logs at INFO as a background `mount` does, so the flush at logout and shutdown is in the log), restarts it after a
  crash, clears a stale mount at start, and stops it with SIGTERM at session end or shutdown, with a stop timeout of
  `shutdown_grace` + 30 s. The systemd unit is ordered `After=dbus.service`, so the session bus outlives the daemon (D091).
  What the systems allow (M5.0): OpenRC gives the full time at logout, but a shutdown kills user services about 16 s after
  their SIGTERM; systemd caps user units at the stop timeout of `user@.service` (120 s), and on the Debian machine a logout
  did not stop the user manager.
- **`pdmount service` (M5.4, M5.5, D094):** the files are written under `$XDG_CONFIG_HOME`.
  - *OpenRC:* `rc/init.d/pdmount` (shared: `supervise-daemon`, `notify="fd:3"`, `need dbus`,
    `DBUS_SESSION_BUS_ADDRESS=unix:path=$XDG_RUNTIME_DIR/bus`, `respawn_delay=5 respawn_max=3 respawn_period=60`, the command line
    of M5.3, with `$pdmount_mount_args` after `mount`), the link `rc/init.d/pdmount.<profile>` to it, and `rc/conf.d/pdmount.<profile>` with `pdmount_secrets`,
    `pdmount_mount_args` (empty, see below),
    `retry="TERM/<grace+30>/KILL/5"` and `output_log`/`error_log` set to the profile's log file (created 0600). Then
    `rc-update --user add pdmount.<profile> default` and `rc-service --user pdmount.<profile> start`. The script refuses to start
    under a name that is not `pdmount.<profile>`, or without the profile's settings, and defines `start()` for failed starts
    (below, D109).
  - *systemd (M5.5):* the template `systemd/user/pdmount@.service` (shared: `Type=notify`, `Restart=on-failure`, `RestartSec=5`,
    `RestartPreventExitStatus=78` (D109, below), `After=dbus.socket dbus.service`, `WantedBy=default.target`, `ExecStart=<binary> --profile %i $PDMOUNT_SECRETS mount $PDMOUNT_MOUNT_ARGS --foreground
    --service systemd --clear-stale`; `%i`, not `%I`, which would turn a `-` in the profile into `/`) and the drop-in
    `pdmount@<profile>.service.d/pdmount.conf` with `Environment=PDMOUNT_SECRETS=<flag>` (when the profile uses one),
    `TimeoutStopSec=<grace+30>` and `ExecStopPost=-<binary> --profile %i unmount --if-stale <mountpoint>` (the configured mountpoint;
    it detaches the mount only by M5.3's stale rule, see below). Then
    `systemctl --user enable --now pdmount@<profile>`; when a daemon that no service runs is running, `enable` without `--now` (OpenRC: no
    `start`), with a note, because the unit's start would fail over that daemon's mount. `uninstall` runs `disable --now`, removes the enable links, the drop-in and, when no
    other profile is enabled, the template, then `daemon-reload`. A running daemon keeps its old unit until `systemctl --user restart`.
    `doctor` warns when the drop-in's stop timeout or mountpoint differs from the config, and when `shutdown_grace` + 30 s exceeds
    `user@.service`'s `TimeoutStopSec` (read from the unit and its drop-ins in `/lib`, `/usr/lib`, `/usr/local/lib`, `/run` and `/etc`;
    120 s, said to be assumed, when none sets it; `DefaultTimeoutStopSec` is not read).
  - *Test builds (feature `fake-backend`, D094 amended for M5.6):* `service install --backend fake:<dir>` (hidden; `<dir>` must exist, no
    quote or backslash in it) runs the unit's daemon on a fake drive, for M5.6's manual runs: `pdmount_mount_args=--backend=fake:<dir>`
    in `conf.d` or `Environment=PDMOUNT_MOUNT_ARGS=…` in the drop-in, both quoted for the target; `pdmount service` shows it. Release builds do not have the option.
  - *Desktop bus (M5.17, D102):* `install` also writes `$XDG_CONFIG_HOME/autostart/pdmount-<profile>-attach.desktop`
    (`<binary> --profile <p> session attach`, no secret flag; written by `autostart.rs`'s entry writer), `uninstall` removes it,
    and `service` shows it. It is not an autostart entry for D059/D093: `autostart`'s state, `autostart enable`'s refusal and
    `doctor`'s "both" check count only a `pdmount-<profile>.desktop` whose `Exec` does not end in `session attach` (the
    profile `p-attach`'s autostart entry has the name of `p`'s attach entry; neither replaces nor counts as the other). At
    each login it sends the desktop's `DBUS_SESSION_BUS_ADDRESS` to the daemon (control request `AttachBus`). The daemon
    accepts only `unix:path=` and `unix:abstract=` addresses, connects (zbus, 2 s), checks that the bus is its own user's
    (the socket's peer uid, `SO_PEERCRED`; D102 amended) and compares the bus ID
    (`org.freedesktop.DBus.GetId`) with its own bus's: equal, it keeps nothing (and clears an address kept before); different,
    it keeps the address (process-wide, `bus.rs`) and logs it at INFO; unreachable or another user's, it refuses with the
    error.
    *Per service (D102 amended after the Gentoo check):* the keyring and the notification server need not share a bus
    (on a Gentoo system with i3, PAM's gnome-keyring runs on the service's bus and the notification server on the
    desktop's; M5.6 had the opposite). With an address kept, `bus::route` picks the bus for each service, by its name
    (`org.freedesktop.secrets`, `org.freedesktop.Notifications`): (1) the attached bus if the name is owned there,
    (2) else the own bus if owned there, (3) else the attached bus if activatable there, (4) else the own bus if
    activatable, (5) else none (keyring unavailable at once, no notification sent). Owned beats activatable, since an
    activatable service on the wrong bus may never answer (D101). The checks are `NameHasOwner`, then
    `ListActivatableNames` only for a name without owner (zbus, 2 s, on connections that pass the user check; the own
    bus is asked only when the attached one does not own the name), cached per bus and name for 30 s; an attach, a
    dropped address, a failed keyring call and a failed notification call clear the cache. A change is logged at INFO
    (`the keyring: using the daemon's own session bus`, `notifications: using the desktop's session bus`); the attach
    reply and `session attach` name each service's bus, and `status` shows them as `Bus: desktop's bus attached;
    keyring: …, notifications: …` while an address is kept (from the last choice, no D-Bus call). Without an address
    kept, notifications use the process's own bus; the keyring's bus is chosen between the own bus and the service's
    (next paragraph); on the own bus its calls are gated by the same owned/activatable check (D101, §12.4).
    *Without an attached bus (D102 amended 2026-11-14, after the M5.16 check):* a command run in the desktop's
    terminal without a daemon (`doctor`, `status`, `login`, `trash`), or one that reads the keyring itself, has the
    desktop's bus as its own, where on the Gentoo system with i3 gnome-keyring is only activatable and never answers,
    while PAM's keyring runs on the service's bus. Such a process (the daemon too, without an attach) chooses the
    keyring's bus with `bus::keyring_buses`: its own bus (`session_address`) and, unless the own bus owns
    `org.freedesktop.secrets`, the service's bus `unix:path=$XDG_RUNTIME_DIR/bus` when its socket exists and its bus ID
    (cached 30 s with the name states) differs from the own bus's or the own bus cannot be asked. Order
    (`bus::choose_without_attach`): (1) own bus owned, (2) service's bus owned, (3) own bus activatable, (4) service's
    bus activatable, (5) none, unavailable at once with both buses named. The service's bus is used like an attached
    one: its keyring calls run in the keyring helper with that address, the bus checked (`GetId`, the peer's uid) just
    before each; a failure clears the cache, nothing is dropped. For a daemon run by the service the two buses are one,
    so nothing changes there. The choice is logged at INFO when it changes (`the keyring: using the service's session
    bus`).
    Notifications are sent by the daemon itself, over D-Bus (M6.1, D138): one zbus connection per bus address, kept on a
    runtime of its own and made again after a failure, opened through the same user check as every connection here (the
    peer's uid). On the attached bus the keyring calls run in the helper child of §12.4; a chosen attached bus is
    checked again just before (a bus that fails it is dropped: once the desktop's bus has exited, another local user
    could listen at its address). On the own bus the keyring calls run as without an attach. A failed call on the kept bus drops the address when the bus no longer
    answers `GetId` (logout). Every attach resets the keyring's unavailable flag (`retry_keyring`) and asks a waiting
    daemon to try the keyring (D106). The desktop must not be moved onto the service's bus: on Gentoo with SDDM that
    broke udisks mounting (polkit "not permitted", M5.6).
  - **The notification client (M6.1, D138; `crates/pdmount/src/notify.rs`).** `org.freedesktop.Notifications` through zbus,
    on the bus the route above picks (`notify::target()`: the attached bus or the process's own); `notify-send` is gone, so no text
    of a notification is in a process's arguments. One connection per bus address, on a runtime of its own (the callers'
    runtimes are short-lived), reused, made again after a failure (a send or a close that fails on an old connection is
    retried once on a new one), and made outside the lock of the table of connections, so a bus that does not answer holds up
    no call on another; every call has the 2 s deadline of the other bus calls. `GetCapabilities` is asked once per connection,
    and again after the service's `NameOwnerChanged` (another server, or one restarted with another configuration, M6.5 V-4):
    buttons are offered only with `actions`, and the body is escaped (`&`, `<`, `>`) only with `body-markup` (names come from
    other devices, D070). Every name in a notification (a file's, a tree's or share's, a device's) is first cleaned by one
    rule, `notify::clean_name`: control characters (Cc), U+2028 and U+2029, the bidirectional formatting characters (U+061C,
    U+200E, U+200F, U+202A–U+202E, U+2066–U+2069), the invisible ones (U+00AD, U+034F, U+180B–U+180E, U+200B–U+200D,
    U+2060–U+206F, U+FEFF, U+FFF9–U+FFFB, the tag characters U+E0000–U+E007F, the variation selectors U+E0100–U+E01EF), the
    blanks that are letters (U+115F, U+1160, U+3164, U+FFA0, U+2800) and the slash look-alikes (U+2044, U+2215, U+29F8,
    U+FF0F) become U+FFFD, so a name cannot split the text, reorder it ("invoice", U+202E, "fdp.exe" would read
    "invoiceexe.pdf"), hide its end or read as a path (M6.5 V-3); emoji's variation selectors (U+FE00–U+FE0F) stay. U+FFFD
    rather than nothing or a space, so the reader sees that something was there. What a server does with plain text is its
    own: GNOME Shell and dunst make links of address-like text in a body (M6.5 V-5, open). Its callers: conflicts (§10.8), slow downloads (below), pending markers of other devices (below) and the
    notices about a further root's share (§14.5). No bus, or no server that owns or can start its name: nothing fails or
    waits, and the first failure is logged once at INFO (later ones at debug level, until a notification goes out). The API for the tasks that act on a
    notification: `send(&Notification) -> Option<Handle>` (summary, body, actions, `replaces`, expiry; the handle is the
    bus and the id, since ids belong to one server), `close(&Handle)`, `capabilities()`, `subscribe()` (a broadcast of
    `Event::Action` for `ActionInvoked` and `Event::Closed` for `NotificationClosed`, only for ids this process sent),
    and the callers' `desktop` (queued, sent in order), `desktop_sent` (waits at most 10 s, true when the server took
    it) and `deliverable` (the server owns or can start its name). That a server appeared is learned by
    `notify_watch.rs` (`NameOwnerChanged`, §10.8, M6.3).
  - **The slow-download notification (M6.4, D137; `crates/pdmount/src/downloads.rs`).** The engine measures (§6.4), the
    daemon notifies. With `notify.downloads` (default `true`) the daemon subscribes to the session's slow reads at the
    mount; with `false` it does not, and no timer runs. A file whose read waited 5 s gets "PdMount: downloading", "<name>
    (633 MiB) is downloading; it opens as its blocks arrive.": the name, not the path (cleaned by `notify::clean_name`,
    above, and escaped for `body-markup`, as names come from other devices, D070), and the size (one decimal under 10).
    At most one per file per 10 minutes (a second slow read of it inside them sends nothing, also when the first
    notification was summed), and three per 30 s through `notify::Burst`, as conflicts (§10.8): beyond three, files wait
    and go out in one sum when the window has room, "N more files downloading; they open as their blocks arrive.", which
    counts as one of the three; once one waits, later ones wait with it. The sum counts the held files whose waits have
    not ended (none left: nothing is sent, and the window keeps its place) and replaces the sum still open. A send the
    server did not take neither names the file nor uses up the window. A notification is closed (`CloseNotification`)
    when its wait ends, the sum when all of its waits have; a server that dropped it already is no error. When reports
    were missed (the broadcast lagged), the notifications still open are closed and the held files dropped, as their
    ends may be among the missed. At the daemon's stop a shutdown signal makes the announcer close what is open, beside
    the stop's other work and waited for at most 3 s.
    *The desktop's display (D102 amended after the second Gentoo check):* a service on the daemon's own bus may start
    programs by D-Bus activation that need the display, above all gnome-keyring's unlock dialog (`gcr-prompter`), and
    the OpenRC user bus's activation environment has no `DISPLAY`. `session attach` therefore also sends those of
    `DISPLAY`, `XAUTHORITY` and `WAYLAND_DISPLAY` that are set and non-empty (field `display`, absent from older
    clients). The daemon keeps only these names with a value of at most 4096 bytes without control characters, and
    logs the others at WARN without failing the attach (`bus::checked_display`). When the attach kept the address and
    the keyring or notifications go to the own bus (its choice is the own bus; a route that fell back to the own bus
    because the attached one was dropped meanwhile counts too), it calls `org.freedesktop.DBus.UpdateActivationEnvironment`
    on its own bus with them (zbus, 2 s, the user check above), before the keyring retry, so that the prompter and an
    activatable notification server open on the desktop. Its own environment is not changed, and variables missing at
    the attach are not removed from the bus; the last attach wins. Success is logged at INFO with the values, a
    failure at WARN; neither changes the attach's result. The reply lists the names set (`display`, absent from older
    daemons), and `session attach` adds a line when there are any.
    *Across restarts (D102 amended after the third Gentoo check):* an attach that keeps an address also saves it, with
    the checked display variables, as `{"address": …, "display": {…}}` in `$XDG_RUNTIME_DIR/pdmount/<profile>.attach`
    (`ProfilePaths::attach_file`; mode 0600, a temporary file renamed over it, written under the lock of the kept
    address, `bus::keep`); without a runtime directory nothing is saved. Every place that clears the kept address
    removes the file: an attach of the daemon's own bus, and a drop of an address whose bus is gone or fails the user
    check (`bus::forget`). A daemon's start (`keywait::restore_saved_attach`, in `run_foreground` before the start's
    keyring guard and first keyring call) reads it: opened with `O_NOFOLLOW | O_NONBLOCK`, then on the open file a
    regular file of the daemon's uid of at most 16 KiB holding that JSON. The address then passes the attach's checks
    (`bus::validate`; connect with the peer uid check; a bus ID different from the own bus's), and the display the
    same check and the same rule for the own bus; the address is kept without a keyring retry (nothing has tried the
    keyring yet), logged at INFO ("using the desktop's session bus saved by the last attach"). A file that fails any
    check is removed (a symlink itself, never its target) and logged at INFO with the reason. The file stays while it
    is used, so a later restart uses it too; the runtime directory ends with the user's last session.
    *An attach before the daemon is up (D102 amended 2026-11-14):* at a login the OpenRC service and the window
    manager's `session attach` start together, and the attach may find no daemon. `session attach` then saves the
    attach itself (`keywait::save_for_next_start`: the same file, writer and display check), unless there is no
    runtime directory or the desktop's bus is the service's (`$XDG_RUNTIME_DIR/bus` with the same bus ID; a service's
    bus that does not answer counts as another, the daemon checks at its start); a desktop's bus that does not answer
    is refused (exit 1). It then asks the daemon once more and, if one answers now, sends the attach as usual. A daemon
    that starts meanwhile reads the file at its start as above, and once more just after its control socket listens
    (`keywait::restore_late_attach`: an address not yet kept is taken, with the same checks, logged at INFO, and the
    keyring is tried again), so an attach saved after the start's read and before the socket listens is not lost.
    *Desktops without XDG autostart (D102 amended):* i3, sway and other plain window managers do not run the attach
    entry. When `XDG_CURRENT_DESKTOP` names `i3` or `sway` (`service.rs` `NO_XDG_AUTOSTART`), or is unset, `install` and
    `service` print a note with the line for its configuration: `exec --no-startup-id <binary> --profile <p> session
    attach` (i3, `~/.config/i3/config`) or `exec <binary> --profile <p> session attach` (sway), the binary and profile
    quoted for `sh -c`; unset, both lines as examples. The entry is still written (for `dex -a`); the window manager's
    configuration is never edited.
  - *Start without the keyring (D106):* a service daemon of a profile mounted before mounts at once when the keyring does
    not answer, signals readiness and finishes its start when the keyring answers (§12.3). Waiting before the mount was
    rejected: OpenRC's `supervise-daemon --notify` waits for readiness without a limit, so the service's start and the user
    runlevel would block, systemd fails a `Type=notify` start after 90 s and restarts it, and the empty mountpoint would take
    applications' writes. For the same reason the start under `--service` probes the collection first and reads nothing
    from a locked one, and spends 30 s at most on the keyring (§12.3), so the units keep systemd's default start timeout.
  - *Failed starts (D109):* a start that fails before the daemon reports ready is final for both managers; a daemon that
    exits after readiness (a crash) is restarted. Causes left after D106: a broken config, a profile never signed in or
    never mounted, preflight refusals, a FUSE mount failure, offline on a profile never mounted. The user starts the
    service again once the cause is fixed; the error is in the log and in `rc-service`'s output or the journal.
    - *Exit code:* under `--service`, a failure before readiness (from the command line that does not parse, through
      the config, to the mount) exits 78 (`EX_CONFIG`) instead of 1 and says so on stderr (`supervise::failure_exit`);
      after readiness a failure exits 1. The systemd unit's `RestartPreventExitStatus=78` keeps `Restart=on-failure` from
      retrying a failed start every 5 s without end. OpenRC ignores the code.
    - *OpenRC:* `supervise-daemon --start` waits for the readiness newline; when the daemon exits first, the start fails,
      but the supervisor it forked keeps respawning the daemon (exit codes are ignored; `respawn_max` only bounds a
      burst), and a respawn that succeeds runs while the service reads "stopped" (M5.6 finding 5). The script's
      `start()` runs `default_start`; when that fails, it waits up to 2 s for the supervisor's pid file
      (`$XDG_RUNTIME_DIR/supervise-<service>.pid` for a user service, `/var/run/` otherwise, as `supervise-daemon` picks
      it) and stops that supervisor with `supervise-daemon <service> --stop`, then returns 1. A failed start takes
      about 5 s (the supervisor's `respawn_delay` sleep). A supervisor whose pid file names a live process before the
      start (a stray of a failure before D109) is not stopped: the start fails with "already running" as before.
    - *A stray:* `status` (on stderr, also when the daemon does not answer) and `doctor` (the `service` check) warn when
      the profile's daemon runs under `--service openrc` (its pid file) while OpenRC holds `pdmount.<profile>` as stopped:
      no entry for it in `started`, `starting`, `stopping` or `inactive` of `$XDG_RUNTIME_DIR/openrc` (file lookups, not
      `rc-service … status`, which runs the script and may rebuild the dependency cache). The warning names the recovery:
      `RC_USER_SERVICES=yes RC_SVCNAME=pdmount.<profile> supervise-daemon pdmount.<profile> -U --stop` (the daemon
      flushes and unmounts as at any stop), then `rc-service --user pdmount.<profile> start`. `-U` selects user mode:
      OpenRC 0.64 no longer takes it from `RC_USER_SERVICES` in `supervise-daemon`; 0.62 takes either.
    - *Tests:* the goldens and the `the_template_uses_…` unit test; `supervise.rs` unit tests for the exit code, the
      stray check and the descriptor; `openrc_a_failed_start_leaves_no_supervisor` in `crates/pdmount/tests/service.rs`
      runs the generated script on the installed OpenRC in a sandbox (temporary `XDG_CONFIG_HOME` and `XDG_RUNTIME_DIR`, a
      stand-in daemon and `dbus` service; skipped without OpenRC): a failed start leaves no supervisor and reads stopped,
      the next start and a stop work, a stray (made with `zap`) survives a start and draws `status`'s warning, and the
      named command stops it. `svc.sh` checks exit 78 and a descriptor that is not a pipe.
  - *elogind (D091):* `install` prints the root file `/etc/elogind/logind.conf.d/pdmount.conf` (`[Login]`,
    `InhibitDelayMaxSec=<grace>`) unless the effective limit already reaches the grace; nothing under `/etc` is written. `doctor`
    reads elogind's configuration (the main file and the `logind.conf.d` directories, layered by file name) and warns when the
    limit is below the grace.
  - `doctor` also warns when the stop timeout in the profile's settings differs from `timeouts.shutdown_grace` + 30 s; `service install`
    rewrites it.
  - Profile names are validated (§13.1); the binary, the mountpoint and the log path are quoted for the target syntax (`%` and `$`
    doubled for systemd) and refused with a control character. The tools (`rc-update`, `rc-service`, `systemctl`) run without a shell,
    found on `$PATH`.
- **Under a supervisor (M5.3, D093):** the options of `mount --foreground` that unit files pass.
  - *Readiness:* once the mount answers (the control socket serves), the daemon sends `READY=1` as one datagram to
    `$NOTIFY_SOCKET` (a path, or `@name` for an abstract socket; systemd's `Type=notify`, OpenRC's `notify="socket:ready"`),
    and with `--notify-fd <n>` writes a newline to descriptor `n` and closes it (OpenRC's `notify="fd:3"`). Only a pipe
    is written and closed (D109): a daemon that `supervise-daemon` respawns has no pipe, and descriptor 3 may then be its
    log, which is left as it is. A failure is logged and never stops the daemon. A background `mount` removes `NOTIFY_SOCKET` from its daemon's environment.
  - *Stale mount:* `--clear-stale` runs the stale-mount check and the lazy unmount of `unmount --force` before anything else,
    only for a pdmount mount, only when no daemon of the profile answers on its control socket and none runs (pid
    file), and only when the mount is dead (D127): `statfs` of the mountpoint answers ENOTCONN (or ECONNABORTED after an
    abort), because no process holds its FUSE connection any more. `statfs` always reaches the daemon (`stat` may answer from
    the kernel's attribute cache); a mount that answers, fails otherwise or does not answer within 2 s (a hung daemon) is
    live. A mount held by a live daemon, slow or shutting down, is never touched; the start then fails as without the option.
    Another profile's live mount there fails the start: see *Profiles and mountpoints* below.
    The further roots' mountpoints (§14.5) get the same rule, since a crashed daemon leaves them mounted too (D118).
  - *`unmount`:* `--service <openrc|systemd>` says that a service manager runs the daemon; the daemon records it as a third
    line of its pid file (`service openrc`). `supervise-daemon` restarts a daemon after exit 0 as well as after a crash,
    and systemd's `Restart=on-failure` does not, so exit codes cannot keep it stopped on OpenRC. A plain `unmount` therefore
    refuses (exit 1) and names `rc-service --user pdmount.<profile> stop` or `systemctl --user stop pdmount@<profile>`.
    `unmount --force` is the recovery path: it detaches and stops the daemon. What follows differs by manager, and so do the
    refusal and the note: OpenRC restarts the daemon (`stop` keeps it stopped); the systemd unit stays stopped, since a daemon
    that exits 0 is not restarted by `Restart=on-failure` (one that had to be killed is), until `systemctl --user start
    pdmount@<profile>`, and stays enabled. The refusal of a plain `unmount` stays for both: it is uniform, it keeps the
    unit's state and the daemon's in step, and it holds when a user's drop-in sets another `Restart=`.
    A dead daemon's stale pid file names no service, so a plain `unmount` removes a stale mount as before.
  - *`unmount --if-stale`:* applies the stale rule above and nothing else: a dead pdmount mount that no daemon of the
    profile answers for and none runs behind is detached with `fusermount3 -uz`; a live daemon's mount, a mount of another
    kind and no mount are left alone, exit 0. Another profile's live mount is left alone with an error, exit 1 (D127; the
    unit's `ExecStopPost=-` ignores the code). The systemd drop-in runs it as `ExecStopPost=`, which systemd also runs
    after a failed start (a hand-started daemon holds the profile; the unit's daemon fails in `preflight`), so a bare
    `fusermount3 -uz` there would detach that live daemon's mount. At `ExecStopPost=` the unit's own daemon is gone, so a mount
    left by its crash is still cleared. The further roots' mountpoints get the same rule (D118). Excludes `--force` and `--wait`.
  - *Double start:* an autostart entry and a user service for one profile would both start the daemon. `autostart enable`
    refuses while a service is installed (OpenRC: `$XDG_CONFIG_HOME/rc/init.d/pdmount.<profile>`; systemd:
    `pdmount@.service` in `$XDG_CONFIG_HOME/systemd/user` enabled for the profile, a link in any `*.wants` directory
    there); `service install` refuses while the autostart entry exists; `doctor` warns when both are set.
- **Profiles and mountpoints (D127):** every profile defaults to `~/ProtonDrive`, so two profiles can name one mountpoint.
  - *Whose a mount is:* the daemon mounts My Files and its further roots with `fsname=pdmount:<profile>` and, as before,
    `subtype=pdmount`: the type in `/proc/self/mountinfo` stays `fuse.pdmount`, and its source field (and the first column
    of `df`, the first field of `/proc/mounts`) is `pdmount:<profile>` instead of `pdmount`. A mount of an earlier version
    has the plain source and names nobody.
  - *Never another profile's live mount:* a live pdmount mount at the mountpoint is this profile's when its source names the
    profile (a plain source: when a daemon of the profile answers or runs). Any other live one is another profile's and is
    never detached: `--clear-stale` fails the start (`preflight` too; under a service before readiness, exit 78, D109),
    `mount`, `unmount --if-stale` and a plain `unmount` without a daemon refuse with "another PdMount mount (profile
    <owner>) is at <path>; give profile <p> its own `mountpoint` in its config (D127)", exit 1; `unmount --force` refuses
    one whose source names another profile and names `pdmount --profile <owner> unmount --force` (a plain source stays the
    recovery path). A further root's mountpoint (§14.5) and `unmount <root mountpoint>` follow the same rule. `status`
    without a daemon shows "Mount: not mounted; another PdMount mount (profile <owner>) is at the mountpoint". A dead mount
    of any profile is stale for every profile and is cleared as above.
  - *Before it happens:* `service install` refuses while another profile's config puts My Files or a root at one of this
    profile's mountpoints (a service would fail every start), and `doctor`'s `mountpoint` line warns about it. Profiles
    counted are those with a config file (`$XDG_CONFIG_HOME/pdmount/<p>.toml`) or a data directory
    (`$XDG_DATA_HOME/pdmount/<p>/`); a config that does not load is skipped.
- **Flush (M5.1, D075, D092):** SIGTERM, SIGINT or SIGHUP uploads what is queued at that moment before the stop below
  (`crates/pdmount/src/flush.rs`):
  1. `syncfs` on the mount, on a thread of its own with a 5 s limit: pages dirtied through a shared writable mmap reach the
     daemon (`write()` already has, D083).
  2. A barrier: the journal entries that are not parked. They are forced without the debounce (`Engine::flush_barrier`,
     `flush_to`), but an entry in its back-off after a failure, HTTP 429 included, waits for its end. What they wait for (a
     parent folder, a name holder) is forced too, as in `sync`.
  3. Wait until none of them can finish any more (uploaded, or parked or held up by a parked entry), the drive cannot be
     reached (offline or `needs_login`: no wait at all), or `timeouts.shutdown_grace` (90 s) has passed. New writes are
     accepted and uploaded as usual, but the wait does not include them.
  4. The log says "flushed N entries in T s", or that the grace ran out ("1 entry" for one); a signal that comes after the
     grace ran out at PrepareForShutdown starts a flush with no grace and says "no grace left: it was used at
     PrepareForShutdown; stopping with N entries left", not "ran out" a second time. The last line of the stop says
     "N entries left for the next mount" when the queue is not empty.

  The grace counts from the first trigger and covers the flush and the wait for running steps below, so a stop takes at most
  the grace, the session's 10 s and the commit. A second signal ends the wait at once, and the stop below waits for nothing:
  the logind lock is released, the session gets at most 0.3 s, running steps are cut off (retried at the next mount), and the
  store is committed, well inside the second that OpenRC's shutdown leaves between its second TERM and KILL. A signal that comes
  during the stop itself (after the flush, an `unmount` request or an external unmount) ends its waits the same way.
  `unmount` and an external unmount do not flush (`unmount` drains first, for up to 10 minutes). The next mount warns if entries were left (M5.2, D095). With D076, a flush
  that runs out of time leaves `.pdmount/<device.id>.pending` in the root of each tree (§14.5) that has entries left and has the
  marker turned on, and other devices and members with that tree mounted show it.
- **Shutdown through logind (M5.1, D091, D092):** while mounted, the daemon holds a logind delay lock for `shutdown`
  (`Inhibit("shutdown", …, "delay")` on the system bus, through `zbus`; the lock is the returned descriptor, close-on-exec) and
  watches PrepareForShutdown, counting only signals from the owner of `org.freedesktop.login1` (`crates/pdmount/src/logind.rs`).
  PrepareForShutdown(true) starts the flush above, and the lock is released when the flush ends, at the end of the grace, at
  a second signal, or at once with an empty queue, offline or in `needs_login`. After an `unmount` request or an external
  unmount during the flush it is held until the stop's wait for running steps ends, within the same grace. The daemon stays mounted after it, until the SIGTERM of the
  shutdown; a SIGTERM during the flush continues it and the daemon stops at its end. PrepareForShutdown(false) forgets the
  shutdown, ends a running flush's wait and takes a new lock. logind sends it whenever a delayed shutdown does not go ahead
  (cancelled, or its job failed); without it the start of the shutdown would stay set, and a later stop would find its grace
  used up. The grace counts from the first of PrepareForShutdown and
  SIGTERM. A non-empty barrier gets the notification "PdMount: uploading N changes before shutdown" (over D-Bus, never
  waited for). logind holds a shutdown for at most `InhibitDelayMaxSec` (5 s by default; root can raise it): `pdmount service
  install` prints the drop-in that raises it to the grace, and `doctor` warns when it is lower, or on systemd when
  `shutdown_grace` + 30 s exceeds the stop timeout of `user@.service`. Without a system bus or logind, with the lock refused, or
  for root's `poweroff` on OpenRC, SIGTERM alone starts the flush; the daemon logs which once.
- **Warning at the next mount (M5.2, D095; `crates/pdmount/src/resume.rs`):** the journal is read when the engine opens, before
  any step runs, so a clean stop and a crash look the same. A non-empty queue gets a WARN, "N changes from the last session were
  not uploaded", and, for the entries that are not parked, the notification "PdMount: N changes from the last session are still
  uploading" (as above). When none of those entries is left to upload, the log says so and a second
  notification, "the N changes from the last session are uploaded", goes out, but only if the notification server took
  the first one within 10 s (`notify::desktop_sent`; none without a session bus or a server). Entries that were parked meanwhile are counted out: "K of N … uploaded",
  with the rest named for `queue list`. Parked entries do not upload by themselves, so they are in the WARN's count and in
  `status`, but make no "still uploading" notification; a queue with only parked entries sends none. An empty queue gives no
  line and no notification. The watch looks at the queue every 2 s and changes nothing about what uploads or when.
- **Pending marker (M5.7, D076, D096):** per tree, in the engine (`crates/pdmount-core/src/engine/marker.rs`); `<root>` is the
  tree's root and the counts are that engine's.
  - *Writing* (opt-in, `sync.pending_marker`, default off): a flush whose barrier is still moving 10 s before the end of the
    grace writes `<root>/.pdmount/<device id>.pending` then, straight through the backend, not the journal, bounded by the
    deadline, and goes on flushing until the deadline. When the barrier is then reached and the queue is empty, the flush
    trashes the marker before it ends, within the grace, since the drain watcher below stops with the daemon. The folder is created when missing; an existing marker gets a new
    revision. The content is JSON, `{"version":1,"device":"<device name>","time":<unix s>,"entries":<n>,"bytes":<n>}`: the
    queue's entries (pending and parked) and the content bytes still to upload when it is written, no file names (mode 0444,
    `application/json`). Nothing is written when the barrier is reached or parked before that point, offline, in
    `needs_login`, or after a second signal. The marker's link is kept in the store (`meta.pending_marker`).
  - *Trash:* once the engine's queue is empty, in the same run (the rest of the grace, or after a PrepareForShutdown flush)
    or the next, the marker is trashed (`Backend::trash`; a marker that is gone counts as trashed). Parked entries keep it.
    A store that lost the link (`reset`) finds the marker in the listing of `.pdmount`. Before the trash the link is
    checked to be still a file named `<device id>.pending` whose parent is the root's `.pdmount`; a link adopted from a
    listing that was renamed or moved since is forgotten, not trashed (D126).
  - *Reading* (on in every tree whose write gate is open, D126): after each event pass, and at least every 2 min, once
    the root is listed (it never lists the root itself), the engine lists `.pdmount` once (events keep it
    current after that, in background turns, D027) and reads each `<id>.pending` of another device that is new or changed,
    at most 16 per pass (`MAX_MARKERS_READ`): devices already shown first, then devices whose markers were read before,
    also in an earlier run, oldest known first (`meta.incoming_devices`, at most 64; when full, a new device takes the
    place of the newest known one not read in that pass; V-12), then by id; the rest are counted (`+N more` in
    `status`, a WARN when their number changes) and not fetched. A tree whose gate is closed (a viewer's share, a tree
    whose write was refused, a retired one) reads none and shows none; what was announced stays remembered.
    The marker is untrusted (D070): its size in the listing must be at most 4 KiB, it must be one block, fetched into a 4 KiB
    buffer; the JSON must be version 1 with typed fields (others are ignored) and a `time` a clock can hold (at most
    `i64::MAX` seconds; ages are computed with checked arithmetic, V-8); the device name loses control and formatting
    characters (bidirectional overrides, zero-width marks; each becomes `?`) and is cut to 64 characters. A marker that fails
    is logged with its device id and ignored until it changes; one whose fetch fails is read again at the next pass, and
    is not announced again if this revision was. Only canonical `<uuid>.pending` names count, and this
    device's own marker is not shown. A pass that panics is logged and the next pass runs.
  - *Shown:* `status` prints `Incoming:   <device>: N changes (<size>), since <time> (<age> ago)`, one line per device,
    `, stale` after 7 days, at most 5 lines per tree and then `+N more` (the rest and the markers not read). Each marker
    revision is announced once (remembered in `meta.incoming_notified`, so a remount does not repeat it; none for a stale
    one), and a pass sends one notification per tree for all it announces (D126): for one marker "PdMount: changes on
    their way from <device>", for several "… from <n> devices: <a>, <b>, <c>, <k> more" (at most three names); a further
    tree adds " in <tree>". The body holds only numbers and a time: the summed changes and size, the oldest marker's
    time, and how many markers were not read.
  - *Read-only in the mount:* `.pdmount` in the root, and everything below it, answer EROFS (D019) whatever the access rules
    say: creating, removing or renaming the name in the root (even before the folder exists), and any create, unlink, rmdir,
    rename into or out of it, setattr, open for writing or `access(W_OK)` inside it. Its attributes show no write bits and
    `user.pdmount.access` says `ro`. So `rsync --delete` on the root cannot remove it and the journal never holds a local change
    there; remote changes there apply as anywhere.
- Shutdown after the flush, on `unmount` or an external unmount (D062):
  1. The journal stops starting steps. New opens are not refused.
  2. Unmount.
  3. Wait for running uploads and inline steps until the grace ends (from the flush's start, or from now for `unmount`), or
     until a second signal.
  4. Commit the store.

  Pending work persists and resumes at the next mount, also an upload the grace cut off.
- **Starting:** `pdmount mount` re-executes itself as `pdmount mount --foreground` in its own process group, with stdout and
  stderr appended to the log file (D026), and returns once the control socket answers `ping` (90 s limit). It refuses to
  start while the profile's daemon answers or while the mountpoint is a mount (a dead pdmount mount is reported as stale,
  another profile's live one as a conflict, D127).
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
  the session, the transfer and rate limits, and one event poller per volume: other users' volumes every 10 min, the one volume whose share was used last through the mount every 30 s until 5 min without a use (D112, as the current official client's `EventScheduler`); share membership, roles and revocations come from the 30 s core event stream.
- **As built (M5.10a, D110):**
  - *Tree:* a backend value for (volume, root): `ProtonBackend::tree` over the session (§3.2). Its engine keeps its state
    under `trees/<key>/` (§13.1), shares My Files' connectivity (§9.6) and gets the profile's config without `[access]`
    rules, `fsync.remote_paths` and `sync.pending_marker`; the engine does not know the tree's kind. Each tree has a
    16 MiB RAM pool; `cache.max_bytes` is one disk cap for the profile, My Files and every further tree together
    (D110 amended): a tree that takes the shared total over the cap evicts its own least recently used blocks, and My
    Files keeps at least half of the cap. The trees share the session's download and upload permits and background
    turns (`EngineOptions::limits`, §6.4 "Per session").
  - *Shared disk cap* (`pdmount_core::cache::DiskBudget`): every open tree's cache reports its unpinned bytes to the
    daemon's one budget and evicts only its own blocks. Shares: My Files half the cap, the further trees the other half
    evenly; they only decide who evicts, and free room is anyone's. When a cache's bytes grow (a block stored, a pin
    let go, the open) and the total is above 90 % of the cap, it makes room down to 80 %: over its share, it evicts its
    own blocks first; then each other tree over its share, the furthest over first, evicts down to its share at most;
    last its own blocks. No other tree takes My Files below half. Pinned blocks stay outside the cap (§6.2). One
    enforcement runs at a time; its lock comes before every cache's index lock and is never taken while one is held.
    A tree's cache counts from its mount to its unmount; an unmounted tree's blocks stay on disk, uncounted, until it
    mounts again. `status` adds `Cache cap:` (the profile's total against the cap) and each tree's own bytes on its
    line; `cache stats` shows My Files' figures and the same total; `cache gc` evicts My Files' blocks (§14.2).
    Without `[[roots]]` My Files' cache is alone on the budget, as before.
  - *Write gate:* §11.2; closed for every further tree (the `test` kind's `writable` opens it).
  - *Router* (`pdmount-fuse`, `Router`): only for a mount with several trees, and always for a Shared mount (below). The top level is inode 1, mode 0555, its
    changes EROFS; inode `n` of tree `i` is `(i + 1) << 48 | n`, so a tree's root is its folder in the top level and the
    `..` of that root is 1. A rename between trees is EXDEV (each tree counts as a file system of its own). A lookup,
    open or opendir in a tree marks its volume as the one in use for the pollers (§9.2). A mount with one tree maps
    nothing.
  - *Pollers:* §9.2. With `[[roots]]`, My Files joins its volume's poller; without, the daemon runs as before.
  - *Daemon* (`crates/pdmount/src/roots.rs`): the further roots mount in the background once the daemon has reported
    ready with My Files, so a slow or failing root never delays or fails the start (D109). A tree that does not open is
    left out of its mount (and named in `status`); a mount whose trees all fail, or that cannot mount, is reported
    failed in `status` and the log; My Files and the other mounts go on. A pdmount mount left at a root's mountpoint by
    a daemon that crashed or was killed (its connection answers ENOTCONN to `statfs`, D127) is detached when the root
    mounts; anything else mounted there fails the root (D118), a live PdMount mount with "a live PdMount mount (profile
    <owner>) is already at <path>".
  - *Stop* (D118): after My Files' flush, the further mounts upload all at once until the flush's deadline, then each is
    unmounted (`fusermount3 -u`, `-uz` when busy) and its trees stopped, all at once: sessions get 5 s, running steps
    the flush's deadline, then the stores are committed. A command still running on a mount ends its step: an
    `unmount`'s drain ends and its mount is stopped the same way, never put back as mounted; a mount still opening
    stops opening further trees and is unmounted and stopped likewise. The stop waits for them until 5 s after the
    deadline, and takes any mount a command left mounted. A gone share's cleanup (below) is waited for within the
    deadline. A second stop signal (D092) ends the drains at once, and the sessions, commands and cleanups get 0.3 s
    and running steps none, so My Files' commit and the tokens' save come well before the supervisor's kill (D092's
    margin of 30 s past the grace). While the start waits for the keyring (D106) the roots
    show as `waits for the keyring` and `mount <mountpoint>` is refused; when the wait ends they mount by themselves in
    the background, as after the readiness report (D110 amended). After such a wait My Files keeps its own event task
    and the further trees get their volumes' pollers. Mountpoints may not overlap My Files' or each other.
  - *Per mount:* `status` adds a `Root:` line per further mount, with its state (`waits for the keyring`, `mounting`,
    `mounted`, `unmounting: uploading first`, `stopping`, `not mounted`, `not mounted (unmounted from outside)`,
    `failed: <why>`) and `read-only`, and a line per tree (gate, queue, last poll, or why it did not open). `pdmount unmount <mountpoint>` of
    a further root uploads its pending work (up to 10 min; the command says how many changes first and waits for it),
    unmounts it and stops its trees (running steps get 10 s, then the stores are committed); the daemon and My Files
    go on, also under a service. A busy mount stays mounted; `--force` detaches it (`fusermount3 -uz`) and the daemon
    stops its trees: a mount no longer in the mount table counts as ended, although open files keep its FUSE session,
    whose trees are then retired (closed gate, a change committed later answered EIO; D118). `pdmount mount [-o ro] <mountpoint>` mounts it again through the running daemon; `-o ro` holds
    until the next plain `mount`, and `read_only` in `[[roots]]` and the daemon's `-o ro` always hold (`UnmountRoot`,
    `MountRoot` on the control socket). The long steps (opening the trees, the drain, the stop) run without the lock
    over the mounts, so `status` always answers; a second command on a mount while one runs is refused (`is being
    mounted`, `is being unmounted`). A mount whose FUSE session ends from outside (`fusermount3 -u`) is noticed within
    a second, or by the next `mount` or `unmount` of it: its trees are stopped as above and a new `mount` works.
    Path commands (`access`, `pin`, `queue`, `conflicts`, `sync`) reach My Files only; `cache wipe` reaches every
    mount's cache (§14.2, D136). Scenarios X14 and X19; the
    mount after a keyring wait in X13.
  - *Kinds:* `shared` (M5.12, below; at most one per profile), `photos` (M5.13, below; at most one per profile) and
    `test` (test builds with a fake drive): trees named in the config. `computers` is refused (Computers are out of M5's scope, D122).
- **Shared, as built (M5.12, D112):** `[[roots]] kind = "shared"` mounts the shared-with-me listing.
  - *Listing* (`SharedWithMe` in `pdmount-backend`): the shared-with-me pages (at most 64, by `More` and `AnchorID`), each
    share's creator and creation time once (`get_share`, kept), and the roots fresh, one batch per volume, for their
    names, kinds and the user's role (`direct_role`). The pages are read into PdMount's own types with the share target
    type as a number (`SharedApi`, as the timeline, D113), not the SDK's closed enum, which failed the whole listing
    for one link of a type it did not know: folders (1) and files (2) are mounted, the other known types (root, album,
    photo, Proton documents) are left out, and a type nobody knows yet is left out with a warning once per type and
    process, as the official clients skip it (M6.0.9, parity F1). Read at the mount, on each `DriveShareRefresh` (or `refresh`) of the core event stream,
    and at the next core poll after a listing that failed. The core stream is polled by one task of the daemon
    (`Pollers::watch_core`) every `sync.poll_interval` (30 s, the official client's) while a Shared mount is mounted,
    with the volume pollers' back-off; it starts from the position read before the first listing.
  - *One tree per share*, each with its engine and state as any further tree (`trees/<key>/`, key from volume and root),
    its write gate open while the role allows writing (below, M5.14a), its volume's poller shared with the other shares on that volume (other
    users' volumes every 10 min, the one in use every `sync.poll_interval`, D107). At most 32 shares are opened, oldest
    first (each tree has a 16 MiB RAM pool); the rest show in `status` as not opened.
  - *Names* (D107, D125): a share keeps its name. Folders are named in the top level, files in the files folder, each on
    its own. A shown name stays with its share for as long as the share is in the listing and its own name is the one
    the shown name was given for: a share that arrives later, or that its owner renames, never takes a name another
    share is shown under (a suffixed one such as `Team (alice@…)` included); it gets the suffix instead. Shares new to
    the mount or renamed are named in the order they were created: a share keeps its own name when no other share has
    it (shown, or among them); when two or more of them have the same name, or one has a name already shown or the
    taken one, each of them becomes `<name> (<owner>)`, and where that is taken too `<name> (<owner>, 2)`, `(…, 3)`.
    The top level's `Shared files` counts as taken. The shown names are kept in
    `~/.local/share/pdmount/<profile>/shared-names.json` (per share key: its own name and the shown name; written
    after each listing that changes them, a file that cannot be read names the shares anew), so a remount or a
    restart shows the same names whatever the listing's order. A name that could not be a file name (empty, `.`, `..`, `/`, NUL,
    over 255 bytes) is left out, except that a name over 255 bytes is cut to fit and a share already shown keeps its
    last name when its new one cannot be a file name (D118); the owner (the share's creator, untrusted) has control characters and `/` replaced and is
    cut to 64 bytes, and a suffixed name is cut to fit 255 bytes. A name changes in place only when its owner renames
    the share; a suffix that comes stays while the share is listed, also after the share it was added for has gone.
  - *Shared single files* sit in the virtual folder `Shared files` of the top level (inode 2, mode 0555, shown while one
    exists). A file's tree has a virtual root folder holding only the file under a fixed name (`ShareTree`), so the
    engine is the same; only the file's own events pass, and a rename by its owner shows at the next listing.
  - *Trees that change while mounted* (`pdmount_fuse::mount_dynamic`, `TreeSet`): a tree's number is its slot,
    `(slot + 1) << 48 | n`, kept for the mount's life and never reused (65 534 slots), so the inodes of a tree that stays
    never change and an inode of a removed tree never leads into another. Adding, removing or renaming a tree
    invalidates the kernel's entry of its name, and the folder's pages and attributes (a cached absence included);
    removing one also drops the kernel's cached pages of its inodes, after its name's entry, so paths through it end at
    once however many inodes the kernel holds (D118). A tree's notify thread starts before it is added, so a tree whose
    thread cannot start never shows. The virtual folders' times are those of the last change.
  - *Revocation:* a share goes when the listing no longer has it, or at the first "not found" of its root (a listing of
    the root, its `get_nodes`, or a refused event stream that finds the root gone), whichever comes first; one gone by
    "not found" is not opened again while the listing still has it. Its tree leaves the mount at once (D118): a request
    on a name in it answers ENOENT, as for a deleted folder (`lookup`, `stat` and `fstat`, which the kernel sends
    without the handle, `open`, `opendir`, `access`, xattrs, the changes), and one on a handle opened before EIO
    (`read`, `write`, `fsync`, `readdir`, the `close` of any handle); releasing a handle succeeds. Its engine is retired
    first: its write gate closes, and a change whose store commit ends after that is answered EIO, so nothing is
    acknowledged and then left out of the export. Its poller stops (its volume's poller with its last tree); a poll
    pass that took the tree before is waited for, and later passes skip it. Then changes still queued and local-only
    files are exported to `orphaned/` (below, D124), its store is committed, and its state and cached blocks are deleted. This cleanup runs in the
    background, kept by the tree's key: a share that comes back while it runs waits for it up to 20 s (two engines on
    one store would lose changes) or is left for a later listing, and the unmount and the daemon's stop wait for it
    within their deadline. A tree that would fail every event page is so dropped instead of listing itself again at
    each poll. A root the server returns but cannot decrypt is not "not found" (D119): the backend tells it apart
    (`Backend::get_nodes_checked` answers `NodeLookup::Degraded`; `ShareTree` never reports it gone, and a shared file's
    tree answers `Transient` for it), and the listing names such shares apart (`SharedWithMe::shared_listing`:
    `SharedListing::degraded`, not in `entries`), so that the mount keeps them as they were.
  - *Not covered yet:* a listing that cannot be read at the mount (offline) leaves the mount empty until it can; the RAM
    pool per tree bounds the number of shares; path commands (`access`, `conflicts`, `queue`, `pin`, `sync`) reach My
    Files only (D110).
  - `status` shows each share's line with its role and its volume's interval, e.g.
    `Team (alice): read-only (viewer), 0 pending, 0 parked, last poll 3 s ago, every 600 s, cache 0 B`. Control
    characters in the `Root:`, tree and `Orphaned:` lines (share names, errors quoting them) print as `?` (D118).
    Scenario X15.
- **Writes (M5.14):** shares where the node's role is `Editor` or `Admin`, go through the My Files write path. `Viewer`
  shares and shared single files return EROFS (D019). Losing the right with changes pending parks them and closes the gate; staged
  data is kept, under `orphaned/<tree>/` once the share is gone. Rename between trees returns EXDEV.
- **Writes in shares, as built (M5.14a, D114).** Computers (M5.11, M5.14b) are out of M5's scope (D122).
  - *Gate:* a folder share's write gate is open while the listing's role is `Editor` or `Admin` (`may_write`), at the
    open and at each listing (a `DriveShareRefresh`, §14.5 above); `Viewer` and shared single files keep it closed
    (EROFS). No second write path: journal, staging, conflicts, fsync modes, `.pdignore` and the flush are My Files',
    except that only the profile's own `.pdignore` applies in a share, never the share's `/.pdignore` (§7.7, D124).
    `read_only` and `-o ro` still hold over an open gate.
  - *Losing the right* (`pdmount_core::engine::rights`): a role that does not allow writing, read from the listing, or
    a change the server refuses (`PermissionDenied`: code 2011 or HTTP 403) on a tree that follows the server (every
    share's; My Files' never, where a refusal parks only its entry as before) closes the gate and holds every queued
    entry, and every entry queued while the hold lasts, with the reason `no write access …`, the way `queue park` holds
    one: nothing staged is deleted (RQ3), the files stay readable in the mount, and a local change (closing a file
    still open) does not lift the hold. An entry parked for another reason keeps it, and an entry whose step runs is
    parked only if it needs another step (D121). A refusal
    also reads the listing again, logs, and sends one notification (`PdMount: N changes to <share> wait`); `status`
    shows the share `read-only (<role>)` with its parked entries and, after a refusal, a line `the server refused a
    change; …`. A later listing with a role that allows writing opens the gate and forces the held entries; a refusal
    whose listing still says `Editor` keeps the gate closed until the role changes or the next mount, so a server that
    refuses while the listing allows is not retried in a loop. Parked state lives in RAM (§8.2): at the next mount the
    entries are tried again, held at once when the role still does not allow writing.
  - *Revocation with changes queued* (`pdmount_core::engine::orphan`): before a gone share's engine stops, its queue is
    held and every queued change exported to `~/.local/share/pdmount/<profile>/orphaned/<share name> (<YYYYMMDD-HHMMSS>)/`:
    each queued file's whole content, read as an upload reads it (staging, then the base blocks from the cache or the
    server), at `files/<path in the share>` with its mode and mtime; folders queued for creation as folders; and
    `pdmount-orphaned.json` (version 1: tree, reason, time, and per entry its path, change, `file` or `error`), with
    renames and deletions listed only. Staged content no journal entry uploads is exported the same way, whatever the
    reason it is not queued (D114, M6.0.3 V-1): a local-only file (§7.7) with the change `local-only`, and the edit of an
    uploaded file in a local-only folder (§7.7) with `not-uploaded`. The count that decides on an export
    (`Engine::changes_to_keep`) adds these files (`Engine::unqueued_staged`: staged, not in the journal, attached, and
    local-only or with content not uploaded) to the queued and parked entries, as `reset`'s count adds the staged
    inodes; a file both queued and staged is exported once, as its queued change. A file whose content cannot be
    written (its base can no longer be read because the share is gone and the blocks were not cached, or it has no
    plain path) is listed with the error, and the tree's raw
    state (`state.db`, `staging/`) moves to `state/` beside it; otherwise the state is deleted as for any gone share. If the export cannot be written, the state stays under
    `trees/<key>/`. One notification (`PdMount: N changes to <share> kept`), a log line, and `status` prints an
    `Orphaned:` line per directory with a manifest (read by the client, so also without a daemon) until the user
    deletes it. PdMount never uploads from `orphaned/`.
  - *Across trees:* a rename between two shares, or between a share and the top level, is EXDEV (the router, D110), so
    `mv`, `rsync` and `git` copy; between a share and My Files the kernel answers EXDEV (two mounts).
  - *Quota:* an upload to a share counts against its owner's storage; the server's quota error maps to ENOSPC as in My
    Files (§7.8, deferred to the next `flush`/`fsync` of the file). `df` (statfs) on the Shared mount shows the user's
    own quota, so a full share shows only as that error.
  - *Conflicts:* conflict copies made in a share are visible to all its members and its owner. A further tree's new
    conflicts are logged (`conflict recorded in a further tree`, with tree, kind and path) and notified as My Files'
    are, the body naming the tree (§10.8); while its gate is open, its other devices' markers are notified as My Files' are (one
    notification per pass, naming the tree) and shown under the tree's line in `status` (`Incoming: …`); a viewer's
    share reads none (D126).
  - *Marker (D076):* `pending_marker = true` under `[[roots]]` turns it on for that mount's trees (the profile's
    `sync.pending_marker` stays My Files' own). At the daemon's stop the further trees drain all at once, after My
    Files' flush, to the same deadline; a tree with the marker on, an open gate and entries still pending
    `MARKER_WINDOW` (10 s) before the deadline writes `.pdmount/<device id>.pending` in its root, counting only its own
    entries, and trashes it again if the rest uploads before the deadline (D096); the next drain trashes it otherwise.
    A tree on another user's volume (every share) writes it without the device name. Trees share the last 10 s of the
    grace, and My Files' flush comes first, so some may get no marker. An owner's PdMount reads `.pdmount` only in its
    My Files root (D114, option (a)), so it does not see a member's marker; the reading side does not filter by device,
    so every member with write access whose PdMount mounts the share reads every member's markers, at most 16 (D126).
    The file name carries the device id in shares too (V-4, Info).
  - Scenario X16; the R, G and N scenarios and M1, M3 run in a share's tree with `TARGET=share` (§15.3).
- **Photos:** a read-only virtual tree over the timeline and albums, not an engine tree: it has no folder operations (D080). Year and
  month folders come from timeline pages; names and sizes are fetched when a month is opened. Proton's stored thumbnails go to
  `~/.cache/thumbnails` in the background, so a file manager need not open the originals (D081; `photos.thumbnail_cache` turns it off).
- **Photos, as built (M5.13a, D113):** `[[roots]] kind = "photos"` mounts `<mountpoint>/<year>/<month>/<name>`.
  - *A read-only tree that is not an engine* (`pdmount_core::photos::PhotosTree`): it implements
    `pdmount_core::readonly::ReadOnlyTree` (lookup, getattr, opendir and the listing's snapshot, open, read, and
    `resolve`, the slow path of a folder whose entries are still arriving), and `pdmount_fuse::mount_read_only` serves it
    with an adapter of its own (`RoFs`), alone in its mount: no router, a kernel `ro` mount, and every change answered
    EROFS without asking the tree. Folders are mode 0555, photos 0444; a photo's times are its capture time.
  - *Volume:* the Photos share's volume and root (`photos_volume`: on Proton the SDK's Photos client finds the share, which
    gives its drive client the share's key). The tree's backend is a tree backend of that volume and root
    (`ProtonBackend::photos_tree`, through the Photos client's drive client and its link endpoints, as the official Photos
    client reads photos). An account without Photos fails the mount with that reason.
  - *Stage 1, years and months:* the timeline (`drive/volumes/{id}/photos`, 500 per page, newest first,
    `PreviousPageLastLinkID`), read once from the mount on, a page per background turn (the session's gate, D027, D110);
    each item's link, capture time, tags and related photos. The volume's event cursor is taken before the first page. A
    photo's month is that of its capture time in the local time zone. Until the read has its first page, the top
    folder's listing waits for it; a lookup of a year or month not read yet waits until it appears or the read ends.
    While the tree shows nothing yet, a page waits for the gate at most 2 s, so a load in another tree slows the first
    `ls` of Photos, never fails it (D119). The pages are read into PdMount's own types (a tag any number, a capture
    time any number of seconds, one beyond a four-digit year placed at the epoch's month), never the SDK's, which
    refuse a whole page for one tag above 9 or a time chrono cannot hold. An empty page after a link means that link
    went since its page (the server answers so), not the end: the read asks for the page after the link before it
    (the last 8 of the page), then from the first page again (at most 3 times per attempt). A read that fails goes on
    from its last page at the next attempt, with the cursor it took; photos it did not list go only after a read that
    finished (D119).
  - *Stage 2, names and sizes:* when a month is opened (`opendir`, or a lookup of a name it does not have yet), its photos'
    nodes in batches of 50 (`get_nodes_checked`, one request each), one background turn per batch, the month opened last
    first. The first `opendir` of a month without names waits for the first batch; later ones show what has arrived. A
    request a user waits on fetches the batch itself (D119): the first batch of a month without names as the user's
    work, outside the gate, as the engine lists a folder; a later batch (a lookup of a name not there yet) with a turn
    of the gate waited for at most 2 s; a lock keeps it and the names task from fetching the same batch. A node the
    server returns but cannot decrypt (`NodeLookup::Degraded`) is not gone: a photo with a name keeps it, one without
    is not waited for (its month counts as named, `status` counts it) until an event of it reads it again. The
    kernel keeps a folder's pages only while its entries hash the same as at its last open, no other handle of it being
    open (D072's rule), so a month whose names arrive in stages is listed again at each open until they are all there; each
    batch also drops the kernel's view of the month and any absence it cached of the new names. A lookup of a name not
    there yet waits for the rest of the month, except a hidden name (`.hidden`, `.directory`), answered at once. A month
    opened once also gets the names of its new photos. Names the server gives that cannot be file names are left out;
    two equal names in a month (only a related photo or a bad name can clash: the server keeps names unique in the
    Photos root) become `name (2).ext`, in link order.
  - *Stage 3, events:* the Photos volume's event stream, polled by the tree itself every `sync.poll_interval` (the user's
    own volume, D107), with the pollers' back-off and the session's connectivity. Photos is the only tree on its
    volume, and the `Poller`s of §9.2 serve engines, whose cursors live in engine stores; a poll of its own reads the
    stream once all the same. A link the tree does not know is read with its node (a file in the Photos root, not
    trashed, is a candidate) and found in the timeline for its capture time: the first page at every poll, further
    pages only when the last search beyond the first page is 5 min old (a photo with an old capture time waits that
    long under a sustained upload); it shows at once with the name its node gave. A candidate the whole timeline does
    not list (a draft still uploading), or anything else (an album, a node that cannot be decrypted), is set aside
    until its next event, which looks at it again (its commit). A trashed or deleted photo goes, with its related
    photos; a changed one whose name is shown has its node read again. The cursor always moves past a page: a link
    whose node or timeline place cannot be read now goes to `pending` (stored in `meta` before the cursor moves,
    counted in `status`) and is tried again at each poll (D119). A `refresh` reads the whole timeline again, keeping
    the names; photos it no longer lists go.
  - *Background tasks:* the timeline and events task, the names task and the thumbnails task run again 5 s after a
    panic, logged and counted in `status` (`PhotosStatus::restarts`, D119).
  - *Sessions:* a new login replaces My Files' drive client, its credential and the Photos clients in one write
    (`ProtonBackend::replace_session`); a Photos call uses the drive client and credential of the Photos clients it
    found ready for the whole call, its token refresh included, and fails without them instead of using My Files'
    client (D119).
  - *Store* (`trees/<key>/state.db`, key from the Photos volume and root): a `photos` table of its own (link, capture
    time, tags, main photo, name, size, revision) beside the store's usual tables, and in `meta` the volume and root, a
    marker that a whole timeline read finished, the event cursor, written after the rows of its page, and the links
    whose events wait (`photos.pending`). A remount loads
    it and reads neither the timeline nor the names again (the log says `timeline and names loaded from the store`); a
    store for another volume or root is emptied.
  - *Reads:* the tree's own `BlockCache` (16 MiB RAM pool, blocks under `trees/<key>/blocks/`, on the profile's disk cap
    with the share of a further tree, D110), the backend's block lists and manifest verification, as an engine's reads;
    the engine's sequential readahead (§5.3) with the session's readahead permits, none for scan reads (M6.0.11, D135).
    Without it a video read one block per round trip: 4 MiB per (RTT + transfer), below a phone video's bitrate at
    5 MB/s.
  - *The session's limits and guard:* the daemon passes the tree My Files' `SessionLimits` and `ScanGuard`
    (`PhotosOptions::limits`, `PhotosOptions::scan`, as `EngineOptions`'), so it takes part as an engine tree does
    (§6.4): its download permits are the session's; its stages, event polls and thumbnails take turns of the session's
    background gate, held back by a request a user waits on in any tree; a read that downloads holds back every tree's
    background work and scan reads. Every open counts for the guard (the mount refuses any other than read-only), so
    a thumbnailer or `grep -r` over the mount begins a scan, alone or with opens in other trees; a photo opened during
    one reads its first `scan.first_bytes` as scan reads (a turn of the guard, scan priority, the scan share of the
    cache), and an interactive read of a scan block takes it out of the scan blocks.
  - *Live Photos and Motion Photos* (tags 3 and 4): the timeline item's related photos (a Live Photo's video) show
    beside their main photo, in its month, under their own names, and go with it. A related photo the timeline also
    lists as a main photo stays one.
  - *Thumbnail hook:* `ThumbnailHook::month_opened` gets a month's named photos (link, revision, path, size, mtime)
    when a month whose names are all there is opened, and when its names are complete. It runs on the FUSE thread and
    only queues; the tree's own hook is its `Thumbnailer` (`PhotosTree::set_thumbnail_hook` replaces it in tests).
  - *Stored thumbnails (M5.13b, D115):* when a month is opened, Proton's thumbnails of its photos go to the freedesktop
    thumbnail cache, `$XDG_CACHE_HOME/thumbnails/{normal,large,x-large}/<md5 of the URI>.png`, so a file manager shows
    them without opening the originals (D081). `photos.thumbnail_cache = false` turns this off.
    - *Order and pacing:* a task of the tree takes the queued months, the one opened last first, in batches of at most
      30 photos, on the session's gate (D027, D110): one background turn per photo's reference, one more per batch for the
      request of the thumbnails' URLs (`get_thumbnail_blocks`) and the downloads, 8 at a time. Only Proton's 512 px
      thumbnail (type 1), never the 1920 px preview (D107). A failed batch is tried again with the pollers' back-off.
      The task asks for one turn at a time and holds it for one request; turns are granted in the order asked, so the
      thumbnails and the other trees' background work take turns, and none of them waits for more than the turns queued
      before it; like all background work, they wait while a user's request is in flight in any tree.
    - *Verification:* a photo's reference comes from the first page of its revision's block listing, without the
      blocks' URLs (`NoBlockUrls`, 20 blocks), one request per photo (D135). When that page is the whole listing (a
      photo: fewer than 20 blocks, 80 MiB), its manifest signature (the SHA-256 of each encrypted thumbnail, then of
      each block) is verified with the keys a block read uses, and the download's SHA-256 must be the listed digest
      before it is decrypted. A larger revision (a video) is not listed further for its thumbnail: the manifest needs
      every block's hash, which took a video's whole listing per thumbnail. Its thumbnail is checked as the official
      clients check every thumbnail, by decrypting with the file's content key, whose signature is checked (D132); the
      JS SDK's `thumbnailDownloader.ts` checks no digest. The SDK's `enumerate_thumbnails` checks nothing but
      decryption either, so PdMount does this itself (`Photos::thumbnail_ref`, `Photos::fetch_thumbnails`).
    - *Untrusted bytes:* at most 1 MiB encrypted (the listed size before the download, then the stream) and decrypted.
      Before the decoder sees the WebP, PdMount walks its RIFF chunks (`thumbnails/frames.rs`; at most 1024 chunks,
      each length checked against what is left, nothing after the RIFF's end, no nesting below an `ANMF`) and reads
      every image frame's size from the frame's own header: the `VP8 ` key frame header (start code `9d 01 2a`,
      14-bit width and height) and the `VP8L` header (14-bit width − 1 and height − 1); an `ALPH` chunk takes its
      frame's size. The layout must be one of WebP's: a simple `VP8 ` or `VP8L` file, a `VP8X` still with one image
      (an `ALPH` only before a `VP8 `) the size of the canvas, or a `VP8X` animation (`ANIM`, `ANMF` frames inside
      the canvas, each with one image the size of its rectangle); other chunks (ICCP, EXIF, XMP, unknown) are
      skipped. The canvas and every frame are at most 1024 px a side and all frames together at most 1024 × 1024
      pixels; the decoder's own size must then be the canvas. This is needed because `image-webp` decodes a `VP8X`
      file's inner frame at the size that frame declares (up to 16383 × 16383) before comparing it with the canvas,
      and its memory limit covers only the metadata chunks (V-5). Decoding and scaling run in a blocking task under
      `catch_unwind`. A photo that fails any of this, or has no thumbnail, is noted for the session with a warning
      and gets none.
    - *The PNGs:* `normal` 128 px, `large` 256 px, `x-large` 512 px on the long side, scaled down from the WebP, never
      up, aspect ratio kept; RGB, or RGBA when the WebP has alpha. Text chunks before the pixels: `Thumb::URI`, the
      `file://` URI of the photo's path in the mount (the mountpoint as configured, absolute, symlinks not resolved),
      escaped as GLib's `g_filename_to_uri` (every byte but ASCII letters, digits and `-._~!$&'()*+,:=@/` as upper-case
      `%XX`); `Thumb::MTime`, the capture time in seconds the mount reports; `Thumb::Size`; `Software`;
      `X-PdMount::Revision`. Each is written as a temporary file of mode 0600 in its folder, then renamed; folders are
      made 0700.
    - *Done work:* a size whose file has the same URI and mtime (and size and revision where it has them) is kept,
      whoever wrote it, and only missing sizes are written. The session notes the photos done and those without a
      usable thumbnail, per revision, and does not look at them again; a new session reads the files' text chunks.
  - `status` shows the mount read-only and the tree's line with its stages, e.g.
    `Photos: read-only, 0 pending, 0 parked, last poll 3 s ago, every 30 s, cache 1.2 MiB, 302 photos, 7 named, timeline read`.
    Scenario X17 (§15.3); on the fake, `pdmount fake photo add|seed|delete` (§15.1), each photo with a thumbnail.
- **Bulk-read guard (all trees, D081, D111):** many different files opened in quick succession become a scan: below interactive priority,
  paced, no readahead, a limited share of the block cache. Opening a single file stays interactive. One guard per session, shared by
  every tree's engine (`EngineOptions::scan`) and the Photos tree (`PhotosOptions::scan`); detection, classes and defaults in §6.4.

What the API allows in practice is settled by M5.9 (`docs/plans/m5.md`).

---

## 15. Testing strategy

### 15.1 Layers

| Layer | Content |
|---|---|
| Unit | Name validation, conflict naming, ignore matcher, errno mapping, block offset math (with and without `BlockSizes`), access resolution |
| Fake backend | Proton model: name uniqueness (2500), `CurrentRevisionID` and `OriginalHash` preconditions, trash, quota, an event log with `more`/`refresh`, several client handles on one drive (`with_client`; a client has a UID and may leave out the SHA-1 and the mode like the official apps, `pdmount fake write\|create --app`). Revisions record the client UID, claimed SHA-1 and creation order (`revisions()`). Pause points (`pause_at`) stop the next upload, folder creation, rename or trash of a client just before its commit, so a test can let another client commit first; uploads are two-phase (draft, then commit), `set_commit_checks(false)` models the server as M3.1 found it (the base revision is checked only at draft creation, D043), and `set_name_hash_enforced(false)` a server that ignored `OriginalHash` (it does enforce it, §10.4; the test documents the alternative). `all_revision_sha1s()` lists the content still reachable. Fault injection: latency, one-shot failures per operation, and `faults.json` in the fake's directory for running daemons (`{"rules":[{"kind":"offline"|"unauthorized"|"brokennodes","until_ms":…,"client":"pdmount-<device id>"}]}`; every call checks it, re-read when the file changes; `brokennodes` fails only `get_nodes`, with an error that is not transient, as a node under an undecryptable folder did (D130); a daemon starts while the rule is active when the profile mounted online before (§9.6); `pdmount fake fault offline\|unauthorized\|broken-nodes [--for <s>] [--client <uid>] <dir>` and `fault clear`; the daemon's client UID is `pdmount-<id>` from `pdmount status`'s "Device" line; the fake has no sign-in, so a daemon in `needs_login` probes every 2 s and goes online when the rule is gone, D047). **Network profile** (M4.2): `profile.json` in the fake's directory (`{"rtt_ms":80,"up_bytes_per_sec":20000000,"down_bytes_per_sec":20000000}`; `pdmount fake profile [--rtt <ms>] [--up <B/s>] [--down <B/s>] <dir>`, `fake profile clear <dir>`; in memory `set_profile`), re-read when it changes in the same per-call pass as `faults.json`. Every call costs one RTT; `list_children` one per 150 children and `get_nodes` one per 150 ids (at least one); uploads three (draft, blocks, commit) plus `size / up`; block fetches one plus `len / down`; a block page one, through the adapter's block lists (§6.1: a read's page, the others for the manifest; the layout from the claimed sizes costs none), and a thumbnail's reference one (M6.0.11, D135). Each direction is one pipe per process, shared by its concurrent transfers (a transfer starts when the one before it ends), so two daemons on one fake each have their own pipes. The cost is slept after the fault checks and before the call reads or changes the drive, outside the state lock. The fake keeps a child index per folder and a running quota total, and reads blobs after releasing its lock, so it stays cheap under load. Signatures that do not verify (D132): `set_unverified(link, part, failed, signer)` marks a node's name, key or content key, persisted and announced with an event, and a rename by a client clears the name's (it signs the new name); `pdmount fake unverified --part name\|key\|content-key [--signer <address>] [--clear] --path <path>\|--photo <name> <dir>`. Planned: partial batch failures, crash points. Persistable to a directory, so two daemons can share one fake (`--backend fake:<dir>`, test builds only): `snapshot.json` plus an append-only `journal.jsonl` (one line per change, written before the call returns, so a `kill -9` of the daemon loses nothing), compacted into the snapshot at 8 MiB; an `epoch` file tells other handles to reload. Growth is bounded: the last 16 revisions per file (`PDMOUNT_FAKE_KEEP_REVISIONS`), 1024 trashed nodes, 10 000 events and 50 000 changes are kept; dropped revisions delete their blobs. Changes and events keep absolute sequence numbers across trimming (`pdmount fake changes --after <n>`). **Volumes and shares** (M5.10, D110): roots without a parent on the drive's own volume (`fake`) or on another user's (`vol-N`), shared with the drive's user as `viewer`, `editor` or `admin` (`add_root`, `set_member_role`, `pdmount fake root [--owner <user>] [--role <role>] [--from <local dir>] --name <n> <dir>`); a handle from `tree(volume, root)` serves one tree and polls only its volume's events (ids stay absolute); reads need ownership or membership, changes ownership, `editor` or `admin`, and a move between volumes is `InvalidMove`. **Shared-with-me** (M5.12, D112): the listing of other users' shares with the drive's user as a member, oldest first, folders and single files (`add_shared_file`, `pdmount fake share-file`); a role change or revocation (`set_own_role`, `pdmount fake share-role --role <role>\|none`) changes only the listing; the core event stream signals a `DriveShareRefresh` only when told to (`core_signal`, `pdmount fake core-signal`), so a test chooses between the core event and the first "not found". **Photos** (M5.13, D113): the drive user's Photos volume (`photos`) with a root named "root", photos as files in it with a capture time, tags and a main photo (`add_photo`, `pdmount fake photo add\|seed\|delete`), the timeline newest first in pages of 500 (`Photos`), and Create events on that volume; each photo has a thumbnail (M5.13b, D115), a 512 × 384 WebP of one colour made when asked for, checked against its digest as on Proton (`set_photo_thumbnail`, `fake photo add --thumbnail <w>x<h>\|none\|tampered\|not-webp`); `commit_photo` makes a file of the Photos root a timeline photo with an `UpdateMetadata` event (a draft's commit), and `set_degraded` makes a node's metadata unreadable in this process, as one the SDK cannot decrypt (`get_nodes_checked` answers `Degraded`, `list_children_checked` names it degraded, the shared-with-me listing names its share as degraded; D119, D132) |
| Property (proptest) | Two tests in `engine/proptests.rs`. `random_changes_converge_to_the_model`: one device against a POSIX model, with restarts, crashes and another device's changes at clean points. `two_devices_and_an_app_converge` (M3.17): two engines (`dev-a`, `dev-b`) and an app on one fake, operations interleaved with pending local changes (no drain before a remote change), restarts and crashes of either device, explicit event syncs. Invariants listed below. Regression cases of both are replayed in `regression_cases` and `concurrent_regression_cases` (the latter with a debounce long enough that steps meet in the written order, one sequence per M1, M3–M7 interleaving; M2 needs a pause at the commit and is `m2_edit_edit_interleaved_at_commit`) |
| Mounted, CI | Local CI script (`scripts/ci.sh`) on the dev machine with fuse3, over the fake backend; it must pass before every push (D084). It runs every step and ends with a summary of all results (`--fail-fast` stops early), and uses one scenario root per checkout, `/dev/shm/pdmount-ci-<checksum of the path>`, locked, so runs in several worktrees can go on at once (D098). On GitHub (D097, M5.15): `ci.yml` runs `--quick` on pushes and pull requests and the full run on demand and weekly, on the fake only; dist's `release.yml` builds the release archive on a `v*` tag. `--full` runs every scenario below, the default a subset, `--quick` none (D084). pjdfstest and fsx run separately. **pjdfstest subset** (`tests/pjdfstest.sh`; pjdfstest built from source at the commit named in `tests/pjdfstest.list`): open (incl. O_EXCL), rename, unlink, mkdir, rmdir, truncate, ftruncate, chmod, utimensat, with the file-type loops cut to regular files and directories; link, symlink, mknod and mkfifo give EPERM by design (§8.7, scenario N1) and are left out. The failures of each test file must equal the list's: cases that need other users (the agent is not root, and without `user_allow_other` only the mounting user reaches the mount, D020), unsupported file types, directory `nlink` 1, and ENOSPC from the quota check (§7.8) for a huge truncate. **fsx** (`tests/fsx.sh`; xfstests' `ltp/fsx.c` built standalone): two instances at once with mapped reads and writes, 256 KiB and 8 MiB files; 6 h for the M2 exit check; without `-c`, which needs root for `drop_caches`. fsx, pjdfstest and soak never run against the real account (D030) |
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

The R, G, N and A scripts, `sha` and `g6_read` take `VERIFY_MODE`; `m*` and `e2` run live or skip other modes, and `queue` (§8.2, §5.6), `ops` (§14.4), `svc` (§14.4, supervisor options) and `kill9` (M1 exit, §16) are live only:

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
| E1 | While mounted, another client renames a directory, trashes a file and writes a new revision of a file read before | All three visible without a remount within 40 s (`poll_interval` + 10 s, D037), measured after the daemon has caught up with the earlier scenarios' events (a file made by the other client shows first: `event_barrier`); the old names are gone from the kernel's caches. Live mode, with the device's node fetches broken (`fake fault broken-nodes`): a new revision still shows within 150 s, after three failed polls and a refresh, and the log names the page given up (D130)
| E2 | (`live`) The drive is unreachable for A for 90 s while `rsync` writes into the mount; then the session is rejected (`fault unauthorized`) and a file and a folder are made; then `fault clear`; `login` in each state and `login --force` online; then a restart under each fault (the daemon starts from the stored account, §9.6) | `status` shows `Connection: offline`, then `online`, then `needs login`, then `online` again, and the same after each restart (the fake daemon probes while `needs_login` lasts, standing in for a sign-in). Cached data reads offline; a file never fetched fails within 3 s (`timeouts.offline_read`). Local writes are accepted in both states. `login` says the profile is already signed in while offline and online (exit 0), and asks the daemon for a sign-in, which the fake does not have (exit 1), in `needs login` and with `--force` (D108). After `fresh`, everything is on the drive, nothing parked and the file reads its new content; the state survives the verify mode |

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
Scripts: `x1.sh` to `x4.sh` and `x11.sh` (live only; `x11` skips without `dbus-daemon` and runs `pdmount fake logind` as
the stub; the notification stub, `pdmount fake notifications serve`, on a private session bus (`lib.sh`:
`start_session_bus`, `start_notify_stub`), serves `x4`, `x11`, `x12`, `x13` and `x16`, which skip without `dbus-daemon`). The fake scenarios' daemons see no system bus otherwise
(`DBUS_SYSTEM_BUS_ADDRESS` in `lib.sh`), so they never take a lock from the machine's logind.

| ID | Interleaving | Pass condition |
|---|---|---|
| X1 | Write files, SIGTERM at once, within the debounce | All on the fake drive before the daemon exits; exit well under the grace |
| X2 | Queue larger than the grace can upload, SIGTERM; then again with a second SIGTERM 2 s after the first | The daemon exits within the grace; after a second SIGTERM within 2 s, with the store committed; the rest uploads at the next mount; no loss |
| X3 | SIGTERM while offline; empty queue | Exit with no flush wait |
| X4 | Remount after a SIGTERM with a queue larger than the grace, and after `kill -9` with a queue; remount with an empty queue; a notifier that fails | One log warning and one "still uploading" notification, then one "uploaded" when the queue is empty; nothing for an empty queue; no "uploaded" when the first notification failed |
| X5 | By hand, Gentoo and Debian: logout and reboot with a queue that fits, crash, locked keyring | Changes visible on the other device, or a documented limit from M5.0; restart after a crash; `needs_login` without a hang |
| X6 | Fake, a private bus with the notification stub and a stub file manager (`fake notifications file-manager`), `xdg-open` a stub in `PATH` (M6.2, D139): an `edit_edit` and a `create_create` of a name with `<b>&` (the other device is `fake write`/`fake create` while the file is held open), a `remote_delete_local_edit` (notice); **Show** invoked through the stub, then again without the file manager; after a restart five conflicts at once; `notify.conflict_names = false`; `notify.conflicts = false`. M6.3 (D140): the stub stopped, two conflicts made and a third resolved, then the stub started; a restart; four more made with the stub stopped, then the stub started. The review's fixes (D139, D140): `session attach` from a second bus with a stub, a conflict and its **Show** there, then that bus killed and another conflict; a `test` root (`fake root`, the other device `fake create --root`) with one tree, then with two, a `create_create` in its tree `team` each time and **Show** | The `open` ones named (path, plain words) and shown once, the name escaped, each with **Show**; the notice logged and listed, not shown; **Show** reaches `ShowItems` with the copy's `file://` URI, without the file manager `xdg-open` gets the copy's folder and the notification's bus; three of the five at once, then "2 more conflicts" when 30 s have passed; no file in the body; no notification. M6.3: nothing while the service is absent; when it starts the two open ones, not the resolved one nor the one made with `notify.conflicts = false`; nothing again after a restart; the four in one "4 conflicts were recorded while no desktop was listening" notification. The attached bus gets the notification and `xdg-open` with its address; once it is gone the daemon (same pid) logs the drop and shows the next conflict on its own bus. The further tree's body is "team: /<name>", and `ShowItems` names the copy at the root's mountpoint with one tree, in `team/` with two |
| X7 | Fake: several roots with My Files; a remote change in each; a write to a read-only root; a share revoked. Real, read-only: the user's own shared item, device and photo | Every root lists and reads with the right sha256; remote changes appear without recursive listing; writes return EROFS; a revoked share disappears without an error on the others; the My Files scenarios are unchanged |
| X8 | Fake: the R, G, N and A scenarios and M1, M3, M6, M7 on a share with `Editor`, in `live`, `remount`, `crash`, `fresh`; a `Viewer` share; the role lowered and the share revoked with changes pending; `mv` between trees | Same pass conditions as in My Files; `Viewer` writes return EROFS; nothing staged is lost, and it is listed and kept; `mv` copies after EXDEV |
| X9 | Real, with the user's exception to D028: a small rsync and git run in a test share from another account | Content is right on the owner's side and in the official client; the owner's account sees the commits' author as this account |
| X16 | Fake, a drive and two profiles of its own (M5.14a, D114): shares `Edit` (alice) and `Other` (bob) as an editor, `View` (alice) as a viewer, `Gone` (carol) as an editor; A mounts them in a Shared root with `pending_marker`, a 1 s debounce. `Other` and `View` each hold 40 markers of other members (D126). Writes, a delete, a rename inside `Edit`; writes in `View`; `os.rename` and `mv` from `Edit` to `Other`. `Gone` holds a `/.pdignore` of `*` from its owner, and A's profile `.pdignore` has `*.local` and `build/` (D124); A writes `a.txt` in `Gone`, and moves the uploaded `up.txt` into a new `build/` and writes it. A file written while held open, the role lowered to viewer without a core event, the file closed; editor and a core event. A file held open, viewer and a core event, the file closed; editor and a core event. Then a 120 s debounce: changes in `Gone` (new files, a folder, an edit of a read file, a delete, a file `keep.local`), then `Gone` revoked and a core event. B mounts the shares. A writes 24 MiB in `Edit` at 1 MB/s up and gets SIGTERM with a 16 s grace; A mounts again | `Gone/a.txt` reaches the drive (the share's `*` keeps nothing local-only); `build/up.txt` changes nothing remotely and shows `pending` (§7.7); `Edit` is `writable (editor)` and its changes reach the drive (B reads them), `View` answers EROFS; `Other` shows 5 `Incoming:` lines and `+35 more` and gets one notification, which counts the 24 markers not read, `View` neither; the rename is EXDEV and `mv` copies; the refusal closes the gate (`read-only`, 1 parked, the note in `status`, log line, a notification), the file stays readable and new files get EROFS; editor again uploads it; the role change holds the open file's change, the close does not lift it, editor again uploads it; `Gone` leaves the mount and `orphaned/Gone (<time>)/` holds the files with their content, `keep.local` among them as `local-only`, `build/up.txt` with its edit as `not-uploaded` (M6.0.3 V-1), while the new files are `upload`, and a manifest listing the delete, `status` shows `Orphaned:`, a notification and a log line, no `state/`, its tree state is deleted; the stop takes at most the grace plus 5 s and leaves one marker, in `Edit`'s `.pdmount/`, whose JSON has no `device`, none in My Files; B shows `Incoming: device <id>` under `Edit` and gets a notification; A's next mount uploads the rest, B's `Incoming:` goes and B reads the files |
| X15 | Fake, a drive and profile of its own (M5.12, D112): a `shared` root; shares `Team` from alice and from bob, `Docs` and `Quiet` from carol, and a shared file `plan.txt` from bob. Reads and writes; `ls` in bob's share, then in alice's; a file of `Docs` held open; a new share from dave, `Docs` revoked and one core event; `Quiet` (never browsed) revoked without a core event, then `ls` in it; bob's `Team` revoked and a core event; a share `Team (alice)` from mallory and a core event (D125); a remount; the daemon's stop | The top level holds `Docs`, `Quiet`, `Shared files`, `Team (alice)`, `Team (bob)` (mode 555) and `Shared files` holds `plan.txt`; reads match, every write gets EROFS; `status` shows the share in use `every 30 s` and the others `every 600 s`; after the core event (within 45 s) `Late` shows and `Docs` is gone, and a read of its open file gives EIO; `Quiet` goes within 10 s of its "not found" (log line); the gone shares' state is deleted; alice's share stays `Team (alice)`, mallory's shows as `Team (alice) (mallory)`, and the remount shows the same names; the stop leaves no mount |
| X17 | Fake, a drive and profile of its own (M5.13, D113): a `photos` root; 300 photos over four years (`fake photo seed`), a Live Photo (still and video) and a photo from a local file in the newest month. `ls` of the top level and each year; `ls` of the newest and the oldest month; a read; writes; a new photo and the Live Photo's still deleted on the fake; `status`; `unmount` and `mount` of the Photos root; the daemon's stop. `XDG_CACHE_HOME` is under `$SCENARIO_ROOT` | The years and months match the capture times (local time; mode 555); each month lists its photos once opened, the video beside its still; the opened month's thumbnails are in `$XDG_CACHE_HOME/thumbnails/{normal,large,x-large}` (M5.13b) under the MD5 of the URI, with that `Thumb::URI`, the photo's mtime as `Thumb::MTime`, 128/256/512 px, mode 600 in folders of mode 700, and a second open rewrites none; the read's SHA-256 matches, mode 444, mtime the capture time; every write gets EROFS; within 45 s the new photo shows and the still and its video are gone; `status` shows `302 photos, … named, timeline read`; after the remount the log says the timeline and names were loaded from the store, with no timeline read or names fetch, the new photo gets its thumbnails and the others are not written again; the stop leaves no mount |
| X18 | Fake, the scenario profile (M6.0.6, D132): in `pdmount-test/x18`, a file `a.txt`, a folder `dir` with a file and `deep/sub/hidden.txt`, made by another client; `fake unverified` marks the content key of `a.txt`, the key of `dir` and the name of `hidden.txt` (a folder never listed here); `cat`, `mv`, `rm`, a create, a copy and a move into `dir` and `ls dir`; `status`; `signatures check pdmount-test/x18`; `signatures accept` of `a.txt` and of an unknown id; a remount; then the marks cleared | `a.txt` and `dir` show in `signatures list` within one event poll, `hidden.txt` does not; `a.txt` is listed and stats, every refused operation fails with EACCES and changes nothing; `status` shows `Signatures: 2 unverified, 2 refused`; the check reports 3 unverified and lists `hidden.txt … [name unverified]`; after the accept `a.txt` reads while `dir` stays refused, the unknown id fails; after the remount the same holds without a listing; once cleared the list is empty and `dir` and `hidden.txt` open |
| X19 | Fake, a drive and profile of its own (M6.0.10, D136): a `photos` and a `shared` root beside My Files; three photos (two in one month of one year, one in another year), the share `Team` with a subfolder, the shared file `plan.txt`, My Files with two files and a pinned one, all read with `O_DIRECT` (one block each); `cache wipe` of a photo, a month, a year and the Photos root; of a share's subfolder, a file, and the files folder; of a relative and an absolute path in My Files; with no path; of a path outside every mount, an unknown share, shared file, year and file, and one with `..`; then each tree read again | Each wipe frees exactly the blocks below its path and names only that tree (the Photos root, `Team`, `Shared files/plan.txt` as `signatures list` names them); with no path the reply gives My Files 2, Photos 3, `Team` 2 and `plan.txt` 1 blocks, 8 in all, and the profile's total in all 4 trees; the pinned bytes are the same after; a second wipe frees 0; the refused paths fail with `outside every mount`, `no tree named Nope`, `no shared file named nope` or `no such file or directory`, and the wiped files read back byte for byte; the stop leaves no mount |
| X20 | Fake, a drive and profile of its own (M6.4, D137): My Files and a `photos` root, a private session bus with the notification stub, `scan.opens = 3`; files of 6 MiB in `pdmount-test/` (one named `<b>&.bin`) and a 6 MiB photo; two files read into the cache, then the profile `--rtt 80 --down 500000` (a 4 MiB block in about 8 s). Reads of the first 64 KiB with `O_DIRECT`: a cached file; `big.bin`; `big.bin` again after its `cache wipe`; `<b>&.bin`; a scan (the two cached files, then three cold ones opened at once); the photo; then a remount with `notify.downloads = false` and a cold file | The cached read sends nothing; `big.bin` (at least 5 s) gives one "PdMount: downloading" with "big.bin (6.0 MiB) is downloading; it opens as its blocks arrive." and its `CloseNotification` within 10 s; the second read of `big.bin` sends nothing; the body of `<b>&.bin` reads `&lt;b&gt;&amp;.bin (6.0 MiB) …`; the scan's reads take at least 5 s and send nothing; the photo gets its notification and close; with the setting off the slow read sends nothing; the stop leaves no mount |
| X21 | By hand on the user's desktops, GNOME and KDE (M6.5; not run for M6, D142; `scripts/repro/m6-desktop.sh` on a fake drive, a test build, the daemon started from a terminal of the desktop session; checklist in `docs/plans/m6.md`, "Running the M6 checks"): an `edit_edit` conflict and a click on **Show**; conflicts made while the daemon ran on a bus without a notification service, then its start on the desktop's bus (as at a login), and `session attach` from the desktop; a slow profile and the first 64 KiB of a 64 MiB file; each `[notify]` key off | The conflict notification with path, plain words and **Show**; **Show** opens the file manager at the folder with the copy selected; nothing while no service, then each missed conflict once at the start and after the attach; "PdMount: downloading" about 5 s into the read, gone by itself when the read ends; no conflict notification, one without the name, no download notification |
| X10 | Fake with the network profile: open a folder of 2,000 images in a file manager with thumbnails on, then open one image; the same with the thumbnail cache off; the same folder in My Files | With the cache on, no originals are downloaded for thumbnails; in every case, no HTTP 429 from the fake's rate rule, the opened image's first byte stays within the §5.7 read SLO, and reads on another mount stay responsive |
| X11 | Fake, private system bus with a stub logind: PrepareForShutdown with a queue, with an empty queue, then cancelled; SIGTERM after PrepareForShutdown; no logind; the lock refused | The lock is taken at mount; with a queue the flush starts at PrepareForShutdown, one notification, the lock released when the flush ends; empty queue: released at once, no notification; cancelled: taken again; the grace not restarted by SIGTERM; without a lock SIGTERM alone flushes, as X1 |
| X12 | Fake, two daemons on one drive: A, with `sync.pending_marker`, runs out of grace with entries left (1 MB/s up); B has a notification stub on a private session bus; then A mounts again; last, A stops with a 5 s grace and a small queue | A leaves `.pdmount/<A>.pending` (JSON as §14.4, no file names); B shows `Incoming:` and gets one notification; on B, `.pdmount` answers EROFS; A's next mount drains and trashes the marker, B's `Incoming:` goes; nothing is lost; with the 5 s grace the marker is written at the start of the flush and trashed in the same stop |
| X13 | Fake, private session buses (M5.17, D102, D106): a profile mounted before starts in keyring mode on bus A, which has no Secret Service and no notification server; `login`; `session attach` from A, from an unreachable bus, with a `tcp:` address, then from B (the notification stub logs its bus; B's and C's bus start it through an activation file at the first notification); gnome-keyring starts on B without the profile's cache key, which then comes back with an attach; a daemon started with `--service` on B with B's collection locked (a stub prompter that never answers), a `login` whose dialog stays open, a stop, a second such start and gnome-keyring restarted unlocked; then the opposite layout (D102 amended): a daemon on A, attached from bus C, which has the notification server and an activatable Secret Service that never answers, while gnome-keyring starts on A; twice, the second time with the cache key only in A's keyring; an attach from C with `DISPLAY` and `XAUTHORITY` set while the keyring is on A, a restart, and one without them. The restart (D102 amended): after B's attach the daemon on A is stopped and started again without an attach; then again with the saved attach pointing at a socket nobody listens on, followed by an attach from B; the locked service start on B finds B's attach saved. An attach before the start (D102 amended 2026-11-14): with no daemon, an attach from A under a runtime directory whose `bus` is A, then one from B; then a daemon on A starts | The daemon mounts, stays up and waits (`status`: needs login, "waits for the keyring"); the stored tree lists, a clean read fails, a write queues; `login` says the keyring is still unavailable; A is "already the daemon's bus", the others are refused; after B's attach one notification, sent on B; without the cache key the daemon waits ("no cache key"), the timer does not retry, no key is made and the disk cache's blocks stay; with it the attach brings the mount online, the cache key moves into B's keyring, the queue drains, clean content and the disk cache work again; the locked service start is ready and waiting within 15 s with no unlock dialog, the stop during the login's dialog takes under 15 s, and the timer brings the second start online once the collection is unlocked, without a dialog; in the opposite layout the attach reports the keyring and notifications on C until gnome-keyring owns its name on A, then the keyring on the daemon's own bus (log line, `status` `Bus:` line), the cache key moves into A's keyring and is read from it on the second run, and each wait's one notification lands on C; the attach with a display says it was set on the daemon's own bus (log line too), and a probe that A starts by D-Bus activation gets C's `DISPLAY` and `XAUTHORITY`; the attach without them passes none. Restart: the attach file exists with mode 0600 and B's address; the restarted daemon logs "using the desktop's session bus saved by the last attach", `status` shows `Bus: desktop's bus attached; keyring: none found, notifications: the desktop's bus`, its notifications go to B and the file stays; the file of the gone bus is removed and logged, no `Bus:` line, and the attach from B saves it again; the service start on B removes the file of its own bus; the restart after C's attach with a display goes online and passes the display to A again. Attach before the start: A's prints "nothing to attach" and saves nothing; B's prints "its next start takes this session's bus" and saves the file (0600); the daemon started on A logs the saved bus, shows the `Bus:` line and sends its notification to B. From that start the profile has a further root (D110 amended): it shows as `waits for the keyring`, its `mount` is refused, and once the keyring answers it mounts by itself (log line) and answers |
| X14 | Fake, a drive and profile of its own (M5.10a, D110): one `test` root with three trees, `work` on the own volume with its gate open, `shared` on another user's volume with the viewer role, and `missing` with a root that does not exist; a second root whose only tree does not exist. Writes in each tree and the top level; `unmount <root>` with 64 KiB pending over a 4 KiB/s upload, with `status`, a second `unmount` and a `mount` meanwhile; `mount -o ro <root>`, then a plain `mount`; `fusermount3 -u <root>`, a later `mount`, then again with the `mount` at once; the daemon's stop; a daemon started with `--service openrc --notify-fd 3` with a 1.5 s round trip per call, then `unmount <root>` and `mount <root>` under it, then SIGTERM | `status` shows the root `mounted` with `work: writable`, `shared: read-only`, `missing: not opened: ...`, and the second root `failed: no tree opened` (also in the log); the top level has `shared` and `work`; writes in `work` upload, the others get EROFS (a move into `shared` too); during the drain `status` answers within 3 s with the root `unmounting: uploading first` and the pending upload, the second `unmount` and the `mount` are refused (`is being unmounted`), My Files works, and the unmount prints the pending count, then `Unmounted`, with the file on the drive; after it the root is `not mounted`, the daemon (same pid) and My Files go on; under `-o ro` reads work and writes get EROFS (`mounted, read-only`), a second `mount` says `is already mounted`, the plain `mount` is writable; the outside unmount shows as `not mounted (unmounted from outside)` within 15 s (log line) and both later mounts work with the content; the stop leaves no mount; the service daemon's readiness comes while the root is still `mounting` or `not mounted`, the root mounts after it, and `unmount <root>` leaves the daemon and My Files running |

X12 is `x12.sh` (live, fake only: the marker is outside `pdmount-test/`, D028). X13 is `x13.sh` (live, fake only, a profile of
its own; without gnome-keyring the unlock part is skipped). X14 is `x14.sh`, X15 `x15.sh`, X16 `x16.sh` and X17 `x17.sh` (live, fake only, a
drive and profiles of their own). X18 is `x18.sh` (live, fake only, the scenario profile). X19 is `x19.sh` and X20 `x20.sh` (live, fake only, a drive and profile of their own). X21 is run by the user; its script's
steps are tested without a desktop by `tests/scenarios/m6desktop.sh` (fake, two private buses, the stubs; run by
`scripts/ci.sh`).

**X8 in a share's tree (M5.14a, D114):** `TARGET=share` in `tests/scenarios/lib.sh` (fake only) runs a scenario in a share
instead of My Files: a fake drive of its own with the share `pdmount-share` of another user, the drive's user an editor;
the profiles `fake-share` and `fake-share-2` mount it in a Shared root, and `TEST_ROOT`, `ROOT_A` and `ROOT_B` point to
`<share>/pdmount-test`. `remount`, `crash` and `fresh` drain the share's queue first or unmount its mount too. The A
scenarios skip there (path commands reach My Files only, D110), and the M scenarios read their conflict kinds from the
daemon's log. `tests/pjdfstest.sh` and `tests/fsx.sh` take it too.

---

## 16. Milestones

Exit criteria are functional tests. A milestone is done when its listed tests pass.

| Milestone | Scope | Exit criteria |
|---|---|---|
| **M0 Foundations** | Workspace; SDK fork with P1, P2, P4, P8; `Backend` trait and fake; local CI script that builds `gopenpgp-sys` (Go ≥ 1.22, clang, protoc); `login`/`logout`/`status`/`doctor` | `pdmount login` opens the browser **and** `login --no-browser` prints the link; both complete sign-in. `status` shows the account and quota. Fake-backend unit tests pass. `doctor` passes on the Gentoo machine |
| **M1 Read-only mount** | Inode table, store, listing, block cache (P3), readahead and prefetch, kernel invalidation, `catch_unwind`, deadlines, unmount tooling; writes return EROFS | **R6** and **G6 read side** (clone, fsck) on data uploaded by the official client, in `live` and `fresh` modes. This confirms API size, mtime, and content. sha256 of 5 MB / 41 MB / 2 GB files equals the web download. `hot_path_network_calls == 0`. `kill -9` → `unmount --force` → remount without a reboot |
| **M2 Writes, single device, access control** | Staging, journal, §7–§8, `fsync=local`, `.pdignore`, quota errors, metadata revisions, P5, P6, P10; access rules (§11); event engine for clean inodes with refresh and echo suppression (§9.1–§9.5, §7.5; D033) | **R1–R7, G1–G7, N1, A1–A6, E1** pass in `live`, `remount`, `crash`, `fresh` on the fake (CI), and all but A5 and E1 (which need a second client) in `live`, `remount`, `fresh` on the real account. pjdfstest subset green and fsx 6 h clean, both on the fake (D030); journal property tests at 10k cases |
| **M3 Multi-device and sync robustness** | Remote changes to dirty inodes, the full conflict engine, offline mode, auth refresh and `needs_login`, P9, P11, P12; verify server precondition behaviour (§10.4) | **M1–M7** on the fake, and the cases marked "real" in §15.3 (M1, M3, M6, the file part of M7) on two real profiles: M2, M4, M5 and the folder and app parts of M7 need fault injection or a pause at the commit (D051). Every R/G/A scenario in `second` mode on two real profiles. Zero data loss. Remote changes visible within `sync.poll_interval` + 10 s of the writer's commit, 40 s at the default (D037, D050). 3 h soak on the fake passes (D030, D048) |
| **M4 Performance and operations** | SLO instrumentation, benchmarks, P7 parallel uploads, pins, writeback-cache experiment, `cache gc`, queue tooling, autostart entry (D059), documentation | §5.7 SLOs met (median of 5 rounds on a quiet disk, D086). Over the fake backend with an 80 ms RTT / 20 MB/s profile: `git status` on warm git.git ≤ 2× local btrfs; untar of 5k small files ≤ 15× local (time to acknowledgement; D065); cached sequential read ≥ 500 MB/s; `cache gc` keeps the cache ≤ 1.05 × the cap, and ≤ the cap after gc (definitions: D057) |
| **M5 User services, flush and further roots** | OpenRC and systemd user services (D074); flush on SIGTERM and at logind's PrepareForShutdown, warning at the next start (D075, D091); pending marker (D076); Shared-with-me and Photos (D078; Computers out of scope, D122); read-write for writable shares, Photos read-only (D080); bulk-read guard and Photos thumbnails (D081); release archive with cargo-dist and GitHub CI (D097) | **X1–X4**, **X7**, **X8** and **X11** on the fake; **X10** through the existing tests (x17's thumbnails, the bulk-read guard's mounted and unit tests; D123); **X5** by hand on Gentoo (OpenRC) and Debian (systemd); **X7** read-only on the real account; **X9** if the user grants the D028 exception; pjdfstest subset and a short fsx on a share tree on the fake; no regression in the §16 M2 scenarios on the fake |
| **M6 Conflict notifications** | Named, actionable D-Bus notifications; missed ones shown later (D077); the slow-download notification (D137). A resolution UI is not decided | Automated, in `scripts/ci.sh` on the fake with stubs on private buses (skipped without `dbus-daemon`): **X6** and **X20**, and the test of X21's script. By hand on the user's desktops, GNOME and KDE: **X21**, not run for M6, risk accepted by the user (D142) (**Show** opens the folder with the copy selected; conflicts made while no notification service ran appear at the next start and at the attach; the downloading notification and its close; each `[notify]` key). Security review of the notifications with nothing above Low open (2026-11-20) |
| **M7 Upstream and crypto migration** | Upstream patches; track Proton's new crypto model; later fork rebases (the first one, onto 0.5.1, is M6.0.1, D099) | Fork diff reduced to unmerged patches; migration plan documented; full matrix re-run |

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
| A node whose signatures do not verify, in a tree in `refuse` mode and not accepted: open (also a folder's), rename, move, delete, replace, chmod, truncate, utimes; a create in, a move into or a rename within a folder whose key did not verify (§6.3, D132) | EACCES; `stat` and listings work |
| `Quota` (200001, 200002, 200100, 200101) | ENOSPC (deferred to flush/fsync/close for async uploads) |
| `FileTooLarge` (200003) | EFBIG |
| A write or truncation of a remote file larger than `MAX_STAGED_SIZE` (4 TiB, §7.1) | EFBIG |
| Code 2000 on a rename or move | Decided by the node's state (§8.3): `NotFound`, `NameConflict`, `InvalidMove` or `AlreadyExists`; by the message ("out of date", "sub-folder") only when the reads fail |
| `NameConflict` (code 2000: a rename or move with a stale `OriginalHash`, or an unchanged node whose hash was refused) | The journal fetches the node again (§8.3); never an errno |
| `InvalidMove` (code 2000: a folder moved into itself or a descendant) | EINVAL (not retryable) |
| Commit refused because the draft was removed (code 2511) | `Transient`: the retry creates a new draft. A 2511 whose revision is already active or obsolete (an earlier attempt went through, its answer lost) is a successful commit in the fork (P22), with the draft's revision id, which step 7 of §7.4 records in `own_revisions` |
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
| An address whose primary key does not unlock (P28: no other key signs in its place; the SDK's message) | `Other`: changes that sign (uploads, folders, renames) are parked after five attempts |
| `Integrity` (hash or signature mismatch; a block that decrypts to another size than claimed) | EIO; nothing cached; ERROR log |
| `Integrity` in an upload (a block that fails its check before upload twice, `IntegrityException`, P20; a size or checksum mismatch at the commit) | the journal retries with backoff and parks the entry after five attempts, as other permanent errors |
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
max_bytes = "10GiB"             # the profile's disk cap: My Files and every further tree together; My Files
                                # keeps at least half (D110 amended, §14.5)
ram_bytes = "32MiB"             # at least 16MiB; the block pool, reserved at startup (§6.2)
prefetch_small_files = "256KiB"
prefetch_dir_budget = "16MiB"

[scan]                          # the bulk-read guard, all trees together (§6.4, D081, D111)
opens = 10                      # different files opened within `window` that begin a scan; 0 = off, else at least 2
window = "5s"                   # more than 0s, at most 60s
first_bytes = "8MiB"            # read through a handle opened during a scan before its reads are interactive
interval = "100ms"              # spacing of the starts of scan downloads, one at a time; at most 10s
cache_share = 10                # percent of cache.max_bytes that blocks of scan reads may fill (0-100)

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
pending_marker = false          # leave .pdmount/<device id>.pending when a stop's grace runs out with changes left (D076)

[timeouts]
metadata = "20s"
block = "60s"
offline_read = "2s"
shutdown_grace = "90s"          # flush at a stop and wait for running uploads (D062, D075, D091)

[stats]
prometheus_textfile = ""        # e.g. "~/node-exporter/pdmount.prom"; empty = off (§5.8)

[photos]
thumbnail_cache = true          # a photos root writes Proton's thumbnails of the months opened to
                                # $XDG_CACHE_HOME/thumbnails (§14.5, D081, D115)

[notify]                        # desktop notifications (§10.8, §14.4, D077, D137, D139)
conflicts = true                # one per conflict to look at (state open), with a Show action, also for one recorded
                                # while no notification service ran (D140); false = none, and none later either
conflict_names = true           # false: a conflict's body names neither the file nor its tree (a lock screen); other
                                # notifications still name a share or a file (D139): set downloads = false as well
downloads = true                # name a file whose read has waited 5 s for its blocks, in any tree (D137)

signatures = "refuse"           # My Files' nodes whose signatures do not verify (§6.3, D132): refuse (EACCES until
                                # `pdmount signatures accept`) | warn (listed and announced only). A top-level key:
                                # put it before the first [table]

# Further roots (§14.5, D079, D110), each on its own mountpoint; none by default. Mountpoints may not overlap
# My Files' or each other.
# [[roots]]
# kind = "shared"               # shared: one tree per share (M5.12, D112), at most one; photos: the Photos timeline
#                               # by year and month, read-only (M5.13, D113), at most one; computers: refused
#                               # (D122); test: test builds only
# mountpoint = "~/ProtonDrive-Shared"
# read_only = false             # like `mount -o ro`, for this mount (a photos root is read-only anyway)
# pending_marker = false        # leave .pdmount/<device id>.pending in each writable tree's root when a stop's grace
#                               # runs out with its changes left (D076, D114); in a share it names no device
# signatures = "warn"           # refuse | warn (§6.3, D132); default: warn for shared, refuse for photos, for a test
#                               # tree refuse on the user's own volume and warn on another's
# Only kind = "test", with a fake drive (`pdmount fake root` prints volume and root):
# [[roots.tree]]
# name = "Team"                 # the tree's folder in a mount with several trees
# volume = "vol-1"
# root = "link-2"
# writable = false              # opens the tree's write gate
```
