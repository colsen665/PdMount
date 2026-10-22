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
//! background. After a remount the stored tree is served at once while
//! every listed directory is re-listed in the background
//! ([`Engine::revalidate`]); event polling follows in M3.
//!
//! Lock order: a per-inode staging lock may be taken before the tree lock,
//! never while holding it.

use crate::Config;
use crate::access::{Rule, RuleSource, RuleView, Rules};
use crate::cache::{BlockCache, BlockKey, CacheOptions, Layout, Priority, assemble};
use crate::config::AccessMode;
use crate::errors::{CoreError, Result};
use crate::gate::Gate;
use crate::journal::Journal;
use crate::metrics::{Metrics, check_not_hot};
use crate::pdignore::IgnoreMatcher;
use crate::rows::{load_rows, sql_ino, write_rows};
use crate::singleflight::SingleFlight;
use crate::staging::{Base, Piece, Staged, StagingDir};
use crate::store::{Store, meta_get, meta_set};
use crate::vfs::{
    ApplyError, Attr, Changes, Content, DirEntry, Ino, Invalidation, Lookup, NodeRow, ROOT_INO,
    RemoteState, RenameMode, Tree,
};
use bytes::Bytes;
use pdmount_backend::{Backend, EventId, LinkId, NodeKind, RevisionId, StorageInfo};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};
use tokio::sync::mpsc;

/// Volume key of the My Files event cursor (§9.1).
pub(crate) const MY_FILES_VOLUME: &str = "my-files";
/// Inode numbers reserved durably per store write (§8.8).
const INO_RESERVATION: u64 = 16_384;
/// Spacing of speculative API requests (prefetch, revalidation): at most
/// two per second, one at a time, and none while a user request waits (D027).
const BACKGROUND_INTERVAL: Duration = Duration::from_millis(500);
const LISTING_PREFETCH_LIMIT: usize = 32;
/// Quota refresh; the write pre-check uses it (§7.8).
const STATFS_REFRESH: Duration = Duration::from_secs(60);
/// How long the sum of pending upload bytes is reused.
const PENDING_BYTES_TTL: Duration = Duration::from_secs(1);
const ACCESS_FLUSH: Duration = Duration::from_secs(60);
/// Staging removals wait this long to share one store fsync: each fsync
/// stalls the WAL writes that FUSE replies wait for (D032).
const STAGING_REMOVAL_DELAY: Duration = Duration::from_secs(1);

pub struct EngineOptions {
    pub state_db: PathBuf,
    pub staging_dir: PathBuf,
    /// The profile's own `.pdignore` rules (§7.7); a missing file is empty.
    pub local_ignore: Option<PathBuf>,
    pub cache: CacheOptions,
    pub config: Config,
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

/// An open file handle.
#[derive(Debug, Clone)]
struct OpenFile {
    ino: Ino,
    writable: bool,
    /// End of the previous read, for sequential detection.
    last_end: u64,
    sequential_reads: u32,
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
    Staged(Arc<parking_lot::Mutex<Staged>>),
}

pub struct Engine {
    pub(crate) backend: Arc<dyn Backend>,
    pub(crate) store: Arc<Store>,
    pub(crate) metrics: Arc<Metrics>,
    pub(crate) cache: Arc<BlockCache>,
    pub(crate) config: Config,
    pub(crate) tree: parking_lot::RwLock<Tree>,
    inos: parking_lot::Mutex<InoAllocator>,
    reserving: tokio::sync::Mutex<()>,
    listings: SingleFlight<Ino, ()>,
    pub(crate) invalidations: mpsc::UnboundedSender<Invalidation>,
    dirs: parking_lot::Mutex<HashMap<u64, Arc<Vec<DirEntry>>>>,
    files: parking_lot::Mutex<HashMap<u64, OpenFile>>,
    next_handle: AtomicU64,
    pub(crate) staging: StagingDir,
    pub(crate) staged: Arc<parking_lot::Mutex<StagedMap>>,
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
    /// `.pdignore` rules: the profile's file, then the mount root's.
    ignore: parking_lot::RwLock<Arc<IgnoreMatcher>>,
    local_ignore: String,
    /// Asks the background task to reload the rules.
    ignore_changed: tokio::sync::Notify,
    /// Access rules (§11); taken before the tree lock, never while holding it.
    access: parking_lot::RwLock<Rules>,
    pub(crate) gate: Gate,
    /// Where the event stream continues (§9.1); held while syncing.
    pub(crate) event_cursor: tokio::sync::Mutex<Option<EventId>>,
    pub(crate) refresh: parking_lot::Mutex<crate::events::RefreshState>,
    background: parking_lot::Mutex<Vec<tokio::task::JoinHandle<()>>>,
    runtime: tokio::runtime::Handle,
}

/// [`Engine::remove_staging_after_commit`]: takes the queue before the flush,
/// so every inode taken had its rows queued before the flush.
async fn remove_committed_staging(
    store: Arc<Store>,
    staging: StagingDir,
    staged: Arc<parking_lot::Mutex<StagedMap>>,
    removals: Arc<parking_lot::Mutex<Vec<Ino>>>,
) {
    let inos = std::mem::take(&mut *removals.lock());
    if inos.is_empty() {
        return;
    }
    if let Err(error) = store.flush().await {
        // Startup deletes the staging of nodes that are gone.
        tracing::warn!(%error, count = inos.len(), "staging kept: the store did not commit");
        return;
    }
    // Locked per inode: `create` waits for one removal, not the batch.
    for ino in inos {
        let staged = staged.lock();
        if !staged.contains_key(&ino) {
            staging.remove(ino);
        }
    }
}

fn now() -> SystemTime {
    SystemTime::now()
}

impl Engine {
    /// Opens the store, staging and cache. On the first mount of a profile
    /// it records the event cursor and the root (network); otherwise it uses
    /// only the stored state. Must run inside a tokio runtime.
    pub async fn open(
        backend: Arc<dyn Backend>,
        options: EngineOptions,
    ) -> Result<(Arc<Self>, mpsc::UnboundedReceiver<Invalidation>)> {
        let store = Arc::new(Store::open(&options.state_db)?);
        let metrics = Arc::new(Metrics::default());
        let account = backend.account().await?;
        let stored_root = store.meta_get("root_link")?;
        match stored_root {
            Some(root) if root != account.root.0 => {
                return Err(CoreError::Store(format!(
                    "{} belongs to another account or drive; run `pdmount reset --keep-login`",
                    options.state_db.display()
                )));
            }
            Some(_) => {}
            None => initialize(&store, backend.as_ref(), &metrics).await?,
        }
        let staging = StagingDir::open(&options.staging_dir)?;
        let (tree, reserved, staged) = load(&store, &staging)?;
        let cursor = crate::events::load_cursor(&store)?;
        let access = store.read(|conn| Rules::load(conn, options.config.access.default))?;
        let cache = BlockCache::open(
            Arc::clone(&backend),
            Arc::clone(&store),
            Arc::clone(&metrics),
            options.cache,
        )?;
        let (sender, receiver) = mpsc::unbounded_channel();
        let journal = Journal::new(&options.config.upload);
        let local_ignore = options
            .local_ignore
            .as_ref()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .unwrap_or_default();
        let engine = Arc::new(Self {
            backend,
            store,
            metrics,
            cache: Arc::new(cache),
            config: options.config,
            tree: parking_lot::RwLock::new(tree),
            inos: parking_lot::Mutex::new(InoAllocator {
                next: reserved + 1,
                reserved,
            }),
            reserving: tokio::sync::Mutex::new(()),
            listings: SingleFlight::default(),
            invalidations: sender,
            dirs: parking_lot::Mutex::default(),
            files: parking_lot::Mutex::default(),
            next_handle: AtomicU64::new(1),
            staging,
            staged: Arc::new(parking_lot::Mutex::new(staged)),
            staging_removals: Arc::default(),
            journal,
            storage: parking_lot::Mutex::default(),
            remote_clock: AtomicU64::new(0),
            pending_bytes: parking_lot::Mutex::default(),
            prefetched_dirs: parking_lot::Mutex::default(),
            ignore: parking_lot::RwLock::new(Arc::new(IgnoreMatcher::new([local_ignore.as_str()]))),
            local_ignore,
            ignore_changed: tokio::sync::Notify::new(),
            access: parking_lot::RwLock::new(access),
            gate: Gate::new(BACKGROUND_INTERVAL),
            event_cursor: tokio::sync::Mutex::new(cursor),
            refresh: parking_lot::Mutex::default(),
            background: parking_lot::Mutex::default(),
            runtime: tokio::runtime::Handle::current(),
        });
        engine.queue_pending_work();
        Ok((engine, receiver))
    }

    pub fn metrics(&self) -> &Arc<Metrics> {
        &self.metrics
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn runtime(&self) -> &tokio::runtime::Handle {
        &self.runtime
    }

    /// Runs `reply` once the store rows the current request queued are
    /// committed, so a crash of the daemon loses nothing the kernel was told
    /// (§5.6). On the hot path that is what this thread queued; elsewhere,
    /// everything queued so far.
    pub fn after_commit(&self, reply: impl FnOnce() + Send + 'static) {
        let seq = if crate::metrics::is_hot() {
            Store::queued_here()
        } else {
            self.store.queued()
        };
        self.store.after(seq, move |result| {
            if let Err(error) = result {
                tracing::warn!(%error, "replying although the store did not commit");
            }
            reply();
        });
    }

    /// Disk cache bytes and entries.
    pub fn cache_usage(&self) -> (u64, usize) {
        self.cache.usage()
    }

    pub fn inode_count(&self) -> usize {
        self.tree.read().len()
    }

    /// Quota as last fetched.
    pub fn storage(&self) -> Option<StorageInfo> {
        *self.storage.lock()
    }

    /// Starts the event task (which also catches up after mounting), the
    /// journal executor, quota refresh and cache bookkeeping.
    pub fn start_background(self: &Arc<Self>) {
        let mut tasks = self.background.lock();
        let this = Arc::clone(self);
        tasks.push(self.runtime.spawn(async move { this.run_events().await }));
        let this = Arc::clone(self);
        tasks.push(self.runtime.spawn(async move { this.run_journal().await }));
        let this = Arc::clone(self);
        tasks.push(self.runtime.spawn(async move {
            loop {
                this.refresh_storage().await;
                tokio::time::sleep(STATFS_REFRESH).await;
            }
        }));
        let this = Arc::clone(self);
        tasks.push(self.runtime.spawn(async move {
            loop {
                tokio::time::sleep(ACCESS_FLUSH).await;
                this.cache.flush_access();
            }
        }));
        let this = Arc::clone(self);
        tasks.push(
            self.runtime
                .spawn(async move { this.keep_inos_reserved().await }),
        );
        let this = Arc::clone(self);
        tasks.push(
            self.runtime
                .spawn(async move { this.keep_ignore_rules().await }),
        );
        self.refresh_ignore();
        let this = Arc::clone(self);
        tasks.push(
            self.runtime
                .spawn(async move { this.import_config_rules().await }),
        );
    }

    /// Stops background work and commits the store. Pending journal work
    /// stays in the store and continues at the next mount.
    pub async fn shutdown(&self) -> Result<()> {
        for task in self.background.lock().drain(..) {
            task.abort();
        }
        self.cache.flush_access();
        let rows: Vec<Ino> = self.tree.read().inos().collect();
        self.persist(&rows);
        self.store.flush().await?;
        remove_committed_staging(
            Arc::clone(&self.store),
            self.staging.clone(),
            Arc::clone(&self.staged),
            Arc::clone(&self.staging_removals),
        )
        .await;
        Ok(())
    }

    // ---- hot path: lookups and attributes ----

    pub fn lookup(&self, parent: Ino, name: &[u8]) -> Result<Lookup> {
        let name = match crate::vfs::validate_name(name) {
            Ok(name) => name,
            // A name that cannot be valid UTF-8 cannot exist remotely.
            Err(CoreError::InvalidName | CoreError::InvalidArgument) => {
                return Ok(Lookup::Negative);
            }
            Err(error) => return Err(error),
        };
        self.ensure_allowed(parent, MAY_EXEC)?;
        let lookup = self.tree.write().lookup(parent, name)?;
        Ok(match lookup {
            Lookup::Found(attr) => Lookup::Found(self.masked(attr)),
            other => other,
        })
    }

    pub fn getattr(&self, ino: Ino) -> Result<Attr> {
        let attr = self
            .tree
            .read()
            .get(ino)
            .map(|node| node.attr())
            .ok_or(CoreError::NotFound)?;
        Ok(self.masked(attr))
    }

    pub fn forget(&self, ino: Ino, count: u64) {
        let droppable = self.tree.write().forget(ino, count);
        if droppable {
            self.drop_detached(ino);
        }
    }

    /// Opens a listed directory; `None` means it must be listed first.
    pub fn opendir(self: &Arc<Self>, ino: Ino) -> Result<Option<u64>> {
        self.ensure_allowed(ino, MAY_READ)?;
        let Some(entries) = self.tree.read().dir_entries(ino)? else {
            return Ok(None);
        };
        let handle = self.next_handle.fetch_add(1, Ordering::Relaxed);
        self.dirs.lock().insert(handle, Arc::new(entries));
        self.prefetch_dir(ino);
        Ok(Some(handle))
    }

    /// The snapshot taken by `opendir`.
    pub fn dir_snapshot(&self, handle: u64) -> Result<Arc<Vec<DirEntry>>> {
        self.dirs
            .lock()
            .get(&handle)
            .cloned()
            .ok_or(CoreError::NotFound)
    }

    /// Attributes for a readdirplus entry; adds a kernel reference.
    pub fn readdirplus_entry(&self, ino: Ino) -> Option<Attr> {
        let attr = self.tree.write().add_lookup(ino)?;
        Some(self.masked(attr))
    }

    pub fn releasedir(&self, handle: u64) {
        self.dirs.lock().remove(&handle);
    }

    /// Whether a directory has been listed (for `FOPEN_CACHE_DIR`).
    pub fn is_listed(&self, ino: Ino) -> bool {
        self.tree.read().get(ino).is_some_and(|node| node.listed)
    }

    pub fn statfs(&self) -> StatFs {
        const BSIZE: u64 = 4096;
        let storage = *self.storage.lock();
        let (max, used) = storage.map_or((0, 0), |s| (s.max, s.used.min(s.max)));
        let files = self.tree.read().len() as u64;
        StatFs {
            blocks: max / BSIZE,
            bfree: (max - used) / BSIZE,
            bavail: (max - used) / BSIZE,
            files,
            ffree: u64::from(u32::MAX),
            bsize: BSIZE as u32,
            namelen: crate::vfs::MAX_NAME_BYTES as u32,
        }
    }

    // ---- hot path: namespace changes (§8) ----

    /// Creates and opens a file (`O_CREAT`). An existing file is opened
    /// instead, unless `exclusive`.
    pub fn create(
        &self,
        parent: Ino,
        name: &[u8],
        mode: u32,
        exclusive: bool,
        open: OpenMode,
    ) -> Result<(Attr, u64)> {
        let name = crate::vfs::validate_name(name)?;
        let existing = self.tree.read().child(parent, name)?;
        if let Some(ino) = existing {
            if exclusive {
                return Err(CoreError::Exists);
            }
            let (handle, _) = self.open_file(ino, open)?;
            let attr = self
                .tree
                .write()
                .add_lookup(ino)
                .ok_or(CoreError::NotFound)?;
            return Ok((self.masked(attr), handle));
        }
        self.ensure_dir_writable(parent)?;
        let ino = self.allocate_ino()?;
        let ignored = self.is_ignored(parent, name, false);
        // Staging first: a node never exists without its content.
        let staged = self.staging.create(ino, None, None, 0)?;
        let attr = {
            let mut tree = self.tree.write();
            let attr = match tree.create(parent, name, NodeKind::File, mode, now(), ino) {
                Ok(attr) => attr,
                Err(error) => {
                    drop(tree);
                    self.staging.remove(ino);
                    return Err(error);
                }
            };
            let node = tree.get_mut(ino).expect("created");
            node.local_only = ignored;
            node.open_handles = 1;
            node.open_writers = 1;
            attr
        };
        self.staged
            .lock()
            .insert(ino, Arc::new(parking_lot::Mutex::new(staged)));
        self.persist(&[ino, parent]);
        self.journal.touch_content(ino);
        let handle = self.add_handle(ino, true);
        Ok((self.masked(attr), handle))
    }

    pub fn mkdir(&self, parent: Ino, name: &[u8], mode: u32) -> Result<Attr> {
        let name = crate::vfs::validate_name(name)?;
        self.ensure_dir_writable(parent)?;
        let ino = self.allocate_ino()?;
        let ignored = self.is_ignored(parent, name, true);
        let attr = {
            let mut tree = self.tree.write();
            let attr = tree.create(parent, name, NodeKind::Folder, mode, now(), ino)?;
            tree.get_mut(ino).expect("created").local_only = ignored;
            attr
        };
        self.persist(&[ino, parent]);
        self.journal.touch(ino);
        Ok(self.masked(attr))
    }

    pub fn unlink(&self, parent: Ino, name: &[u8]) -> Result<()> {
        self.remove(parent, name, false)
    }

    pub fn rmdir(&self, parent: Ino, name: &[u8]) -> Result<()> {
        self.remove(parent, name, true)
    }

    fn remove(&self, parent: Ino, name: &[u8], dir: bool) -> Result<()> {
        let name = crate::vfs::validate_name(name)?;
        self.ensure_dir_writable(parent)?;
        let (ino, droppable) = {
            let mut tree = self.tree.write();
            let ino = tree.remove_entry(parent, name, dir, now())?;
            (ino, tree.is_droppable(ino))
        };
        self.persist(&[ino, parent]);
        self.journal.touch(ino);
        if droppable {
            self.drop_detached(ino);
        }
        if parent == ROOT_INO && name == crate::pdignore::ROOT_FILE {
            self.refresh_ignore();
        }
        Ok(())
    }

    /// `rename(2)` and `renameat2` with `RENAME_NOREPLACE` (§8.3, §7.6).
    pub fn rename(
        &self,
        parent: Ino,
        name: &[u8],
        new_parent: Ino,
        new_name: &[u8],
        mode: RenameMode,
    ) -> Result<()> {
        let name = crate::vfs::validate_name(name)?;
        let new_name = crate::vfs::validate_name(new_name)?;
        self.ensure_dir_writable(parent)?;
        self.ensure_dir_writable(new_parent)?;
        let target = self.tree.read().child(new_parent, new_name)?;
        if let Some(target) = target {
            self.ensure_writable(target)?;
        }
        if parent != new_parent {
            // A directory moved elsewhere has its `..` rewritten (POSIX).
            let source = self.tree.read().child(parent, name)?;
            if let Some(source) = source
                && self
                    .tree
                    .read()
                    .get(source)
                    .is_some_and(|node| node.is_dir())
            {
                self.ensure_allowed(source, MAY_WRITE)?;
            }
        }
        let is_dir = {
            let tree = self.tree.read();
            let source = tree.child(parent, name)?;
            source.is_some_and(|ino| tree.get(ino).is_some_and(|node| node.is_dir()))
        };
        let ignored = self.is_ignored(new_parent, new_name, is_dir);
        let (renamed, adopted_rev) = {
            let mut tree = self.tree.write();
            let renamed = tree.rename(parent, name, new_parent, new_name, mode, now())?;
            let adopted_rev = self.adopt_on_replace(&mut tree, &renamed, ignored);
            (renamed, adopted_rev)
        };
        let mut reclassified = self.reclassify(renamed.ino);
        if let Some(rev) = adopted_rev
            && let Some(staged) = self.staged_entry(renamed.ino)
        {
            staged.lock().set_base_rev(rev)?;
        }
        let mut rows = vec![renamed.ino, parent, new_parent];
        rows.extend(renamed.replaced);
        rows.append(&mut reclassified);
        self.persist(&rows);
        for &ino in &rows {
            self.journal.touch(ino);
        }
        if let Some(replaced) = renamed.replaced {
            let droppable = self.tree.read().is_droppable(replaced);
            if droppable {
                self.drop_detached(replaced);
            }
        }
        let rules = crate::pdignore::ROOT_FILE;
        if (parent == ROOT_INO && name == rules) || (new_parent == ROOT_INO && new_name == rules) {
            self.refresh_ignore();
        }
        Ok(())
    }

    /// Atomic save (§7.6): a file that was never uploaded, renamed over a
    /// remote file, takes over that file's remote node, so its content
    /// becomes a new revision of it. Returns the revision the upload must
    /// build on.
    fn adopt_on_replace(
        &self,
        tree: &mut Tree,
        renamed: &crate::vfs::Renamed,
        source_ignored: bool,
    ) -> Option<Option<RevisionId>> {
        let replaced = renamed.replaced?;
        let source = tree.get(renamed.ino)?;
        let target = tree.get(replaced)?;
        let adopt = source.kind == NodeKind::File
            && target.kind == NodeKind::File
            && source.remote.is_none()
            && source.content == Content::Staged
            && target.remote.is_some()
            && !source_ignored;
        if !adopt {
            return None;
        }
        let remote = target.remote.clone();
        let rev = remote.as_ref().and_then(|r| r.rev.clone());
        tree.set_remote(replaced, None);
        if let Some(target) = tree.get_mut(replaced) {
            target.deleted = false;
        }
        tree.set_remote(renamed.ino, remote);
        if let Some(source) = tree.get_mut(renamed.ino) {
            // Its own mode and mtime replace the old file's.
            source.gens.meta += 1;
        }
        Some(rev)
    }

    /// `setattr` (§8.5): mode, size, mtime and atime; ownership is checked
    /// by the caller.
    pub fn setattr(&self, ino: Ino, change: &SetAttr) -> Result<Attr> {
        let kind = self.tree.read().get(ino).ok_or(CoreError::NotFound)?.kind;
        if change != &SetAttr::default() {
            self.ensure_writable(ino)?;
        }
        if change.size.is_some() && !self.writable_handle(ino, change.handle) {
            self.ensure_allowed(ino, MAY_WRITE)?;
        }
        if let Some(size) = change.size {
            if kind == NodeKind::Folder {
                return Err(CoreError::IsADirectory);
            }
            self.truncate(ino, size)?;
        }
        let attr = {
            let mut tree = self.tree.write();
            let node = tree.get_mut(ino).ok_or(CoreError::NotFound)?;
            let mut meta_changed = false;
            if let Some(mode) = change.mode {
                meta_changed |= node.mode != mode & 0o7777;
                node.mode = mode & 0o7777;
            }
            if let Some(mtime) = change.mtime {
                meta_changed |= node.mtime != mtime;
                node.mtime = mtime;
            }
            if let Some(atime) = change.atime {
                node.atime = Some(atime);
            }
            if meta_changed && node.kind == NodeKind::File {
                node.gens.meta += 1;
            }
            node.ctime = now();
            node.attr()
        };
        self.persist(&[ino]);
        if kind == NodeKind::File {
            self.journal.touch_meta(ino);
        }
        Ok(self.masked(attr))
    }

    // ---- hot path: file content ----

    /// Opens a file. Returns the handle and whether the kernel may keep its
    /// cached pages (nothing replaced the content since the last open).
    pub fn open_file(&self, ino: Ino, mode: OpenMode) -> Result<(u64, bool)> {
        {
            let tree = self.tree.read();
            let node = tree.get(ino).ok_or(CoreError::NotFound)?;
            if node.is_dir() {
                return Err(CoreError::IsADirectory);
            }
        }
        if mode.write {
            self.ensure_writable(ino)?;
        }
        let want = if mode.read { MAY_READ } else { 0 } | if mode.write { MAY_WRITE } else { 0 };
        self.ensure_allowed(ino, want)?;
        if mode.write && mode.truncate {
            self.truncate(ino, 0)?;
        }
        let keep_cache = {
            let mut tree = self.tree.write();
            let node = tree.get_mut(ino).ok_or(CoreError::NotFound)?;
            let keep = node.served_epoch == Some(node.content_epoch) && !mode.truncate;
            node.served_epoch = Some(node.content_epoch);
            node.open_handles += 1;
            if mode.write {
                node.open_writers += 1;
            }
            keep
        };
        Ok((self.add_handle(ino, mode.write), keep_cache))
    }

    fn add_handle(&self, ino: Ino, writable: bool) -> u64 {
        let handle = self.next_handle.fetch_add(1, Ordering::Relaxed);
        self.files.lock().insert(
            handle,
            OpenFile {
                ino,
                writable,
                last_end: 0,
                sequential_reads: 0,
            },
        );
        handle
    }

    pub fn release_file(&self, handle: u64) {
        let Some(file) = self.files.lock().remove(&handle) else {
            return;
        };
        let droppable = {
            let mut tree = self.tree.write();
            let Some(node) = tree.get_mut(file.ino) else {
                return;
            };
            node.open_handles = node.open_handles.saturating_sub(1);
            if file.writable {
                node.open_writers = node.open_writers.saturating_sub(1);
            }
            tree.is_droppable(file.ino)
        };
        if file.writable {
            self.persist(&[file.ino]);
            self.journal.touch_content(file.ino);
            if self.is_root_rules(file.ino) {
                self.refresh_ignore();
            }
        }
        if droppable {
            self.drop_detached(file.ino);
        }
    }

    /// `flush` (close): returns an upload error deferred on this inode
    /// once (§7.8). Never waits for uploads.
    pub fn flush(&self, handle: u64) -> Result<()> {
        let ino = self.file(handle)?.ino;
        let error = self
            .tree
            .write()
            .get_mut(ino)
            .and_then(|node| node.deferred_error.take());
        match error {
            Some(errno) => {
                self.persist(&[ino]);
                Err(CoreError::Errno(errno))
            }
            None => Ok(()),
        }
    }

    /// Writes into staging. `NeedsMaterialize` means a partly overwritten
    /// base block must be downloaded first ([`Engine::write`]).
    pub fn write_local(&self, handle: u64, offset: u64, data: &[u8]) -> Result<u32> {
        let file = self.file(handle)?;
        if !file.writable {
            return Err(CoreError::Errno(nix::libc::EBADF));
        }
        // Handles opened before a switch to `ro` are refused too (§11.4).
        self.ensure_writable(file.ino)?;
        let current = self.tree.read().get(file.ino).map_or(0, |n| n.size);
        self.check_quota((offset + data.len() as u64).saturating_sub(current))?;
        let first_change = self.with_staged(file.ino, false, |staged| {
            if !staged
                .blocks_to_materialize_for_write(offset, data.len() as u64)
                .is_empty()
            {
                return Err(CoreError::NeedsMaterialize(file.ino));
            }
            staged.write(offset, data)?;
            let mut tree = self.tree.write();
            let node = tree.get_mut(file.ino).ok_or(CoreError::NotFound)?;
            let first = !node.content_pending();
            node.content = Content::Staged;
            node.size = staged.size();
            let time = now();
            node.mtime = time;
            node.ctime = time;
            node.gens.content += 1;
            Ok(first)
        })?;
        if first_change {
            self.persist(&[file.ino]);
        }
        self.journal.touch_content(file.ino);
        Ok(data.len() as u32)
    }

    /// Slow-path write: materializes what the write needs, then writes.
    pub async fn write(self: &Arc<Self>, handle: u64, offset: u64, data: Vec<u8>) -> Result<u32> {
        let _foreground = self.gate.foreground();
        let ino = self.file(handle)?.ino;
        for _ in 0..8 {
            match self.write_local(handle, offset, &data) {
                Err(CoreError::NeedsMaterialize(_)) => {
                    let blocks = match self.staged_entry(ino) {
                        Some(staged) => staged
                            .lock()
                            .blocks_to_materialize_for_write(offset, data.len() as u64),
                        None => Vec::new(),
                    };
                    self.materialize(ino, &blocks, Priority::Interactive)
                        .await?;
                }
                result => return result,
            }
        }
        Err(CoreError::Internal(
            "write kept needing materialization".into(),
        ))
    }

    /// Changes a file's size; `NeedsMaterialize` when the cut falls inside a
    /// base block not downloaded yet.
    fn truncate(&self, ino: Ino, size: u64) -> Result<()> {
        let current = self.tree.read().get(ino).map_or(0, |n| n.size);
        self.check_quota(size.saturating_sub(current))?;
        self.with_staged(ino, size == 0, |staged| {
            if !staged.blocks_to_materialize_for_truncate(size).is_empty() {
                return Err(CoreError::NeedsMaterialize(ino));
            }
            staged.truncate(size)?;
            let mut tree = self.tree.write();
            let node = tree.get_mut(ino).ok_or(CoreError::NotFound)?;
            node.content = Content::Staged;
            node.size = staged.size();
            let time = now();
            node.mtime = time;
            node.ctime = time;
            node.gens.content += 1;
            Ok(())
        })?;
        self.persist(&[ino]);
        self.journal.touch_content(ino);
        Ok(())
    }

    /// Slow-path `setattr`: materializes the block a truncation cuts.
    pub async fn setattr_slow(self: &Arc<Self>, ino: Ino, change: SetAttr) -> Result<Attr> {
        let _foreground = self.gate.foreground();
        for _ in 0..8 {
            match self.setattr(ino, &change) {
                Err(CoreError::NeedsMaterialize(_)) => {
                    let size = change.size.unwrap_or(0);
                    let blocks = match self.staged_entry(ino) {
                        Some(staged) => staged.lock().blocks_to_materialize_for_truncate(size),
                        None => Vec::new(),
                    };
                    self.materialize(ino, &blocks, Priority::Interactive)
                        .await?;
                }
                result => return result,
            }
        }
        Err(CoreError::Internal(
            "truncate kept needing materialization".into(),
        ))
    }

    /// Serves a read from RAM, staging or the disk cache. `None` means a
    /// block is missing and the slow path ([`Engine::read`]) must run.
    pub fn read_local(
        self: &Arc<Self>,
        handle: u64,
        offset: u64,
        size: u32,
    ) -> Result<Option<Vec<u8>>> {
        let file = self.file(handle)?;
        match self.read_source(file.ino)? {
            ReadSource::Remote {
                link,
                rev,
                size: file_size,
            } => {
                let Some((rev, len)) = read_range(rev, file_size, offset, size) else {
                    return Ok(Some(Vec::new()));
                };
                let Some(layout) = self.cache.layout_local(&link, &rev, file_size) else {
                    return Ok(None);
                };
                let spans = layout.spans(offset, len);
                let mut blocks = Vec::with_capacity(spans.len());
                for span in spans {
                    let key = BlockKey {
                        link: link.clone(),
                        rev: rev.clone(),
                        index: span.index,
                    };
                    match self.cache.get_local(&key) {
                        Some(data) => blocks.push(data),
                        None => return Ok(None),
                    }
                }
                self.readahead(handle, &link, &rev, &layout, offset, len);
                Ok(Some(assemble(spans, &blocks, offset, len)))
            }
            ReadSource::Staged(staged) => {
                let staged = staged.lock();
                let pieces = staged.plan_read(offset, u64::from(size));
                let base = staged.base().cloned();
                let mut out = Vec::new();
                for piece in pieces {
                    match piece {
                        Piece::Staging { offset, len } => {
                            let start = out.len();
                            out.resize(start + len as usize, 0);
                            staged.read_at(offset, &mut out[start..])?;
                        }
                        Piece::Base { offset, len } => {
                            let base = base.as_ref().expect("base pieces have a base");
                            match self.read_revision_local(base, offset, len) {
                                Some(data) => out.extend_from_slice(&data),
                                None => return Ok(None),
                            }
                        }
                    }
                }
                Ok(Some(out))
            }
        }
    }

    /// Reads a range, downloading missing blocks.
    pub async fn read(self: &Arc<Self>, handle: u64, offset: u64, size: u32) -> Result<Vec<u8>> {
        let _foreground = self.gate.foreground();
        let file = self.file(handle)?;
        match self.read_source(file.ino)? {
            ReadSource::Remote {
                link,
                rev,
                size: file_size,
            } => {
                let Some((rev, len)) = read_range(rev, file_size, offset, size) else {
                    return Ok(Vec::new());
                };
                let layout = self.cache.layout(&link, &rev, file_size).await?;
                let data = self
                    .read_layout(&link, &rev, &layout, offset, len, Priority::Interactive)
                    .await?;
                self.readahead(handle, &link, &rev, &layout, offset, len);
                Ok(data)
            }
            ReadSource::Staged(staged) => {
                // Fetch base pieces without holding the lock, then assemble.
                let (pieces, base) = {
                    let staged = staged.lock();
                    (
                        staged.plan_read(offset, u64::from(size)),
                        staged.base().cloned(),
                    )
                };
                let mut fetched = HashMap::new();
                for piece in &pieces {
                    if let (Piece::Base { offset, len }, Some(base)) = (piece, &base) {
                        let data = self
                            .read_revision(base, *offset, *len, Priority::Interactive)
                            .await?;
                        fetched.insert(*offset, data);
                    }
                }
                let staged = staged.lock();
                let mut out = Vec::new();
                for piece in staged.plan_read(offset, u64::from(size)) {
                    match piece {
                        Piece::Staging { offset, len } => {
                            let start = out.len();
                            out.resize(start + len as usize, 0);
                            staged.read_at(offset, &mut out[start..])?;
                        }
                        Piece::Base { offset, len } => match fetched.get(&offset) {
                            Some(data) if data.len() as u64 >= len => {
                                out.extend_from_slice(&data[..len as usize]);
                            }
                            _ => {
                                return Err(CoreError::Internal(
                                    "staged read changed while fetching".into(),
                                ));
                            }
                        },
                    }
                }
                Ok(out)
            }
        }
    }

    /// `fsync` in `local` mode (§5.6): staging data and the store are on
    /// disk when it returns.
    pub async fn fsync(&self, ino: Ino) -> Result<()> {
        if let Some(staged) = self.staged_entry(ino) {
            let staged = Arc::clone(&staged);
            tokio::task::spawn_blocking(move || staged.lock().sync())
                .await
                .map_err(|e| CoreError::Internal(e.to_string()))??;
        }
        self.persist(&[ino]);
        self.store.flush().await?;
        let error = self
            .tree
            .write()
            .get_mut(ino)
            .and_then(|node| node.deferred_error.take());
        match error {
            Some(errno) => Err(CoreError::Errno(errno)),
            None => Ok(()),
        }
    }

    /// `fsyncdir`: the store is on disk when it returns.
    pub async fn fsyncdir(&self) -> Result<()> {
        self.store.flush().await?;
        Ok(())
    }

    // ---- slow path: listings ----

    /// Lists `dir` if it was never listed, for a request a user waits on.
    pub async fn ensure_listed(self: &Arc<Self>, dir: Ino) -> Result<()> {
        let _foreground = self.gate.foreground();
        self.ensure_listed_now(dir).await
    }

    async fn ensure_listed_now(self: &Arc<Self>, dir: Ino) -> Result<()> {
        let listed = {
            let tree = self.tree.read();
            let node = tree.get(dir).ok_or(CoreError::NotFound)?;
            if !node.is_dir() {
                return Err(CoreError::NotADirectory);
            }
            node.listed
        };
        if listed { Ok(()) } else { self.list(dir).await }
    }

    /// Fetches the children of `dir` and applies them (shared per directory).
    pub async fn list(self: &Arc<Self>, dir: Ino) -> Result<()> {
        let this = Arc::clone(self);
        self.listings
            .run(dir, move || async move { this.list_now(dir).await })
            .await
    }

    async fn list_now(self: &Arc<Self>, dir: Ino) -> Result<()> {
        let link = {
            let tree = self.tree.read();
            let node = tree.get(dir).ok_or(CoreError::NotFound)?;
            if !node.is_dir() {
                return Err(CoreError::NotADirectory);
            }
            match node.link() {
                Some(link) => link.clone(),
                // Created locally: complete by construction.
                None => return Ok(()),
            }
        };
        check_not_hot(&self.metrics, "list_children");
        Metrics::inc(&self.metrics.listings);
        let fetched_at = self.remote_clock.load(Ordering::SeqCst);
        let children = self.backend.list_children(&link).await?;
        let mut inos = Vec::new();
        loop {
            let unknown = self.tree.read().unknown_count(&children);
            if unknown > inos.len() {
                inos.extend(self.allocate_inos((unknown - inos.len()) as u64).await?);
            }
            let applied = self
                .tree
                .write()
                .apply_listing_at(dir, &children, &mut inos, fetched_at);
            match applied {
                Ok(changes) => {
                    self.apply_changes(changes);
                    return Ok(());
                }
                Err(ApplyError::NeedInos(_)) => continue,
                Err(ApplyError::NotADirectory) => return Err(CoreError::NotFound),
            }
        }
    }

    /// Persists and announces the effects of a tree change.
    pub(crate) fn apply_changes(&self, changes: Changes) {
        let Changes {
            changed,
            removed,
            invalidations,
        } = changes;
        let mut rows = changed;
        rows.extend(&removed);
        if rows.contains(&ROOT_INO) {
            // The root was listed: `/.pdignore` may have changed.
            self.refresh_ignore();
        }
        self.persist(&rows);
        for ino in &rows {
            if self.journal_relevant(*ino) {
                self.journal.touch(*ino);
            }
        }
        for ino in removed {
            if self.tree.read().get(ino).is_none() {
                self.unstage(ino);
                self.remove_staging_after_commit(ino);
            }
        }
        for invalidation in invalidations {
            let _ = self.invalidations.send(invalidation);
        }
    }

    fn journal_relevant(&self, ino: Ino) -> bool {
        let tree = self.tree.read();
        tree.get(ino)
            .is_some_and(|node| node.deleted || node.content_pending() || node.meta_pending())
            || tree.placement_pending(ino)
    }

    /// Re-lists every listed directory, root first, as background work
    /// (D025, D027): for a `refresh` from the event stream, or a profile
    /// without a cursor. Fails with the last error after trying them all.
    pub async fn revalidate(self: &Arc<Self>) -> Result<()> {
        let dirs = self.tree.read().listed_dirs();
        let count = dirs.len();
        let mut failed = None;
        for dir in dirs {
            let _turn = self.gate.background().await;
            match self.list(dir).await {
                // Removed by the listing of an ancestor.
                Ok(()) | Err(CoreError::NotFound) => {}
                Err(error) => {
                    tracing::warn!(dir, %error, "revalidation listing failed");
                    failed = Some(error);
                }
            }
        }
        tracing::info!(directories = count, "revalidation finished");
        failed.map_or(Ok(()), Err)
    }

    pub async fn refresh_storage(&self) {
        check_not_hot(&self.metrics, "storage_info");
        match self.backend.storage_info().await {
            Ok(info) => *self.storage.lock() = Some(info),
            Err(error) => tracing::warn!(%error, "cannot refresh the quota"),
        }
    }

    /// The next value of the remote clock, for a change the executor just
    /// made remotely.
    pub(crate) fn stamp(&self) -> u64 {
        self.remote_clock.fetch_add(1, Ordering::SeqCst) + 1
    }

    // ---- internals: quota ----

    /// ENOSPC when growing by `growth` bytes would exceed the quota, counting
    /// what is still waiting for upload (§7.8). Unknown quota passes.
    fn check_quota(&self, growth: u64) -> Result<()> {
        if growth == 0 {
            return Ok(());
        }
        let Some(storage) = *self.storage.lock() else {
            return Ok(());
        };
        let mut cached = self.pending_bytes.lock();
        let (summed_at, summed, since) = match *cached {
            Some(entry) if entry.0.elapsed() < PENDING_BYTES_TTL => entry,
            _ => (std::time::Instant::now(), self.pending_upload_bytes(), 0),
        };
        if storage
            .used
            .saturating_add(summed)
            .saturating_add(since)
            .saturating_add(growth)
            > storage.max
        {
            return Err(CoreError::NoSpace);
        }
        *cached = Some((summed_at, summed, since + growth));
        Ok(())
    }

    fn pending_upload_bytes(&self) -> u64 {
        let inos = self.journal.inos();
        let tree = self.tree.read();
        inos.iter()
            .filter_map(|ino| tree.get(*ino))
            .filter(|node| node.content_pending())
            .map(|node| node.size)
            .sum()
    }

    // ---- internals: staging ----

    pub(crate) fn staged_entry(&self, ino: Ino) -> Option<Arc<parking_lot::Mutex<Staged>>> {
        self.staged.lock().get(&ino).cloned()
    }

    /// The staging of a file, created from its remote content if needed
    /// (local disk only).
    pub(crate) fn ensure_staged(&self, ino: Ino) -> Result<Arc<parking_lot::Mutex<Staged>>> {
        self.stage(ino, false)
    }

    /// Runs `change` on the current staging of `ino` with its lock held
    /// (`empty`: a new staging starts empty, nothing of the old content is
    /// used, for `O_TRUNC`),
    /// so that a change to the staging and to the node is one step for an
    /// upload commit, which drops a clean staging under the same lock: a
    /// write between its check and the drop would be lost (fsx, M2.9).
    fn with_staged<T>(
        &self,
        ino: Ino,
        empty: bool,
        change: impl FnOnce(&mut Staged) -> Result<T>,
    ) -> Result<T> {
        for _ in 0..8 {
            let staged = self.stage(ino, empty)?;
            let mut guard = staged.lock();
            let current = self
                .staged
                .lock()
                .get(&ino)
                .is_some_and(|entry| Arc::ptr_eq(entry, &staged));
            if current {
                return change(&mut guard);
            }
        }
        Err(CoreError::Internal(format!(
            "the staging of inode {ino} kept changing"
        )))
    }

    fn stage(&self, ino: Ino, empty: bool) -> Result<Arc<parking_lot::Mutex<Staged>>> {
        let mut map = self.staged.lock();
        if let Some(staged) = map.get(&ino) {
            return Ok(Arc::clone(staged));
        }
        let (base_rev, base, size) = {
            let tree = self.tree.read();
            let node = tree.get(ino).ok_or(CoreError::NotFound)?;
            if node.is_dir() {
                return Err(CoreError::IsADirectory);
            }
            let remote_rev = node.remote.as_ref().and_then(|r| r.rev.clone());
            match &node.content {
                Content::Remote {
                    link,
                    rev: Some(rev),
                } if !empty && node.size > 0 => (
                    remote_rev,
                    Some(Base {
                        link: link.clone(),
                        rev: rev.clone(),
                        rev_size: node.size,
                        size: node.size,
                    }),
                    node.size,
                ),
                _ => (remote_rev, None, if empty { 0 } else { node.size }),
            }
        };
        let staged = Arc::new(parking_lot::Mutex::new(
            self.staging.create(ino, base_rev, base, size)?,
        ));
        map.insert(ino, Arc::clone(&staged));
        drop(map);
        if let Some(node) = self.tree.write().get_mut(ino) {
            node.content = Content::Staged;
        }
        Ok(staged)
    }

    /// Downloads base blocks into staging.
    pub(crate) async fn materialize(
        &self,
        ino: Ino,
        blocks: &[u64],
        priority: Priority,
    ) -> Result<()> {
        let Some(staged) = self.staged_entry(ino) else {
            return Ok(());
        };
        for &index in blocks {
            let (base, range) = {
                let staged = staged.lock();
                (staged.base().cloned(), staged.base_range(index))
            };
            let (Some(base), Some((start, end))) = (base, range) else {
                continue;
            };
            let data = self
                .read_revision(&base, start, end - start, priority)
                .await?;
            staged.lock().materialize(index, &base, &data)?;
        }
        Ok(())
    }

    /// Bytes `[offset, offset + len)` of a base revision, from the cache.
    fn read_revision_local(&self, base: &Base, offset: u64, len: u64) -> Option<Vec<u8>> {
        let layout = self
            .cache
            .layout_local(&base.link, &base.rev, base.rev_size)?;
        let spans = layout.spans(offset, len);
        let mut blocks = Vec::with_capacity(spans.len());
        for span in spans {
            let key = BlockKey {
                link: base.link.clone(),
                rev: base.rev.clone(),
                index: span.index,
            };
            blocks.push(self.cache.get_local(&key)?);
        }
        Some(assemble(spans, &blocks, offset, len))
    }

    /// Bytes `[offset, offset + len)` of a base revision, downloading
    /// missing blocks.
    pub(crate) async fn read_revision(
        &self,
        base: &Base,
        offset: u64,
        len: u64,
        priority: Priority,
    ) -> Result<Vec<u8>> {
        let layout = self
            .cache
            .layout(&base.link, &base.rev, base.rev_size)
            .await?;
        self.read_layout(&base.link, &base.rev, &layout, offset, len, priority)
            .await
    }

    async fn read_layout(
        &self,
        link: &LinkId,
        rev: &RevisionId,
        layout: &Layout,
        offset: u64,
        len: u64,
        priority: Priority,
    ) -> Result<Vec<u8>> {
        let spans = layout.spans(offset, len);
        let fetches = spans.iter().map(|span| {
            let key = BlockKey {
                link: link.clone(),
                rev: rev.clone(),
                index: span.index,
            };
            let cache = Arc::clone(&self.cache);
            async move { cache.get(&key, span.len, priority).await }
        });
        let blocks: Vec<Bytes> = futures::future::try_join_all(fetches).await?;
        Ok(assemble(spans, &blocks, offset, len))
    }

    fn read_source(&self, ino: Ino) -> Result<ReadSource> {
        let (content, size) = {
            let tree = self.tree.read();
            let node = tree.get(ino).ok_or(CoreError::NotFound)?;
            (node.content.clone(), node.size)
        };
        match content {
            Content::Remote { link, rev } => Ok(ReadSource::Remote { link, rev, size }),
            Content::Staged => self
                .staged_entry(ino)
                .map(ReadSource::Staged)
                .ok_or_else(|| CoreError::Internal(format!("inode {ino} has no staging"))),
        }
    }

    /// Removes a detached node nobody references any more, with its
    /// staging.
    fn drop_detached(&self, ino: Ino) {
        let dropped = {
            let mut tree = self.tree.write();
            if tree.is_droppable(ino) && !self.journal.in_flight(ino) {
                tree.drop_node(ino);
                true
            } else {
                false
            }
        };
        if dropped {
            self.unstage(ino);
            self.persist(&[ino]);
            self.remove_staging_after_commit(ino);
        }
    }

    /// Drops the staged entry of `ino`. Its file is closed after the map
    /// lock is released: on btrfs a close can wait for a commit in progress,
    /// and `create` needs the lock.
    pub(crate) fn unstage(&self, ino: Ino) {
        let staged = self.staged.lock().remove(&ino);
        drop(staged);
    }

    /// Deletes the staging of `ino` once the store rows queued so far are
    /// durable, unless the inode has a new staging by then: a crash before
    /// that finds the stored rows still naming it. Queue the rows first.
    /// Removals within [`STAGING_REMOVAL_DELAY`] share one fsync.
    pub(crate) fn remove_staging_after_commit(&self, ino: Ino) {
        let first = {
            let mut removals = self.staging_removals.lock();
            removals.push(ino);
            removals.len() == 1
        };
        if first {
            let removal = remove_committed_staging(
                Arc::clone(&self.store),
                self.staging.clone(),
                Arc::clone(&self.staged),
                Arc::clone(&self.staging_removals),
            );
            self.runtime.spawn(async move {
                tokio::time::sleep(STAGING_REMOVAL_DELAY).await;
                removal.await;
            });
        }
    }

    // ---- access rules (§11) ----

    /// EACCES unless the owner bits of the stored mode grant `want`. Only
    /// the mounting user reaches the mount (no `allow_other`), so these are
    /// the bits that apply; the kernel does not check them itself (D031).
    /// Whether `handle` is open for writing on `ino`.
    fn writable_handle(&self, ino: Ino, handle: Option<u64>) -> bool {
        handle.is_some_and(|handle| {
            self.files
                .lock()
                .get(&handle)
                .is_some_and(|file| file.ino == ino && file.writable)
        })
    }

    fn ensure_allowed(&self, ino: Ino, want: u32) -> Result<()> {
        if want == 0 {
            return Ok(());
        }
        let mode = self.tree.read().get(ino).ok_or(CoreError::NotFound)?.mode;
        if (mode >> 6) & want == want {
            Ok(())
        } else {
            Err(CoreError::AccessDenied)
        }
    }

    /// Changing entries of a directory: EROFS in `ro` subtrees, then
    /// write and search permission.
    fn ensure_dir_writable(&self, dir: Ino) -> Result<()> {
        self.ensure_writable(dir)?;
        self.ensure_allowed(dir, MAY_WRITE | MAY_EXEC)
    }

    /// `access(2)`: `mask` of `MAY_*` bits; write access in a `ro` subtree
    /// is EROFS, like on a read-only file system.
    pub fn access(&self, ino: Ino, mask: u32) -> Result<()> {
        if mask & MAY_WRITE != 0 {
            self.ensure_writable(ino)?;
        }
        self.ensure_allowed(ino, mask & (MAY_READ | MAY_WRITE | MAY_EXEC))
    }

    /// EROFS when `ino` is in a `ro` subtree (D019).
    fn ensure_writable(&self, ino: Ino) -> Result<()> {
        let access = self.access.read();
        if access.is_empty() {
            return Ok(());
        }
        let tree = self.tree.read();
        if access.is_read_only(&tree, ino) {
            return Err(CoreError::ReadOnly);
        }
        Ok(())
    }

    /// Reported attributes lose their write bits in `ro` subtrees (§11.2).
    fn masked(&self, mut attr: Attr) -> Attr {
        let access = self.access.read();
        if !access.is_empty() && access.is_read_only(&self.tree.read(), attr.ino) {
            attr.perm &= !0o222;
        }
        attr
    }

    /// Resolves a path below the mount root (`/a/b` or `a/b`), listing
    /// directories on the way.
    pub async fn resolve(self: &Arc<Self>, path: &str) -> Result<Ino> {
        let mut ino = ROOT_INO;
        for part in path.split('/').filter(|p| !p.is_empty() && *p != ".") {
            if part == ".." {
                return Err(CoreError::InvalidArgument);
            }
            self.ensure_listed(ino).await?;
            ino = self
                .tree
                .read()
                .child(ino, part)?
                .ok_or(CoreError::NotFound)?;
        }
        Ok(ino)
    }

    /// `access set ro|rw <path>` (§11.4). Returns how many pending journal
    /// entries are in the subtree; they still upload.
    pub async fn access_set(
        self: &Arc<Self>,
        path: &str,
        mode: AccessMode,
        source: RuleSource,
    ) -> Result<usize> {
        let ino = self.resolve(path).await?;
        let rule = {
            let tree = self.tree.read();
            Rule {
                ino,
                link: tree.link_of(ino).map(|link| link.0.clone()),
                path_at_set: tree.path(ino).unwrap_or_else(|| path.to_owned()),
                mode,
                source,
            }
        };
        let stored = rule.clone();
        self.store
            .write_durable(move |conn| crate::access::store_rule(conn, &stored, false))
            .await?;
        self.access.write().insert(rule);
        self.invalidate_subtree(ino);
        let tree = self.tree.read();
        Ok(self
            .journal
            .inos()
            .into_iter()
            .filter(|&pending| tree.is_within(pending, ino))
            .count())
    }

    /// `access unset <path>`: the rule on the entry at `path`, or an
    /// orphaned rule that was set on `path`. Returns whether one existed.
    pub async fn access_unset(self: &Arc<Self>, path: &str) -> Result<bool> {
        let resolved = self.resolve(path).await.ok();
        let ino = {
            let access = self.access.read();
            let tree = self.tree.read();
            let normalized = format!("/{}", path.trim_matches('/'));
            resolved
                .filter(|ino| access.get(*ino).is_some())
                .or_else(|| access.orphaned_ino(&tree, &normalized))
        };
        let Some(ino) = ino else {
            return Ok(false);
        };
        self.store
            .write_durable(move |conn| crate::access::delete_rule(conn, ino))
            .await?;
        self.access.write().remove(ino);
        if self.tree.read().get(ino).is_some() {
            self.invalidate_subtree(ino);
        }
        Ok(true)
    }

    /// `access list [<path>]`.
    pub async fn access_list(
        self: &Arc<Self>,
        path: Option<&str>,
    ) -> Result<(Vec<RuleView>, AccessMode)> {
        let under = match path {
            Some(path) => Some(self.resolve(path).await?),
            None => None,
        };
        let access = self.access.read();
        let views = access.views(&self.tree.read(), under);
        Ok((views, self.config.access.default))
    }

    /// `access effective <path>`: the mode and the deciding rule's path
    /// (`None`: the default).
    pub async fn access_effective(
        self: &Arc<Self>,
        path: &str,
    ) -> Result<(AccessMode, Option<String>)> {
        let ino = self.resolve(path).await?;
        let access = self.access.read();
        let tree = self.tree.read();
        let (mode, rule) = access.effective(&tree, ino);
        Ok((
            mode,
            rule.map(|rule| {
                tree.path(rule.ino)
                    .unwrap_or_else(|| rule.path_at_set.clone())
            }),
        ))
    }

    /// Imports `[[access.rule]]` from the config once per path; from then on
    /// the database is authoritative (§11.5).
    pub async fn import_config_rules(self: &Arc<Self>) {
        for rule in self.config.access.rule.clone() {
            let key = format!("access_config_imported:/{}", rule.path.trim_matches('/'));
            if self.store.meta_get(&key).ok().flatten().is_some() {
                continue;
            }
            match self
                .access_set(&rule.path, rule.mode, RuleSource::Config)
                .await
            {
                Ok(_) => {
                    let stored = key.clone();
                    if let Err(error) = self
                        .store
                        .write_durable(move |conn| meta_set(conn, &stored, "1"))
                        .await
                    {
                        tracing::warn!(%error, "cannot record an imported access rule");
                    }
                }
                Err(error) => {
                    tracing::warn!(path = rule.path, %error, "cannot import an access rule from the config");
                }
            }
        }
    }

    /// Drops the kernel's attributes for a subtree (mode bits changed).
    fn invalidate_subtree(&self, ino: Ino) {
        let tree = self.tree.read();
        let mut stack = vec![ino];
        while let Some(current) = stack.pop() {
            let Some(node) = tree.get(current) else {
                continue;
            };
            stack.extend(node.children.values().copied());
            let _ = self
                .invalidations
                .send(Invalidation::Inode { ino: current });
        }
    }

    // ---- internals: `.pdignore` (§7.7) ----

    /// Whether a new entry `name` in `parent` stays local-only: matched by
    /// the rules, or inside a local-only folder.
    fn is_ignored(&self, parent: Ino, name: &str, is_dir: bool) -> bool {
        let matcher = Arc::clone(&self.ignore.read());
        let tree = self.tree.read();
        if tree.get(parent).is_some_and(|dir| dir.local_only) {
            return true;
        }
        if matcher.is_empty() {
            return false;
        }
        let Some(dir) = tree.path(parent) else {
            return false;
        };
        matcher.is_ignored(&format!("{}/{name}", dir.trim_end_matches('/')), is_dir)
    }

    /// After a rename: entries never uploaded follow the rules of their new
    /// path. Returns the inodes whose flag changed.
    fn reclassify(&self, ino: Ino) -> Vec<Ino> {
        let matcher = Arc::clone(&self.ignore.read());
        let updates: Vec<(Ino, bool)> = {
            let tree = self.tree.read();
            let mut updates = Vec::new();
            // Parents before children, so a folder's new flag is inherited.
            let mut queue = std::collections::VecDeque::from([(ino, false)]);
            while let Some((current, parent_local)) = queue.pop_front() {
                let Some(node) = tree.get(current) else {
                    continue;
                };
                let parent_local = parent_local
                    || (current == ino && tree.get(node.parent).is_some_and(|p| p.local_only));
                let local = if node.remote.is_some() {
                    false
                } else {
                    parent_local
                        || tree
                            .path(current)
                            .is_some_and(|path| matcher.is_ignored(&path, node.is_dir()))
                };
                if node.remote.is_none() && local != node.local_only {
                    updates.push((current, local));
                }
                queue.extend(node.children.values().map(|&child| (child, local)));
            }
            updates
        };
        let mut tree = self.tree.write();
        updates
            .into_iter()
            .filter_map(|(ino, local)| {
                tree.get_mut(ino).map(|node| {
                    node.local_only = local;
                    ino
                })
            })
            .collect()
    }

    fn is_root_rules(&self, ino: Ino) -> bool {
        self.tree
            .read()
            .child(ROOT_INO, crate::pdignore::ROOT_FILE)
            .ok()
            .flatten()
            == Some(ino)
    }

    /// Asks for the rules to be recompiled (in the background: `/.pdignore`
    /// may have to be downloaded).
    pub(crate) fn refresh_ignore(&self) {
        self.ignore_changed.notify_one();
    }

    async fn keep_ignore_rules(self: Arc<Self>) {
        loop {
            self.ignore_changed.notified().await;
            let root_rules = self
                .tree
                .read()
                .child(ROOT_INO, crate::pdignore::ROOT_FILE)
                .ok()
                .flatten();
            let text = match root_rules {
                Some(ino) => match self.read_whole(ino, 1 << 20).await {
                    Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
                    Err(error) => {
                        tracing::warn!(%error, "cannot read /.pdignore; keeping the previous rules");
                        continue;
                    }
                },
                None => String::new(),
            };
            let matcher = IgnoreMatcher::new([self.local_ignore.as_str(), text.as_str()]);
            *self.ignore.write() = Arc::new(matcher);
            tracing::debug!("`.pdignore` rules reloaded");
        }
    }

    /// The first `limit` bytes of a file.
    async fn read_whole(self: &Arc<Self>, ino: Ino, limit: u64) -> Result<Vec<u8>> {
        let (handle, _) = self.open_file(ino, OpenMode::default())?;
        let size = self.getattr(ino).map_or(0, |attr| attr.size).min(limit);
        let result = async {
            let mut out = Vec::new();
            while (out.len() as u64) < size {
                let want = (size - out.len() as u64).min(1 << 20) as u32;
                let chunk = self.read(handle, out.len() as u64, want).await?;
                if chunk.is_empty() {
                    break;
                }
                out.extend_from_slice(&chunk);
            }
            Ok(out)
        }
        .await;
        self.release_file(handle);
        result
    }

    // ---- internals: store ----

    /// Queues the current rows of `inos` (deleting rows of nodes that no
    /// longer need one).
    pub(crate) fn persist(&self, inos: &[Ino]) {
        let rows: Vec<(Ino, Option<NodeRow>)> = {
            let tree = self.tree.read();
            let mut seen = HashSet::new();
            inos.iter()
                .filter(|ino| seen.insert(**ino))
                .map(|&ino| {
                    let row = tree.persisted(ino).then(|| tree.row(ino)).flatten();
                    (ino, row)
                })
                .collect()
        };
        if rows.is_empty() {
            return;
        }
        self.store.write(move |conn| write_rows(conn, &rows));
    }

    /// Hands every node with pending work to the journal (after loading).
    fn queue_pending_work(&self) {
        let pending: Vec<Ino> = {
            let tree = self.tree.read();
            tree.inos()
                .filter(|&ino| {
                    tree.get(ino).is_some_and(|node| {
                        node.deleted || node.content_pending() || node.meta_pending()
                    }) || tree.placement_pending(ino)
                })
                .collect()
        };
        for ino in pending {
            self.journal.touch(ino);
        }
    }

    // ---- internals: handles and inode numbers ----

    fn file(&self, handle: u64) -> Result<OpenFile> {
        self.files
            .lock()
            .get(&handle)
            .cloned()
            .ok_or(CoreError::NotFound)
    }

    /// An inode number from the reserved range (hot path).
    fn allocate_ino(&self) -> Result<Ino> {
        let mut inos = self.inos.lock();
        if inos.next > inos.reserved {
            return Err(CoreError::NeedsInos);
        }
        let ino = inos.next;
        inos.next += 1;
        Ok(ino)
    }

    /// Extends the durable reservation so the hot path rarely runs out.
    pub async fn reserve_inos(&self, at_least: u64) -> Result<()> {
        let _reserving = self.reserving.lock().await;
        let reserved = {
            let inos = self.inos.lock();
            if inos.reserved >= inos.next + at_least {
                return Ok(());
            }
            inos.next + at_least + INO_RESERVATION
        };
        self.store
            .write_durable(move |conn| meta_set(conn, "ino_reserved", &reserved.to_string()))
            .await?;
        let mut inos = self.inos.lock();
        inos.reserved = inos.reserved.max(reserved);
        Ok(())
    }

    async fn keep_inos_reserved(&self) {
        loop {
            if let Err(error) = self.reserve_inos(INO_RESERVATION / 2).await {
                tracing::warn!(%error, "cannot reserve inode numbers");
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    async fn allocate_inos(&self, count: u64) -> Result<Vec<Ino>> {
        self.reserve_inos(count).await?;
        let mut inos = self.inos.lock();
        if inos.next + count - 1 > inos.reserved {
            return Err(CoreError::NeedsInos);
        }
        let range = inos.next..inos.next + count;
        inos.next += count;
        // `apply_listing` pops from the end; hand out ascending numbers.
        Ok(range.rev().collect())
    }

    // ---- internals: prefetch ----

    /// Sequential readahead (§5.3): after two adjacent reads, fetch the next
    /// blocks (2, then 4 once the run continues) in the background.
    fn readahead(
        self: &Arc<Self>,
        handle: u64,
        link: &LinkId,
        rev: &RevisionId,
        layout: &Arc<Layout>,
        offset: u64,
        len: u64,
    ) {
        let run = {
            let mut files = self.files.lock();
            let Some(open) = files.get_mut(&handle) else {
                return;
            };
            open.sequential_reads = if offset == open.last_end && offset > 0 {
                open.sequential_reads + 1
            } else {
                0
            };
            open.last_end = offset + len;
            open.sequential_reads
        };
        if run == 0 {
            return;
        }
        let ahead = if run < 4 { 2 } else { 4 };
        let Some(last) = layout.spans(offset, len).last().map(|span| span.index) else {
            return;
        };
        let mut index = last;
        for _ in 0..ahead {
            let Some(next) = layout.next_after(index) else {
                break;
            };
            index = next.index;
            let key = BlockKey {
                link: link.clone(),
                rev: rev.clone(),
                index: next.index,
            };
            if self.cache.contains(&key) {
                continue;
            }
            let cache = Arc::clone(&self.cache);
            let len = next.len;
            self.runtime.spawn(async move {
                if let Err(error) = cache.get(&key, len, Priority::Readahead).await {
                    tracing::debug!(%error, "readahead failed");
                }
            });
        }
    }

    /// Listing and small-file prefetch (§5.3) for a directory the user
    /// opened. Runs once per directory per mount.
    fn prefetch_dir(self: &Arc<Self>, dir: Ino) {
        if !self.prefetched_dirs.lock().insert(dir) {
            return;
        }
        let small = self.config.cache.prefetch_small_files.0;
        let mut budget = self.config.cache.prefetch_dir_budget.0;
        let (files, subdirs) = {
            let tree = self.tree.read();
            let Some(node) = tree.get(dir) else { return };
            let mut files = Vec::new();
            let mut subdirs = Vec::new();
            for &child in node.children.values() {
                let Some(child) = tree.get(child) else {
                    continue;
                };
                match (&child.kind, &child.content) {
                    (NodeKind::Folder, _) if !child.listed => subdirs.push(child.ino),
                    (
                        NodeKind::File,
                        Content::Remote {
                            link,
                            rev: Some(rev),
                        },
                    ) if child.size > 0 && child.size <= small && child.size <= budget => {
                        budget -= child.size;
                        files.push((link.clone(), rev.clone(), child.size));
                    }
                    _ => {}
                }
            }
            (files, subdirs)
        };
        if files.is_empty() && subdirs.is_empty() {
            return;
        }
        let this = Arc::clone(self);
        self.runtime.spawn(async move {
            for (link, rev, size) in files {
                let _turn = this.gate.background().await;
                if let Err(error) = this.prefetch_file(&link, &rev, size).await {
                    tracing::debug!(%error, "small-file prefetch failed");
                }
            }
            if !subdirs.is_empty() {
                this.prefetch_listings(subdirs).await;
            }
        });
    }

    async fn prefetch_file(&self, link: &LinkId, rev: &RevisionId, size: u64) -> Result<()> {
        let layout = self.cache.layout(link, rev, size).await?;
        for span in &layout.blocks {
            let key = BlockKey {
                link: link.clone(),
                rev: rev.clone(),
                index: span.index,
            };
            self.cache.get(&key, span.len, Priority::Prefetch).await?;
        }
        Ok(())
    }

    /// Lists subdirectories to depth 2, at most 32 listings.
    async fn prefetch_listings(self: Arc<Self>, subdirs: Vec<Ino>) {
        let mut queue: std::collections::VecDeque<(Ino, u32)> =
            subdirs.into_iter().map(|d| (d, 1)).collect();
        let mut done = 0;
        while let Some((dir, depth)) = queue.pop_front() {
            if done >= LISTING_PREFETCH_LIMIT {
                break;
            }
            let turn = self.gate.background().await;
            let result = self.ensure_listed_now(dir).await;
            drop(turn);
            done += 1;
            if let Err(error) = result {
                tracing::debug!(dir, %error, "listing prefetch failed");
                continue;
            }
            if depth < 2 {
                let tree = self.tree.read();
                if let Some(node) = tree.get(dir) {
                    for &child in node.children.values() {
                        if tree.get(child).is_some_and(|c| c.is_dir() && !c.listed) {
                            queue.push_back((child, depth + 1));
                        }
                    }
                }
            }
        }
    }
}

/// Local changes of a profile that are not uploaded yet, from its stored
/// state (for `reset --keep-login`, which must not lose them). Staging
/// files count too.
pub fn pending_changes(state_db: &std::path::Path, staging_dir: &std::path::Path) -> Result<u64> {
    let mut pending = 0;
    if state_db.exists() {
        let store = Store::open(state_db)?;
        pending += store.read(crate::rows::count_pending)?;
    }
    if let Ok(staging) = StagingDir::open(staging_dir) {
        pending += staging.staged_inos()?.len() as u64;
    }
    Ok(pending)
}

/// The revision and clamped length of a read, or `None` at or past EOF.
fn read_range(
    rev: Option<RevisionId>,
    file_size: u64,
    offset: u64,
    size: u32,
) -> Option<(RevisionId, u64)> {
    if offset >= file_size {
        return None;
    }
    Some((rev?, u64::from(size).min(file_size - offset)))
}

/// First mount: persist the event cursor before listing anything (§9.1),
/// then the root.
async fn initialize(store: &Store, backend: &dyn Backend, metrics: &Metrics) -> Result<()> {
    check_not_hot(metrics, "latest_event_id");
    let cursor = backend.latest_event_id().await?;
    check_not_hot(metrics, "root");
    let root = backend.root().await?;
    let tree = Tree::with_root(RemoteState::from_remote(&root));
    let row = tree.row(ROOT_INO).expect("root");
    store
        .write_durable(move |conn| {
            conn.execute(
                "INSERT OR REPLACE INTO event_cursor(volume, event_id, updated_at) VALUES (?1, ?2, strftime('%s','now'))",
                (MY_FILES_VOLUME, &cursor.0),
            )?;
            write_rows(conn, &[(ROOT_INO, Some(row.clone()))])?;
            meta_set(conn, "ino_reserved", &ROOT_INO.to_string())?;
            meta_set(conn, "root_link", &row.remote.as_ref().expect("root is remote").link.0)
        })
        .await?;
    Ok(())
}

type StagedMap = HashMap<Ino, Arc<parking_lot::Mutex<Staged>>>;

/// Loads the stored tree, the inode reservation and the staged inodes.
fn load(store: &Store, staging: &StagingDir) -> Result<(Tree, Ino, StagedMap)> {
    let (rows, reserved) = store.read(|conn| {
        let reserved: Ino = meta_get(conn, "ino_reserved")?
            .and_then(|value| value.parse().ok())
            .unwrap_or(ROOT_INO);
        Ok((load_rows(conn)?, reserved))
    })?;
    if !rows.iter().any(|row| row.ino == ROOT_INO) {
        return Err(CoreError::Store(
            "the state database has no root; run `pdmount reset --keep-login`".into(),
        ));
    }
    // Never hand out a number at or below one already in use.
    let reserved = rows
        .iter()
        .map(|row| row.ino)
        .max()
        .unwrap_or(ROOT_INO)
        .max(reserved);
    let (mut tree, stale) = Tree::from_rows(rows);
    if !stale.is_empty() {
        tracing::info!(count = stale.len(), "dropping unreachable stored nodes");
        store.write(move |conn| {
            for ino in &stale {
                conn.execute("DELETE FROM inodes WHERE ino = ?1", [sql_ino(*ino)])?;
            }
            Ok(())
        });
    }
    // Staging files are the content of changed files. Staging without a
    // node is left over from a node that is gone, and staging of a node
    // stored as clean remote content from an upload whose staging removal
    // had not run yet (§7.1): the remote revision may be newer by now.
    let mut staged = HashMap::new();
    for ino in staging.staged_inos()? {
        let keep = tree.get(ino).is_some_and(|node| {
            node.attached
                && node.kind == NodeKind::File
                // Rows do not store `Content::Staged`; the generations tell.
                && !(matches!(node.content, Content::Remote { .. })
                    && node.gens.content == node.gens.uploaded_content)
        });
        if !keep {
            staging.remove(ino);
            continue;
        }
        match staging.reopen(ino) {
            Ok(file) => {
                if let Some(node) = tree.get_mut(ino) {
                    node.content = Content::Staged;
                    node.size = file.size();
                    if node.gens.content == node.gens.uploaded_content && node.remote.is_none() {
                        node.gens.content += 1;
                    }
                }
                staged.insert(ino, Arc::new(parking_lot::Mutex::new(file)));
            }
            Err(error) => {
                tracing::error!(ino, %error, "cannot reopen a staging file; its local changes are lost");
                staging.remove(ino);
            }
        }
    }
    // A new file whose staging is gone (it was empty) starts empty again.
    let missing: Vec<Ino> = tree
        .inos()
        .filter(|ino| {
            tree.get(*ino).is_some_and(|node| {
                node.attached && node.kind == NodeKind::File && node.content == Content::Staged
            }) && !staged.contains_key(ino)
        })
        .collect();
    for ino in missing {
        let file = staging.create(ino, None, None, 0)?;
        if let Some(node) = tree.get_mut(ino) {
            node.size = 0;
        }
        staged.insert(ino, Arc::new(parking_lot::Mutex::new(file)));
    }
    Ok((tree, reserved, staged))
}

#[cfg(test)]
mod proptests;
#[cfg(test)]
mod tests;
