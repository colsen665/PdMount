//! The filesystem engine behind the FUSE adapter (DESIGN.md §3.4, §4.2, §5,
//! §7, §8).
//!
//! Methods without `async` are hot-path operations: they use RAM and local
//! disk only, and return an error for which [`CoreError::is_slow`] holds when
//! the slow path must run first (a listing, base blocks to materialize, more
//! inode numbers). `async` methods may call the backend and run on tokio
//! tasks.
//!
//! Local changes are applied to the tree at once, persisted through the
//! store's group commit and handed to the journal executor
//! ([`crate::journal`]), which reconciles them with the remote state in the
//! background. After a remount the stored tree is served at once and the
//! event task ([`crate::events`]) resumes the event stream from the stored
//! cursor (D034). Listed directories are re-listed in the background
//! ([`Engine::revalidate`]) only when the server asks for a `refresh` or the
//! profile has no cursor.
//!
//! # Lock order
//!
//! Synchronous locks, outermost first; a lock is taken only while holding
//! locks above it ([`crate::lock_order`] checks this in debug builds):
//!
//! 1. the staging of one inode (one at a time);
//! 2. the staged map (`staged`);
//! 3. the access rules;
//! 4. the tree (no recursive read: a waiting writer blocks the second);
//! 5. leaves, which take nothing while held: journal entries, the conflict
//!    registry, the `.pdignore` rules. Queueing a store write
//!    ([`Store::write`]: a short mutex and an unbounded channel send, no
//!    wait) is below every lock; the journal does it under its lock so the
//!    `queued` rows follow the entries (D105).
//!
//! The other mutexes (handles, quota, inode numbers, refresh state, ...)
//! guard one field each and take nothing while held. None of these locks
//! is held across an `.await`. The block caches of a profile's trees share a
//! disk cap ([`crate::cache::DiskBudget`], D110): while held, its enforcement
//! lock takes only the caches' index and protection locks and queues store
//! writes, never an engine lock; it is taken before any cache's index lock,
//! never while one is held, so a cache asks another one to evict holding
//! nothing of its own. Asynchronous locks, held across the backend
//! calls they serialize, come before all of them: the event cursor (a
//! volume's poller holds the cursors of all its trees, taken in tree order,
//! D110), then a background turn of the [`Gate`], which a holder never asks
//! for again (it would wait for itself); `reserving` before `inos`. A
//! scan read's turn of the session's [`ScanGuard`] is taken outside any
//! download flight, so a read that joins a flight never waits for a turn;
//! the guard's own mutexes are leaves.
//!
//! A guard in a call's arguments, or in an `if let` or `match` scrutinee,
//! lives until the call or the whole block ends.
//!
//! # Module map
//!
//! `engine.rs` (here) holds the constants, the option and view types, the
//! [`Engine`] struct and this lock order. The methods of `impl Engine` are in
//! the submodules, grouped by job:
//!
//! - `open.rs`: [`Engine::open`], first-run initialization, loading the stored
//!   tree, [`pending_changes`].
//! - `accessors.rs`: accessors of the engine's parts, the conflict registry,
//!   `after_commit`, `retire`, `storage`, `stamp`.
//! - `background.rs`: the background task set, the health loop, relisting,
//!   revalidation, storage refresh and the queued-work restart.
//! - `shutdown.rs`: `shutdown`, `shutdown_by` and their deadline.
//! - `cache_glue.rs`: cache usage, gc, wipe and the orphan sweep.
//! - `namespace.rs`: hot-path lookups, attributes and namespace changes
//!   (`lookup`, `create`, `rename`, `setattr`, ...).
//! - `io.rs`: file content (`open_file`, `read`, `write`, `truncate`, `flush`,
//!   `fsync`) and the read helpers.
//! - `listing.rs`: slow-path listings, applying remote changes and resolving
//!   vanished nodes (the events glue).
//! - `staging.rs`: staged files, the quota check, materializing base blocks,
//!   reading a revision.
//! - `access.rs`: the write gate, permission checks and the access rules.
//! - `ignore.rs`: `.pdignore` rules and reclassification.
//! - `inos.rs`: inode number allocation and reservation.
//! - `prefetch.rs`: readahead and directory prefetch.
//! - `scan.rs`: the engine's side of the bulk-read guard
//!   ([`crate::scan`], D111): classing opens and reads, waits of
//!   interactive requests, the turns of scan reads.
//! - `rights.rs`: write access a tree can lose (a share's role, D114).
//! - `orphan.rs`: exporting the queued changes of a tree that goes away.
//! - `marker.rs`, `pins.rs`, `queue.rs`, `surface.rs`: markers, pins, queue
//!   tooling, conflict and xattr views (existing). `tests.rs` and
//!   `proptests.rs` are the tests.

use crate::Config;
use crate::access::{Rule, RuleSource, RuleView, Rules};
use crate::cache::{
    BlockCache, BlockKey, CacheOptions, DiskStats, Evicted, Layout, Priority, READ_BLOCKS_HELD,
    ReadData, ReadFrom,
};
use crate::config::AccessMode;
use crate::conflicts::{
    Clock, Conflict, ConflictKind, ConflictNote, ConflictNotice, ConflictState, Conflicts, Namer,
    store_conflict, store_state,
};
use crate::device::DeviceIdentity;
use crate::errors::{CoreError, Result};
use crate::gate::Gate;
use crate::health::{Connectivity, Health, Monitored};
use crate::journal::Journal;
use crate::limits::SessionLimits;
use crate::lock_order::{self, Rank};
use crate::metrics::{Metrics, TimedBackend, check_not_hot};
use crate::pdignore::IgnoreMatcher;
use crate::rows::{load_rows, write_rows};
use crate::scan::ScanGuard;
use crate::singleflight::SingleFlight;
use crate::staging::{Base, Piece, Staged, StagingDir};
use crate::store::{Store, meta_get, meta_set};
use crate::vfs::{
    ApplyError, Attr, Changes, Content, DirEntry, Ino, Invalidation, Lookup, NodeRow, ROOT_INO,
    RemoteState, RenameMode, Tree,
};
use futures::StreamExt;
use pdmount_backend::{Backend, EventId, LinkId, NodeKind, RemoteNode, RevisionId, StorageInfo};
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};
use tokio::sync::mpsc;

/// Volume key of the My Files event cursor (§9.1).
pub(crate) const MY_FILES_VOLUME: &str = "my-files";
/// Inode numbers reserved durably per store write (§8.8).
const INO_RESERVATION: u64 = 16_384;
const LISTING_PREFETCH_LIMIT: usize = 32;
/// Quota refresh; the write pre-check uses it (§7.8).
const STATFS_REFRESH: Duration = Duration::from_secs(60);
/// How long the sum of pending upload bytes is reused.
const PENDING_BYTES_TTL: Duration = Duration::from_secs(1);
const ACCESS_FLUSH: Duration = Duration::from_secs(60);
/// `meta` key of the directories to list again (D069), space-separated.
const RELIST_KEY: &str = "relist";
/// Staging removals wait this long to share one store fsync: each fsync
/// stalls the WAL writes that FUSE replies wait for (D032).
const STAGING_REMOVAL_DELAY: Duration = Duration::from_secs(1);
/// Plans of a staged read before giving up on writes that keep moving its
/// base pieces.
const STAGED_READ_ATTEMPTS: usize = 8;
/// The largest file that can be staged: 2^20 blocks of 4 MiB, 4 TiB (§7.1).
/// Above it, writes to a remote file fail with EFBIG.
pub const MAX_STAGED_SIZE: u64 = crate::cache::BLOCK_SIZE << 20;

pub struct EngineOptions {
    pub state_db: PathBuf,
    pub staging_dir: PathBuf,
    /// The profile's own `.pdignore` rules (§7.7); a missing file is empty.
    pub local_ignore: Option<PathBuf>,
    /// Whether the tree root's `/.pdignore` adds to `local_ignore` (§7.7,
    /// D124): yes in My Files and other trees on the user's own volume; no
    /// in a tree on another user's volume (a share), where that file is an
    /// ordinary file whose rules other users write.
    pub remote_ignore: bool,
    pub cache: CacheOptions,
    pub config: Config,
    /// This device, named in conflict copies (§10.2, §10.7).
    pub device: DeviceIdentity,
    /// The time source for conflict names.
    pub clock: Clock,
    /// `mount -o ro`: every write path answers EROFS; pending work uploads.
    pub read_only: bool,
    /// The tree's write gate at the start (D080, D110): closed, every write
    /// path answers EROFS as under `mount -o ro`, but it can open and close
    /// at runtime ([`Engine::set_write_gate`]). My Files' is open.
    pub write_gate: bool,
    /// Connectivity shared with the other trees of the session (§9.6,
    /// D110). `None`: the engine has its own and probes it; given, the
    /// engine that made it probes and this one only follows it.
    pub health: Option<Arc<Health>>,
    /// The session's bulk-read guard, shared by every tree (D081, D111).
    /// `None`: the engine has its own, from `config.scan` and its cache cap.
    pub scan: Option<Arc<ScanGuard>>,
    /// The session's download, upload and background limits, shared by
    /// every tree (D033, D110). `None`: the engine has its own, from
    /// `config`.
    pub limits: Option<Arc<SessionLimits>>,
}

impl EngineOptions {
    /// State, staging and block cache all in `dir`, no profile `.pdignore`,
    /// 16 MiB of RAM cache and 64 MiB of disk cache (tests and benchmarks).
    pub fn in_dir(dir: &Path, config: Config, device: DeviceIdentity, clock: Clock) -> Self {
        Self {
            state_db: dir.join("state.db"),
            staging_dir: dir.join("staging"),
            local_ignore: None,
            remote_ignore: true,
            cache: CacheOptions {
                dir: dir.join("blocks"),
                master_key: Some([7; 32]),
                ram_bytes: 16 << 20,
                disk_bytes: 64 << 20,
                shared: None,
                downloads: None,
            },
            config,
            device,
            clock,
            read_only: false,
            write_gate: true,
            health: None,
            scan: None,
            limits: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatFs {
    pub blocks: u64,
    pub bfree: u64,
    pub bavail: u64,
    pub files: u64,
    pub ffree: u64,
    pub bsize: u32,
    pub namelen: u32,
}

/// Permission bits a request needs (owner bits: only the mounting user
/// reaches the mount).
pub const MAY_READ: u32 = 4;
pub const MAY_WRITE: u32 = 2;
pub const MAY_EXEC: u32 = 1;

/// How a file is opened.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OpenMode {
    pub read: bool,
    pub write: bool,
    /// `O_TRUNC` with `FUSE_ATOMIC_O_TRUNC`: start empty, fetch nothing.
    pub truncate: bool,
}

/// Attribute changes of `setattr` (§8.5).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SetAttr {
    pub mode: Option<u32>,
    pub size: Option<u64>,
    pub mtime: Option<SystemTime>,
    /// Kept locally only.
    pub atime: Option<SystemTime>,
    /// The handle the kernel passed (`ftruncate`): one opened for writing
    /// allows a truncation whatever the mode says now.
    pub handle: Option<u64>,
}

/// Open directory handles, and per directory a digest of the entries the
/// kernel was last given (`FOPEN_KEEP_CACHE`, §5.4).
#[derive(Default)]
struct DirHandles {
    open: HashMap<u64, (Ino, Arc<Vec<DirEntry>>)>,
    served: HashMap<Ino, u64>,
}

/// An open file handle.
#[derive(Debug, Clone)]
struct OpenFile {
    ino: Ino,
    writable: bool,
    /// End of the previous read, for sequential detection.
    last_end: u64,
    sequential_reads: u32,
    /// Opened during a scan (D111): its first `scan.first_bytes` are scan
    /// reads.
    scan: bool,
    /// Bytes read through the handle so far.
    read_bytes: u64,
}

struct InoAllocator {
    next: Ino,
    reserved: Ino,
}

/// What a read of a file needs.
enum ReadSource {
    Remote {
        link: LinkId,
        rev: Option<RevisionId>,
        size: u64,
    },
    Staged(Arc<StagingLock>),
}

pub struct Engine {
    /// The backend as the engine sees it: watched by [`Monitored`].
    pub(crate) backend: Arc<dyn Backend>,
    monitored: Arc<Monitored>,
    pub(crate) health: Arc<Health>,
    pub(crate) store: Arc<Store>,
    pub(crate) metrics: Arc<Metrics>,
    pub(crate) cache: Arc<BlockCache>,
    pub(crate) config: Config,
    device: DeviceIdentity,
    clock: Clock,
    pub(crate) tree: lock_order::RwLock<Tree>,
    inos: parking_lot::Mutex<InoAllocator>,
    reserving: tokio::sync::Mutex<()>,
    /// Per directory: the listing in flight, which answers with the number
    /// it took from `listing_epoch` when it started (D054).
    listings: SingleFlight<Ino, (u64, Result<()>)>,
    listing_epoch: AtomicU64,
    pub(crate) invalidations: mpsc::UnboundedSender<Invalidation>,
    dirs: parking_lot::Mutex<DirHandles>,
    files: parking_lot::Mutex<HashMap<u64, OpenFile>>,
    next_handle: AtomicU64,
    pub(crate) staging: StagingDir,
    pub(crate) staged: Arc<lock_order::Mutex<StagedMap>>,
    /// Staging to delete once the store rows naming it are durable.
    staging_removals: Arc<parking_lot::Mutex<Vec<Ino>>>,
    pub(crate) journal: Journal,
    storage: parking_lot::Mutex<Option<StorageInfo>>,
    /// Counts remote changes made by the executor; listings remember it at
    /// fetch time (`Tree::apply_listing_at`).
    remote_clock: AtomicU64,
    /// Bytes waiting for upload, when they were summed, and the growth
    /// accepted since.
    pending_bytes: parking_lot::Mutex<Option<(std::time::Instant, u64, u64)>>,
    prefetched_dirs: parking_lot::Mutex<HashSet<Ino>>,
    /// `.pdignore` rules: the profile's file, then the mount root's (only
    /// with `remote_ignore`, D124).
    ignore: lock_order::RwLock<Arc<IgnoreMatcher>>,
    local_ignore: String,
    /// [`EngineOptions::remote_ignore`].
    remote_ignore: bool,
    /// Asks the background task to reload the rules.
    ignore_changed: tokio::sync::Notify,
    /// Access rules (§11).
    access: lock_order::RwLock<Rules>,
    /// The conflict registry (§10.8).
    conflicts: lock_order::Mutex<Conflicts>,
    /// Newly registered conflicts, for the notification hook.
    notices: tokio::sync::broadcast::Sender<ConflictNotice>,
    /// The session's background turns (D027), from `limits`.
    pub(crate) gate: Arc<Gate>,
    /// The session's transfer limits, shared by every tree (D110).
    limits: Arc<SessionLimits>,
    /// The session's bulk-read guard (D111) and this tree's id on it.
    pub(crate) scan: Arc<ScanGuard>,
    scan_tree: u64,
    /// Files and folders kept offline (§5.3).
    pub(crate) pins: pins::Pins,
    /// Pending markers: this device's and other devices' (D076).
    pub(crate) markers: marker::Markers,
    /// Where the event stream continues (§9.1); held while syncing.
    pub(crate) event_cursor: tokio::sync::Mutex<Option<EventId>>,
    pub(crate) refresh: parking_lot::Mutex<crate::events::RefreshState>,
    /// When the event stream was last read to its end.
    pub(crate) last_poll: parking_lot::Mutex<Option<std::time::Instant>>,
    /// `mount -o ro`.
    read_only: bool,
    /// The write gate (D080, D110): open, writes follow the access rules;
    /// closed, they answer EROFS.
    write_gate: std::sync::atomic::AtomicBool,
    /// Whether this engine made its `health` and so probes it.
    owns_health: bool,
    /// `fsync.remote_paths`, compiled.
    fsync_remote: globset::GlobSet,
    background: parking_lot::Mutex<Vec<tokio::task::JoinHandle<()>>>,
    /// Whether a refusal of the server closes the write gate (D114).
    pub(crate) rights: rights::Rights,
    /// Set by [`Engine::end_shutdown_wait`]: the shutdown waits no longer
    /// for running steps.
    shutdown_wait_ended: std::sync::atomic::AtomicBool,
    /// Set by [`Engine::retire`]: the tree left its mount for good, so a
    /// change's reply after it answers EIO ([`Engine::after_commit_kept`]).
    retired: Arc<std::sync::atomic::AtomicBool>,
    runtime: tokio::runtime::Handle,
}

fn now() -> SystemTime {
    SystemTime::now()
}

type StagedMap = HashMap<Ino, Arc<StagingLock>>;
/// The lock of one inode's staging.
pub(crate) type StagingLock = lock_order::Mutex<Staged>;

mod access;
mod accessors;
mod background;
mod cache_glue;
mod ignore;
mod inos;
mod io;
mod listing;
pub mod marker;
mod namespace;
mod open;
pub mod orphan;
pub mod pins;
mod prefetch;
#[cfg(test)]
mod proptests;
pub mod queue;
pub mod rights;
mod scan;
mod shutdown;
mod staging;
mod surface;
#[cfg(test)]
mod tests;

pub use open::pending_changes;

use staging::remove_committed_staging;
