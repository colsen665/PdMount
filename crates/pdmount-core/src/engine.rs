//! The filesystem engine behind the FUSE adapter (DESIGN.md §3.4, §4.2, §5).
//!
//! Methods without `async` are hot-path operations: they use RAM and local
//! disk only and report when the slow path is needed. `async` methods may
//! call the backend and run on tokio tasks.
//!
//! M1 serves the remote tree read-only. After a remount the stored tree is
//! served at once while every listed directory is re-listed in the
//! background ([`Engine::revalidate`]); changes found that way reach the
//! kernel through the invalidation channel. Event polling follows in M3.

use crate::Config;
use crate::cache::{BlockCache, BlockKey, CacheOptions, Layout, Priority, assemble};
use crate::errors::{CoreError, Result};
use crate::gate::Gate;
use crate::metrics::{Metrics, check_not_hot};
use crate::singleflight::SingleFlight;
use crate::store::{Store, meta_get, meta_set};
use crate::vfs::{
    ApplyError, Attr, DirEntry, Ino, Invalidation, ListingDiff, Lookup, NodeData, NodeRow,
    ROOT_INO, Tree, ns_to_system_time, system_time_to_ns,
};
use bytes::Bytes;
use pdmount_backend::{Backend, LinkId, NodeKind, RevisionId, StorageInfo};
use rusqlite::Connection;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::mpsc;

/// Volume key of the My Files event cursor (§9.1).
const MY_FILES_VOLUME: &str = "my-files";
/// Inode numbers reserved durably per store write (§8.8).
const INO_RESERVATION: u64 = 1024;
/// Spacing of speculative API requests (prefetch, revalidation): at most
/// two per second, one at a time, and none while a user request waits (D027).
const BACKGROUND_INTERVAL: Duration = Duration::from_millis(500);
const LISTING_PREFETCH_LIMIT: usize = 32;
const STATFS_REFRESH: Duration = Duration::from_secs(300);
const ACCESS_FLUSH: Duration = Duration::from_secs(60);

pub struct EngineOptions {
    pub state_db: PathBuf,
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

/// What `open` captured: reads see the revision current at open time.
#[derive(Debug, Clone)]
struct OpenFile {
    link: LinkId,
    rev: Option<RevisionId>,
    size: u64,
    /// End of the previous read, for sequential detection.
    last_end: u64,
    sequential_reads: u32,
}

struct InoAllocator {
    next: Ino,
    reserved: Ino,
}

pub struct Engine {
    backend: Arc<dyn Backend>,
    store: Arc<Store>,
    metrics: Arc<Metrics>,
    cache: Arc<BlockCache>,
    config: Config,
    tree: parking_lot::RwLock<Tree>,
    inos: tokio::sync::Mutex<InoAllocator>,
    listings: SingleFlight<Ino, ()>,
    invalidations: mpsc::UnboundedSender<Invalidation>,
    dirs: parking_lot::Mutex<HashMap<u64, Arc<Vec<DirEntry>>>>,
    files: parking_lot::Mutex<HashMap<u64, OpenFile>>,
    next_handle: AtomicU64,
    storage: parking_lot::Mutex<Option<StorageInfo>>,
    prefetched_dirs: parking_lot::Mutex<HashSet<Ino>>,
    gate: Gate,
    background: parking_lot::Mutex<Vec<tokio::task::JoinHandle<()>>>,
    runtime: tokio::runtime::Handle,
}

impl Engine {
    /// Opens the store and cache. On the first mount of a profile it records
    /// the event cursor and the root (network); otherwise it uses only the
    /// stored state. Must run inside a tokio runtime.
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
        let (tree, reserved) = load(&store)?;
        let cache = BlockCache::open(
            Arc::clone(&backend),
            Arc::clone(&store),
            Arc::clone(&metrics),
            options.cache,
        )?;
        let (sender, receiver) = mpsc::unbounded_channel();
        let engine = Arc::new(Self {
            backend,
            store,
            metrics,
            cache: Arc::new(cache),
            config: options.config,
            tree: parking_lot::RwLock::new(tree),
            inos: tokio::sync::Mutex::new(InoAllocator {
                next: reserved + 1,
                reserved,
            }),
            listings: SingleFlight::default(),
            invalidations: sender,
            dirs: parking_lot::Mutex::default(),
            files: parking_lot::Mutex::default(),
            next_handle: AtomicU64::new(1),
            storage: parking_lot::Mutex::default(),
            prefetched_dirs: parking_lot::Mutex::default(),
            gate: Gate::new(BACKGROUND_INTERVAL),
            background: parking_lot::Mutex::default(),
            runtime: tokio::runtime::Handle::current(),
        });
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

    /// Starts revalidation, quota refresh and cache bookkeeping.
    pub fn start_background(self: &Arc<Self>) {
        let mut tasks = self.background.lock();
        let this = Arc::clone(self);
        tasks.push(self.runtime.spawn(async move { this.revalidate().await }));
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
    }

    /// Stops background work and commits the store.
    pub async fn shutdown(&self) -> Result<()> {
        for task in self.background.lock().drain(..) {
            task.abort();
        }
        self.cache.flush_access();
        self.store.flush().await?;
        Ok(())
    }

    // ---- hot path ----

    pub fn lookup(&self, parent: Ino, name: &[u8]) -> Result<Lookup> {
        let name = match crate::vfs::validate_name(name) {
            Ok(name) => name,
            // A name that cannot be valid UTF-8 cannot exist remotely.
            Err(CoreError::InvalidName) => return Ok(Lookup::Negative),
            Err(error) => return Err(error),
        };
        self.tree.write().lookup(parent, name)
    }

    pub fn getattr(&self, ino: Ino) -> Result<Attr> {
        self.tree
            .read()
            .get(ino)
            .map(|node| node.attr())
            .ok_or(CoreError::NotFound)
    }

    pub fn forget(&self, ino: Ino, count: u64) {
        self.tree.write().forget(ino, count);
    }

    /// Opens a listed directory; `None` means it must be listed first.
    pub fn opendir(self: &Arc<Self>, ino: Ino) -> Result<Option<u64>> {
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
        self.tree.write().add_lookup(ino)
    }

    pub fn releasedir(&self, handle: u64) {
        self.dirs.lock().remove(&handle);
    }

    /// Whether a directory has been listed (for `FOPEN_CACHE_DIR`).
    pub fn is_listed(&self, ino: Ino) -> bool {
        self.tree.read().get(ino).is_some_and(|node| node.listed)
    }

    /// Opens a file. Returns the handle and whether the kernel may keep its
    /// cached pages (same revision as the last open).
    pub fn open_file(&self, ino: Ino) -> Result<(u64, bool)> {
        let mut tree = self.tree.write();
        let node = tree.get_mut(ino).ok_or(CoreError::NotFound)?;
        if node.is_dir() {
            return Err(CoreError::IsADirectory);
        }
        let keep_cache = node.served_rev.is_some() && node.served_rev == node.data.rev;
        node.served_rev = node.data.rev.clone();
        let file = OpenFile {
            link: node.data.link.clone(),
            rev: node.data.rev.clone(),
            size: node.data.size,
            last_end: 0,
            sequential_reads: 0,
        };
        drop(tree);
        let handle = self.next_handle.fetch_add(1, Ordering::Relaxed);
        self.files.lock().insert(handle, file);
        Ok((handle, keep_cache))
    }

    pub fn release_file(&self, handle: u64) {
        self.files.lock().remove(&handle);
    }

    /// Serves a read from RAM or disk. `None` means a block is missing and
    /// the slow path ([`Engine::read`]) must run.
    pub fn read_local(
        self: &Arc<Self>,
        handle: u64,
        offset: u64,
        size: u32,
    ) -> Result<Option<Vec<u8>>> {
        let file = self.file(handle)?;
        let Some((rev, len)) = read_range(&file, offset, size) else {
            return Ok(Some(Vec::new()));
        };
        let Some(layout) = self.cache.layout_local(&file.link, &rev, file.size) else {
            return Ok(None);
        };
        let spans = layout.spans(offset, len);
        let mut blocks = Vec::with_capacity(spans.len());
        for span in spans {
            let key = BlockKey {
                link: file.link.clone(),
                rev: rev.clone(),
                index: span.index,
            };
            match self.cache.get_local(&key) {
                Some(data) => blocks.push(data),
                None => return Ok(None),
            }
        }
        self.readahead(handle, &file, &rev, &layout, offset, len);
        Ok(Some(assemble(spans, &blocks, offset, len)))
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

    // ---- slow path ----

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
            node.data.link.clone()
        };
        check_not_hot(&self.metrics, "list_children");
        Metrics::inc(&self.metrics.listings);
        let children = self.backend.list_children(&link).await?;
        let mut inos = Vec::new();
        loop {
            let unknown = self.tree.read().unknown_count(&children);
            if unknown > inos.len() {
                inos.extend(self.allocate_inos((unknown - inos.len()) as u64).await?);
            }
            let applied = self.tree.write().apply_listing(dir, &children, &mut inos);
            match applied {
                Ok(diff) => {
                    self.persist_listing(dir, link, diff);
                    return Ok(());
                }
                Err(ApplyError::NeedInos(_)) => continue,
                Err(ApplyError::NotADirectory) => return Err(CoreError::NotFound),
            }
        }
    }

    /// Reads a range, downloading missing blocks.
    pub async fn read(self: &Arc<Self>, handle: u64, offset: u64, size: u32) -> Result<Vec<u8>> {
        let _foreground = self.gate.foreground();
        let file = self.file(handle)?;
        let Some((rev, len)) = read_range(&file, offset, size) else {
            return Ok(Vec::new());
        };
        let layout = self.cache.layout(&file.link, &rev, file.size).await?;
        let spans = layout.spans(offset, len);
        let fetches = spans.iter().map(|span| {
            let key = BlockKey {
                link: file.link.clone(),
                rev: rev.clone(),
                index: span.index,
            };
            let cache = Arc::clone(&self.cache);
            async move { cache.get(&key, span.len, Priority::Interactive).await }
        });
        let blocks: Vec<Bytes> = futures::future::try_join_all(fetches).await?;
        self.readahead(handle, &file, &rev, &layout, offset, len);
        Ok(assemble(spans, &blocks, offset, len))
    }

    /// Re-lists every listed directory, root first, as background work
    /// (D025, D027). Run after mounting so a stored tree catches up with
    /// remote changes.
    pub async fn revalidate(self: &Arc<Self>) {
        let dirs = self.tree.read().listed_dirs();
        let count = dirs.len();
        for dir in dirs {
            let _turn = self.gate.background().await;
            if let Err(error) = self.list(dir).await {
                tracing::warn!(dir, %error, "revalidation listing failed");
            }
        }
        tracing::info!(directories = count, "revalidation finished");
    }

    pub async fn refresh_storage(&self) {
        check_not_hot(&self.metrics, "storage_info");
        match self.backend.storage_info().await {
            Ok(info) => *self.storage.lock() = Some(info),
            Err(error) => tracing::warn!(%error, "cannot refresh the quota"),
        }
    }

    // ---- internals ----

    fn file(&self, handle: u64) -> Result<OpenFile> {
        self.files
            .lock()
            .get(&handle)
            .cloned()
            .ok_or(CoreError::NotFound)
    }

    async fn allocate_inos(&self, count: u64) -> Result<Vec<Ino>> {
        let mut inos = self.inos.lock().await;
        let last = inos.next + count - 1;
        if last > inos.reserved {
            let reserved = last + INO_RESERVATION;
            self.store
                .write_durable(move |conn| meta_set(conn, "ino_reserved", &reserved.to_string()))
                .await?;
            inos.reserved = reserved;
        }
        let range = inos.next..inos.next + count;
        inos.next += count;
        // `apply_listing` pops from the end; hand out ascending numbers.
        Ok(range.rev().collect())
    }

    fn persist_listing(&self, dir: Ino, dir_link: LinkId, diff: ListingDiff) {
        let ListingDiff {
            rows,
            removed,
            invalidations,
        } = diff;
        self.store.write(move |conn| {
            for (ino, link) in &removed {
                conn.execute("DELETE FROM inodes WHERE ino = ?1", [sql_ino(*ino)])?;
                conn.execute("DELETE FROM remote WHERE link_id = ?1", [&link.0])?;
            }
            conn.execute("DELETE FROM inodes WHERE parent_ino = ?1", [sql_ino(dir)])?;
            for row in &rows {
                write_row(conn, row, Some(&dir_link))?;
            }
            conn.execute(
                "UPDATE inodes SET listed = 1 WHERE ino = ?1",
                [sql_ino(dir)],
            )?;
            Ok(())
        });
        for invalidation in invalidations {
            let _ = self.invalidations.send(invalidation);
        }
    }

    /// Sequential readahead (§5.3): after two adjacent reads, fetch the next
    /// blocks (2, then 4 once the run continues) in the background.
    fn readahead(
        self: &Arc<Self>,
        handle: u64,
        file: &OpenFile,
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
                link: file.link.clone(),
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
                match child.data.kind {
                    NodeKind::Folder if !child.listed => subdirs.push(child.ino),
                    NodeKind::File
                        if child.data.size > 0
                            && child.data.size <= small
                            && child.data.size <= budget =>
                    {
                        budget -= child.data.size;
                        if let Some(rev) = &child.data.rev {
                            files.push((child.data.link.clone(), rev.clone(), child.data.size));
                        }
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

/// The revision and clamped length of a read, or `None` at or past EOF.
fn read_range(file: &OpenFile, offset: u64, size: u32) -> Option<(RevisionId, u64)> {
    if offset >= file.size {
        return None;
    }
    let rev = file.rev.clone()?;
    Some((rev, u64::from(size).min(file.size - offset)))
}

/// Inode numbers stay far below `i64::MAX`; SQLite integers are signed.
fn sql_ino(ino: Ino) -> i64 {
    ino as i64
}

fn kind_to_int(kind: NodeKind) -> i64 {
    match kind {
        NodeKind::File => 1,
        NodeKind::Folder => 2,
    }
}

fn int_to_kind(value: i64) -> NodeKind {
    if value == 2 {
        NodeKind::Folder
    } else {
        NodeKind::File
    }
}

fn write_row(
    conn: &Connection,
    row: &NodeRow,
    parent_link: Option<&LinkId>,
) -> rusqlite::Result<()> {
    let data = &row.data;
    let mtime = system_time_to_ns(data.mtime);
    let (parent, name) = if row.ino == ROOT_INO {
        (None, None)
    } else {
        (Some(sql_ino(row.parent)), Some(row.name.as_str()))
    };
    conn.execute(
        "INSERT OR REPLACE INTO inodes(ino, kind, parent_ino, name, link_id, mode, mtime_ns, ctime_ns, size, listed)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7, ?8, ?9)",
        rusqlite::params![
            sql_ino(row.ino),
            kind_to_int(data.kind),
            parent,
            name,
            data.link.0,
            data.mode.unwrap_or(0),
            mtime,
            data.size as i64,
            row.listed,
        ],
    )?;
    conn.execute(
        "INSERT OR REPLACE INTO remote(link_id, parent_link, name, kind, active_rev, size, mtime_ns, mode, trashed, seen_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0, strftime('%s','now'))",
        rusqlite::params![
            data.link.0,
            parent_link.map(|link| link.0.as_str()),
            row.name,
            kind_to_int(data.kind),
            data.rev.as_ref().map(|rev| rev.0.as_str()),
            data.size as i64,
            mtime,
            data.mode,
        ],
    )?;
    Ok(())
}

/// First mount: persist the event cursor before listing anything (§9.1),
/// then the root.
async fn initialize(store: &Store, backend: &dyn Backend, metrics: &Metrics) -> Result<()> {
    check_not_hot(metrics, "latest_event_id");
    let cursor = backend.latest_event_id().await?;
    check_not_hot(metrics, "root");
    let root = backend.root().await?;
    let row = NodeRow {
        ino: ROOT_INO,
        parent: ROOT_INO,
        name: String::new(),
        data: NodeData::from_remote(&root),
        listed: false,
    };
    store
        .write_durable(move |conn| {
            conn.execute(
                "INSERT OR REPLACE INTO event_cursor(volume, event_id, updated_at) VALUES (?1, ?2, strftime('%s','now'))",
                (MY_FILES_VOLUME, &cursor.0),
            )?;
            write_row(conn, &row, None)?;
            meta_set(conn, "ino_reserved", &ROOT_INO.to_string())?;
            meta_set(conn, "root_link", &row.data.link.0)
        })
        .await?;
    Ok(())
}

/// Loads the stored tree and the inode reservation.
fn load(store: &Store) -> Result<(Tree, Ino)> {
    let (rows, reserved) = store.read(|conn| {
        let reserved: Ino = meta_get(conn, "ino_reserved")?
            .and_then(|value| value.parse().ok())
            .unwrap_or(ROOT_INO);
        let mut statement = conn.prepare(
            "SELECT i.ino, i.kind, i.parent_ino, i.name, i.link_id, i.listed, r.active_rev, r.size, r.mtime_ns, r.mode
             FROM inodes i JOIN remote r ON r.link_id = i.link_id
             WHERE i.deleted = 0",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok(NodeRow {
                    ino: row.get::<_, i64>(0)? as Ino,
                    parent: row.get::<_, Option<i64>>(2)?.map_or(ROOT_INO, |parent| parent as Ino),
                    name: row.get::<_, Option<String>>(3)?.unwrap_or_default(),
                    listed: row.get(5)?,
                    data: NodeData {
                        link: LinkId(row.get(4)?),
                        kind: int_to_kind(row.get(1)?),
                        rev: row.get::<_, Option<String>>(6)?.map(RevisionId),
                        size: row.get::<_, i64>(7)? as u64,
                        mtime: ns_to_system_time(row.get(8)?),
                        mode: row.get(9)?,
                    },
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok((rows, reserved))
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
    let (tree, stale) = Tree::from_rows(rows);
    if !stale.is_empty() {
        tracing::info!(count = stale.len(), "dropping unreachable stored nodes");
        store.write(move |conn| {
            for (ino, link) in &stale {
                conn.execute("DELETE FROM inodes WHERE ino = ?1", [sql_ino(*ino)])?;
                conn.execute("DELETE FROM remote WHERE link_id = ?1", [&link.0])?;
            }
            Ok(())
        });
    }
    Ok((tree, reserved))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::hot;
    use pdmount_backend::fake::FakeBackend;
    use pdmount_backend::{FileMeta, UploadSource};
    use std::time::UNIX_EPOCH;

    fn meta() -> FileMeta {
        FileMeta {
            mtime: UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            mode: Some(0o600),
            media_type: "application/octet-stream".into(),
        }
    }

    async fn seeded() -> FakeBackend {
        let backend = FakeBackend::new();
        backend.set_block_size(4);
        let root = backend.root().await.expect("root").link_id;
        let docs = backend
            .create_folder(&root, "docs", UNIX_EPOCH)
            .await
            .expect("mkdir");
        backend
            .upload_new(
                &docs,
                "a.txt",
                UploadSource::Bytes(Bytes::from_static(b"hello world")),
                meta(),
            )
            .await
            .expect("upload");
        backend
            .upload_new(
                &root,
                "top.bin",
                UploadSource::Bytes(Bytes::from_static(b"0123456789")),
                meta(),
            )
            .await
            .expect("upload");
        backend
    }

    async fn open(
        backend: &FakeBackend,
        dir: &std::path::Path,
    ) -> (Arc<Engine>, mpsc::UnboundedReceiver<Invalidation>) {
        Engine::open(
            Arc::new(backend.clone()),
            EngineOptions {
                state_db: dir.join("state.db"),
                cache: CacheOptions {
                    dir: dir.join("blocks"),
                    master_key: [1; 32],
                    ram_bytes: 1 << 20,
                    disk_bytes: 1 << 20,
                },
                config: Config::default(),
            },
        )
        .await
        .expect("engine")
    }

    fn found(lookup: Lookup) -> Attr {
        match lookup {
            Lookup::Found(attr) => attr,
            other => panic!("expected an entry, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn listing_lookup_and_read_through_the_cache() {
        let dir = tempfile::tempdir().expect("tempdir");
        let backend = seeded().await;
        let (engine, _inval) = open(&backend, dir.path()).await;
        assert_eq!(
            hot(|| engine.lookup(ROOT_INO, b"docs")).expect("lookup"),
            Lookup::NeedsListing
        );
        engine.ensure_listed(ROOT_INO).await.expect("list");
        let docs = found(hot(|| engine.lookup(ROOT_INO, b"docs")).expect("lookup"));
        assert_eq!(docs.kind, NodeKind::Folder);
        engine.ensure_listed(docs.ino).await.expect("list docs");
        let file = found(hot(|| engine.lookup(docs.ino, b"a.txt")).expect("lookup"));
        assert_eq!((file.size, file.perm), (11, 0o600));
        assert_eq!(
            hot(|| engine.lookup(docs.ino, b"nope")).expect("lookup"),
            Lookup::Negative
        );

        let (handle, keep) = engine.open_file(file.ino).expect("open");
        assert!(!keep);
        assert_eq!(
            hot(|| engine.read_local(handle, 0, 100)).expect("local"),
            None
        );
        assert_eq!(engine.read(handle, 3, 6).await.expect("read"), b"lo wor");
        assert_eq!(
            hot(|| engine.read_local(handle, 0, 100))
                .expect("local")
                .as_deref(),
            Some(&b"hello world"[..])
        );
        assert_eq!(
            hot(|| engine.read_local(handle, 11, 5))
                .expect("eof")
                .as_deref(),
            Some(&b""[..])
        );
        engine.release_file(handle);
        let (_, keep) = engine.open_file(file.ino).expect("reopen");
        assert!(keep, "same revision keeps the page cache");
        assert_eq!(
            engine
                .metrics()
                .hot_path_network_calls
                .load(Ordering::Relaxed),
            0
        );
    }

    #[tokio::test]
    async fn a_remount_serves_the_stored_tree_with_the_same_inodes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let backend = seeded().await;
        let first = {
            let (engine, _inval) = open(&backend, dir.path()).await;
            engine.ensure_listed(ROOT_INO).await.expect("list");
            let attr = found(engine.lookup(ROOT_INO, b"top.bin").expect("lookup"));
            engine.shutdown().await.expect("shutdown");
            attr
        };
        let (engine, _inval) = open(&backend, dir.path()).await;
        let calls = engine.metrics().backend_calls.load(Ordering::Relaxed);
        let again = found(hot(|| engine.lookup(ROOT_INO, b"top.bin")).expect("lookup"));
        assert_eq!(again, first);
        assert_eq!(
            engine.metrics().backend_calls.load(Ordering::Relaxed),
            calls
        );
    }

    #[tokio::test]
    async fn revalidation_applies_remote_changes_and_invalidates() {
        let dir = tempfile::tempdir().expect("tempdir");
        let backend = seeded().await;
        let (engine, mut inval) = open(&backend, dir.path()).await;
        engine.ensure_listed(ROOT_INO).await.expect("list");
        let root = backend.root().await.expect("root").link_id;
        backend
            .upload_new(
                &root,
                "new.txt",
                UploadSource::Bytes(Bytes::from_static(b"n")),
                meta(),
            )
            .await
            .expect("upload");
        engine.revalidate().await;
        found(hot(|| engine.lookup(ROOT_INO, b"new.txt")).expect("lookup"));
        assert_eq!(
            inval.try_recv().expect("invalidation"),
            Invalidation::Entry {
                parent: ROOT_INO,
                name: "new.txt".into()
            }
        );
    }

    #[tokio::test]
    async fn inode_numbers_are_never_reused_across_reopen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let backend = seeded().await;
        let reserved = {
            let (engine, _inval) = open(&backend, dir.path()).await;
            engine.ensure_listed(ROOT_INO).await.expect("list");
            let reserved = engine.inos.lock().await.reserved;
            engine.shutdown().await.expect("shutdown");
            reserved
        };
        let (engine, _inval) = open(&backend, dir.path()).await;
        assert!(engine.inos.lock().await.next > reserved);
    }

    #[tokio::test]
    async fn a_state_database_of_another_account_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let backend = seeded().await;
        drop(open(&backend, dir.path()).await);
        let store = Store::open(&dir.path().join("state.db")).expect("store");
        store
            .write_durable(|conn| meta_set(conn, "root_link", "someone-else"))
            .await
            .expect("write");
        drop(store);
        let result = Engine::open(
            Arc::new(backend),
            EngineOptions {
                state_db: dir.path().join("state.db"),
                cache: CacheOptions {
                    dir: dir.path().join("blocks"),
                    master_key: [1; 32],
                    ram_bytes: 1 << 20,
                    disk_bytes: 1 << 20,
                },
                config: Config::default(),
            },
        )
        .await;
        assert!(
            matches!(result, Err(CoreError::Store(message)) if message.contains("another account"))
        );
    }

    #[tokio::test]
    async fn prefetch_is_paced_instead_of_bursting() {
        let dir = tempfile::tempdir().expect("tempdir");
        let backend = FakeBackend::new();
        let root = backend.root().await.expect("root").link_id;
        for i in 0..20 {
            backend
                .upload_new(
                    &root,
                    &format!("f{i}"),
                    UploadSource::Bytes(Bytes::from_static(b"x")),
                    meta(),
                )
                .await
                .expect("upload");
        }
        let (engine, _inval) = open(&backend, dir.path()).await;
        engine.ensure_listed(ROOT_INO).await.expect("list");
        let before = engine.metrics().backend_calls.load(Ordering::Relaxed);
        engine.opendir(ROOT_INO).expect("opendir").expect("listed");
        tokio::time::sleep(Duration::from_millis(1200)).await;
        // Turns start at 0, 0.5 and 1.0 s; each prefetches one file with two
        // calls (block list, block). Unpaced, all 20 files would be done.
        let calls = engine.metrics().backend_calls.load(Ordering::Relaxed) - before;
        assert!((2..=6).contains(&calls), "{calls} backend calls in 1.2 s");
    }

    #[tokio::test]
    async fn sequential_reads_trigger_readahead() {
        let dir = tempfile::tempdir().expect("tempdir");
        let backend = seeded().await;
        let (engine, _inval) = open(&backend, dir.path()).await;
        engine.ensure_listed(ROOT_INO).await.expect("list");
        let file = found(engine.lookup(ROOT_INO, b"top.bin").expect("lookup"));
        let (handle, _) = engine.open_file(file.ino).expect("open");
        engine.read(handle, 0, 2).await.expect("read");
        engine.read(handle, 2, 2).await.expect("read");
        for _ in 0..50 {
            if engine.metrics().prefetched_blocks.load(Ordering::Relaxed) > 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("no readahead happened");
    }
}
