//! The Photos tree (M5.13, D080, D081, D113): `Photos/<year>/<month>/<name>`,
//! read-only, over the Photos volume's timeline. A virtual tree, not an
//! engine tree: the official Photos client has no folder listing, folder
//! creation, move or revision upload for Photos, only a flat timeline.
//!
//! Folders come in stages:
//!
//! 1. **Years and months** from the timeline pages alone (link, capture
//!    time, tags, related photos; 500 per page, newest first). Read once,
//!    in the background from the mount on, paced like other background work
//!    (the session's [`Gate`], shared with every tree, D110); the event
//!    cursor is taken before the first page, so what changes during the
//!    read arrives as events. While nothing is shown yet, a page waits for
//!    the gate at most [`sync::USER_PATIENCE`] (D119). An empty page after
//!    a link means that link went, not the end; a read that fails goes on
//!    from its last page; photos go only after a read that finished.
//! 2. **Names and sizes** when a month is opened (`opendir` or a lookup in
//!    it): its photos' nodes in batches of [`NAME_BATCH`], one background
//!    turn each, the month opened last first. A request a user waits on
//!    fetches the next batch itself: the first of a month without names
//!    outside the gate, later ones with a patient turn (D119). A month whose
//!    names arrive in stages shows those it has, and its listing changes at
//!    each batch, so the kernel keeps its pages only while the entries hash
//!    the same (D072's rule, [`PhotosTree::opendir`]). A month opened once
//!    keeps getting the names of its new photos. A node that cannot be
//!    decrypted keeps what is known of it.
//! 3. **Events** of the Photos volume, polled at `sync.poll_interval`: it is
//!    the user's own volume (D107). Photos is the only tree on it, so the
//!    tree polls it itself, with the pollers' back-off. A new link is read
//!    with its node (name, size) and found in the timeline (its capture
//!    time): the first page at every poll, further pages at most every
//!    [`sync::DEEP_SEARCH_INTERVAL`]; a trashed or deleted one goes, with
//!    its related photos; a changed one has its node read again. What
//!    cannot be applied now waits in `pending` for the next poll, so the
//!    cursor always moves on (D119). A `refresh` reads the whole timeline
//!    again, keeping the names.
//!
//! Everything a stage learns is kept in the tree's store ([`rows`]), so a
//! remount reads neither the timeline nor the names again: it continues the
//! event stream from the stored cursor.
//!
//! Reads go through the tree's own [`BlockCache`] on the profile's disk cap,
//! block by block with the backend's manifest verification, as the engine's
//! do, with the session's download permits. The session's bulk-read guard
//! ([`ScanGuard`], D111) sees the tree's opens as it sees the engines': a
//! photo opened during a scan reads its first bytes as a scan read (a turn
//! of the guard, scan priority, the scan share of the cache). No readahead
//! for any read.
//!
//! Live Photos and Motion Photos (tags 3 and 4): the timeline's related
//! photos (the video of a Live Photo) show beside their main photo, in its
//! month, under their own names.
//!
//! When a month is opened, [`ThumbnailHook::month_opened`] gets its named
//! photos: the tree's [`Thumbnailer`] (M5.13b, D115) then writes Proton's
//! thumbnails of them to the freedesktop thumbnail cache in the background,
//! paced by the same gate (`photos.thumbnail_cache`).
//!
//! # Lock order
//!
//! The model, then the directory handles or the open files; the store and
//! the invalidation channel are leaves. No lock is held across an `.await`.
//! Asynchronous: a background turn of the session's gate is held across
//! one backend call and never asked for again by its holder; a scan read's
//! turn of the guard is taken before its downloads, as the engine's.
//!
//! # Module map
//!
//! - `photos.rs` (here): [`PhotosTree`], its options, opening, and the
//!   [`ReadOnlyTree`] the FUSE layer calls.
//! - `model.rs`: [`Model`], years, months, photos and their inode numbers.
//! - `rows.rs`: the store's `photos` table and the stage markers.
//! - `sync.rs`: the background stages: timeline reads, events, names.
//! - `thumbnails.rs`: the thumbnails of the months opened (M5.13b).
//! - `tests.rs`: the tests; `tests/session.rs`: the tree beside an engine
//!   tree on the session's gate and bulk-read guard.

mod model;
mod rows;
mod sync;
#[cfg(test)]
mod tests;
pub mod thumbnails;

pub use model::{Month, Photo, PhotoMeta, Placer, local_month, utc_month};
pub use thumbnails::{ThumbnailOptions, Thumbnailer};

use crate::Config;
use crate::cache::{BlockCache, BlockKey, CacheOptions, Layout, Priority, READ_BLOCKS_HELD};
use crate::cache::{ReadData, ReadFrom};
use crate::engine::StatFs;
use crate::errors::{CoreError, Result};
use crate::gate::{Gate, Turn};
use crate::health::{Health, Monitored};
use crate::limits::SessionLimits;
use crate::metrics::{Metrics, TimedBackend};
use crate::readonly::ReadOnlyTree;
use crate::scan::{ScanGuard, UserWait};
use crate::staging::Protections;
use crate::store::Store;
use crate::vfs::{Attr, DirEntry, Ino, Invalidation, Lookup, MAX_NAME_BYTES, ROOT_INO};
use futures::StreamExt;
use model::{Model, Node, month_name, parse_month, parse_year, year_name};
use pdmount_backend::photos::Photos;
use pdmount_backend::{Backend, EventId, LinkId, NodeKind, RevisionId};
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{Notify, mpsc, watch};

/// Photos whose nodes one request reads: the SDK's batch of uncached
/// nodes (P13).
pub const NAME_BATCH: usize = 50;
const FILE_MODE: u32 = 0o444;
const DIR_MODE: u32 = 0o555;

pub struct PhotosOptions {
    /// The Photos volume and its root.
    pub volume: String,
    pub root: LinkId,
    pub state_db: PathBuf,
    pub cache: CacheOptions,
    pub config: Config,
    /// The session's connectivity (§9.6); `None`: one of its own.
    pub health: Option<Arc<Health>>,
    /// The month of a capture time: [`local_month`] in the daemon.
    pub placer: Placer,
    /// Where the thumbnails of the months opened go (M5.13b); `None`, or
    /// `photos.thumbnail_cache = false`: none are written.
    pub thumbnails: Option<ThumbnailOptions>,
    /// The session's bulk-read guard (D081, D111), as
    /// `EngineOptions::scan`. `None`: one of its own, from `config.scan`
    /// and the cache's cap.
    pub scan: Option<Arc<ScanGuard>>,
    /// The session's download permits and background gate (D027, D033,
    /// D110), as `EngineOptions::limits`; they replace `cache.downloads`.
    /// `None`: its own, from `config`.
    pub limits: Option<Arc<SessionLimits>>,
}

/// A photo as [`ThumbnailHook`] sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhotoView {
    pub link: LinkId,
    pub revision: RevisionId,
    /// Its path below the mount: `<year>/<month>/<name>`.
    pub path: String,
    pub size: u64,
    /// The modification time the mount reports.
    pub mtime: SystemTime,
}

/// Told when a month is opened ([`Thumbnailer`]: Proton's thumbnails into
/// `~/.cache/thumbnails`, at most 30 per request, the 512 px WebP as PNG,
/// D107, D115). Called on the FUSE thread: it must only queue work.
pub trait ThumbnailHook: Send + Sync {
    fn month_opened(&self, month: Month, photos: Vec<PhotoView>);
}

/// What `status` shows of the tree.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PhotosStatus {
    pub photos: usize,
    pub named: usize,
    /// A whole timeline read has finished.
    pub timeline_complete: bool,
    /// Months whose names are being fetched.
    pub fetching: usize,
    pub last_poll: Option<Duration>,
    /// Links whose events wait for another try (D119).
    pub pending: usize,
    /// Photos whose node cannot be decrypted, without a name (D119).
    pub unreadable: usize,
    /// Background tasks that panicked and were started again (D119).
    pub restarts: usize,
}

#[derive(Debug, Clone)]
struct OpenPhoto {
    link: LinkId,
    revision: RevisionId,
    size: u64,
    /// Opened during a scan (D111): its first `scan.first_bytes` are scan
    /// reads.
    scan: bool,
    /// Bytes read through the handle so far.
    read_bytes: u64,
}

/// Open folder handles, and per folder the digest of the entries the
/// kernel was last given (D072).
#[derive(Default)]
struct DirHandles {
    open: HashMap<u64, (Ino, Arc<Vec<DirEntry>>)>,
    served: HashMap<Ino, u64>,
}

pub struct PhotosTree {
    backend: Arc<dyn Backend>,
    photos: Arc<dyn Photos>,
    volume: String,
    root: LinkId,
    store: Arc<Store>,
    cache: Arc<BlockCache>,
    metrics: Arc<Metrics>,
    health: Arc<Health>,
    config: Config,
    /// The session's background turns (D027, D110).
    gate: Arc<Gate>,
    /// The session's bulk-read guard (D111), and the tree's id on it.
    scan: Arc<ScanGuard>,
    scan_tree: u64,
    model: parking_lot::RwLock<Model>,
    dirs: parking_lot::Mutex<DirHandles>,
    files: parking_lot::Mutex<HashMap<u64, OpenPhoto>>,
    /// The revision each photo's pages in the kernel come from.
    served: parking_lot::Mutex<HashMap<Ino, RevisionId>>,
    next_handle: AtomicU64,
    /// Months whose names are wanted, the one asked for last at the end.
    wanted: parking_lot::Mutex<Vec<Month>>,
    wake: Notify,
    /// Bumped whenever a stage changed the model: waiters look again.
    progress: watch::Sender<u64>,
    timeline_complete: AtomicBool,
    cursor: parking_lot::Mutex<Option<EventId>>,
    /// Links of events that the timeline does not have (drafts, a photo
    /// only in an album) or whose node cannot be decrypted: looked for
    /// again only at their next event (D119).
    strangers: parking_lot::Mutex<HashSet<LinkId>>,
    /// Links whose event could not be applied (a node or the timeline not
    /// read): tried again at each poll; stored (D119).
    pending: parking_lot::Mutex<HashSet<LinkId>>,
    /// When a search for new photos last read beyond the first page.
    last_deep_search: parking_lot::Mutex<Option<Instant>>,
    /// A timeline read that has not finished, to go on with.
    reading: parking_lot::Mutex<Option<sync::TimelineRead>>,
    /// Held across a names request, so the names task and a request a
    /// user waits on never fetch the same batch. Taken after a gate turn.
    names_flight: tokio::sync::Mutex<()>,
    /// Background tasks that panicked and were started again.
    restarts: AtomicUsize,
    invalidations: mpsc::UnboundedSender<Invalidation>,
    tasks: parking_lot::Mutex<Vec<tokio::task::JoinHandle<()>>>,
    last_poll: parking_lot::Mutex<Option<Instant>>,
    hook: parking_lot::RwLock<Option<Arc<dyn ThumbnailHook>>>,
    /// The thumbnails' writer, the hook unless a test set another.
    thumbnailer: Option<Arc<Thumbnailer>>,
    runtime: tokio::runtime::Handle,
}

impl PhotosTree {
    /// Opens the tree's store and cache and loads what an earlier mount
    /// learnt. Asks nothing of the server: [`PhotosTree::start`] begins the
    /// stages. Must run inside a tokio runtime.
    pub async fn open(
        backend: Arc<dyn Backend>,
        photos: Arc<dyn Photos>,
        options: PhotosOptions,
    ) -> Result<(Arc<Self>, mpsc::UnboundedReceiver<Invalidation>)> {
        let health = options.health.clone().unwrap_or_default();
        let metrics = Arc::new(Metrics::default());
        let timed = Arc::new(TimedBackend::new(backend, Arc::clone(&metrics)));
        let backend: Arc<dyn Backend> = Arc::new(Monitored::new(timed, Arc::clone(&health)));
        if let Some(parent) = options.state_db.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let store = Arc::new(Store::open(&options.state_db)?);
        let loaded = rows::open(&store, &options.volume, &options.root).await?;
        let scan = options.scan.clone().unwrap_or_else(|| {
            let cap = options
                .cache
                .shared
                .as_ref()
                .map_or(options.cache.disk_bytes, |shared| shared.budget.cap());
            ScanGuard::new(&options.config.scan, cap)
        });
        let limits = options
            .limits
            .clone()
            .unwrap_or_else(|| SessionLimits::new(&options.config));
        let mut cache_options = options.cache;
        cache_options.downloads = Some(Arc::clone(&limits.downloads));
        let cache = Arc::new(BlockCache::open(
            Arc::clone(&backend),
            Arc::clone(&store),
            Arc::clone(&metrics),
            cache_options,
            Protections::default(),
        )?);
        let seat = {
            // Weak: the guard never keeps a closed tree's cache.
            let cache = Arc::downgrade(&cache);
            scan.join(Arc::new(move |key: &BlockKey| {
                if let Some(cache) = cache.upgrade() {
                    cache.evict_scan_block(key);
                }
            }))
        };
        let scan_tree = seat.id();
        cache.set_scan_seat(seat);
        let mut model = Model::new(options.placer);
        let stored = loaded.photos.len();
        // Main photos first, so related ones find their month.
        let (mains, related): (Vec<_>, Vec<_>) = loaded
            .photos
            .into_iter()
            .partition(|(_, photo)| photo.main.is_none());
        for (link, photo) in mains.into_iter().chain(related) {
            model.put(link, photo);
        }
        let unfinished = model.unfinished();
        if loaded.timeline_complete {
            tracing::info!(
                photos = stored,
                named = model.named(),
                "photos: timeline and names loaded from the store; the timeline is not read again"
            );
        }
        let (sender, receiver) = mpsc::unbounded_channel();
        let thumbnailer = options
            .thumbnails
            .filter(|_| options.config.photos.thumbnail_cache)
            .map(|thumbnails| Arc::new(Thumbnailer::new(thumbnails)));
        let hook = thumbnailer
            .clone()
            .map(|thumbnailer| thumbnailer as Arc<dyn ThumbnailHook>);
        let tree = Arc::new(Self {
            backend,
            photos,
            volume: options.volume,
            root: options.root,
            store,
            cache,
            metrics,
            health,
            config: options.config,
            gate: Arc::clone(&limits.gate),
            scan,
            scan_tree,
            model: parking_lot::RwLock::new(model),
            dirs: parking_lot::Mutex::default(),
            files: parking_lot::Mutex::default(),
            served: parking_lot::Mutex::default(),
            next_handle: AtomicU64::new(1),
            wanted: parking_lot::Mutex::new(unfinished),
            wake: Notify::new(),
            progress: watch::Sender::new(0),
            timeline_complete: AtomicBool::new(loaded.timeline_complete),
            cursor: parking_lot::Mutex::new(loaded.cursor),
            strangers: parking_lot::Mutex::default(),
            pending: parking_lot::Mutex::new(loaded.pending.into_iter().collect()),
            last_deep_search: parking_lot::Mutex::default(),
            reading: parking_lot::Mutex::default(),
            names_flight: tokio::sync::Mutex::new(()),
            restarts: AtomicUsize::new(0),
            invalidations: sender,
            tasks: parking_lot::Mutex::default(),
            last_poll: parking_lot::Mutex::default(),
            hook: parking_lot::RwLock::new(hook),
            thumbnailer,
            runtime: tokio::runtime::Handle::current(),
        });
        Ok((tree, receiver))
    }

    /// Starts the stages: the timeline read (unless a whole one is
    /// stored), then the events; the names of the months asked for; and
    /// their thumbnails. Each runs again if it panics (D119).
    pub fn start(self: &Arc<Self>) {
        let mut tasks = self.tasks.lock();
        tasks.push(self.supervised("timeline and events", |this| this.run_timeline_and_events()));
        tasks.push(self.supervised("names", |this| this.run_names()));
        if let Some(thumbnailer) = self.thumbnailer.clone() {
            tasks.push(self.supervised("thumbnails", move |this| {
                let thumbnailer = Arc::clone(&thumbnailer);
                async move { this.run_thumbnails(thumbnailer).await }
            }));
        }
    }

    /// Stops the stages and commits the store.
    pub async fn shutdown(&self) -> Result<()> {
        for task in self.tasks.lock().drain(..) {
            task.abort();
        }
        self.cache.flush_access();
        self.store.flush().await?;
        Ok(())
    }

    /// Who is told of the months opened: the tree's [`Thumbnailer`] unless
    /// this replaces it (tests).
    pub fn set_thumbnail_hook(&self, hook: Option<Arc<dyn ThumbnailHook>>) {
        *self.hook.write() = hook;
    }

    pub fn volume(&self) -> &str {
        &self.volume
    }

    /// The thumbnails' writer, when `photos.thumbnail_cache` is on.
    pub fn thumbnailer(&self) -> Option<&Arc<Thumbnailer>> {
        self.thumbnailer.as_ref()
    }

    pub fn status(&self) -> PhotosStatus {
        let model = self.model.read();
        PhotosStatus {
            photos: model.count(),
            named: model.named(),
            timeline_complete: self.timeline_complete.load(Ordering::Acquire),
            fetching: self
                .wanted
                .lock()
                .iter()
                .filter(|month| !model.complete(**month))
                .count(),
            last_poll: self.last_poll.lock().map(|at| at.elapsed()),
            pending: self.pending.lock().len(),
            unreadable: model.unreadable(),
            restarts: self.restarts.load(Ordering::Relaxed),
        }
    }

    /// Bytes of the tree's block cache on disk, and its blocks.
    pub fn cache_usage(&self) -> (u64, usize) {
        self.cache.usage()
    }

    /// Opens the disk cache with the key that came after the start.
    pub fn unlock_cache(&self, master_key: &[u8; 32]) -> bool {
        self.cache.unlock(master_key)
    }

    /// Wakes waiters on the model.
    fn changed(&self) {
        self.progress.send_modify(|n| *n += 1);
    }

    /// Asks for the names of `month`, before the months asked for earlier.
    fn want(&self, month: Month) {
        {
            let mut wanted = self.wanted.lock();
            if wanted.last() == Some(&month) {
                return;
            }
            wanted.retain(|other| *other != month);
            wanted.push(month);
        }
        self.wake.notify_one();
    }

    fn dir_attr(ino: Ino, newest: Option<i64>) -> Attr {
        let time = newest.map_or(UNIX_EPOCH, time_of);
        Attr {
            ino,
            kind: NodeKind::Folder,
            size: 0,
            mtime: time,
            ctime: time,
            atime: time,
            perm: DIR_MODE,
            nlink: 1,
        }
    }

    fn attr_of(model: &Model, ino: Ino) -> Result<Attr> {
        match model.node(ino).ok_or(CoreError::NotFound)? {
            Node::Root => Ok(Self::dir_attr(ino, model.newest(None))),
            Node::Year(year) => Ok(Self::dir_attr(ino, model.newest_in_year(*year))),
            Node::Month(month) => Ok(Self::dir_attr(ino, model.newest(Some(*month)))),
            Node::Photo(link) => {
                let photo = model.photo(link).ok_or(CoreError::NotFound)?;
                let meta = photo.meta.as_ref().ok_or(CoreError::NotFound)?;
                let time = time_of(photo.capture);
                Ok(Attr {
                    ino,
                    kind: NodeKind::File,
                    size: meta.size,
                    mtime: time,
                    ctime: time,
                    atime: time,
                    perm: FILE_MODE,
                    nlink: 1,
                })
            }
        }
    }

    /// The entries of folder `ino`, `.` and `..` first; `NeedsListing`
    /// while it has none yet and its stage is still running.
    fn entries(&self, ino: Ino) -> Result<(Vec<DirEntry>, Option<Month>)> {
        let mut model = self.model.write();
        let node = model.node(ino).cloned().ok_or(CoreError::NotFound)?;
        let folder = |ino, name: String| DirEntry {
            ino,
            kind: NodeKind::Folder,
            name,
        };
        let (parent, children, month) = match &node {
            Node::Root => {
                let years = model.years();
                if years.is_empty() && !self.timeline_complete.load(Ordering::Acquire) {
                    return Err(CoreError::NeedsListing(ino));
                }
                let children = years
                    .into_iter()
                    .map(|year| folder(model.ino(Node::Year(year)), year_name(year)))
                    .collect();
                (ROOT_INO, children, None)
            }
            Node::Year(year) => {
                let children = model
                    .months_of(*year)
                    .into_iter()
                    .map(|month| folder(model.ino(Node::Month((*year, month))), month_name(month)))
                    .collect();
                (ROOT_INO, children, None)
            }
            Node::Month(month) => {
                let entries = model.entries(*month);
                if entries.is_empty() && !model.complete(*month) {
                    drop(model);
                    self.want(*month);
                    return Err(CoreError::NeedsListing(ino));
                }
                let children = entries
                    .into_iter()
                    .map(|(name, link)| DirEntry {
                        ino: model.ino(Node::Photo(link)),
                        kind: NodeKind::File,
                        name,
                    })
                    .collect();
                (model.ino(Node::Year(month.0)), children, Some(*month))
            }
            Node::Photo(_) => return Err(CoreError::NotADirectory),
        };
        let mut entries = vec![folder(ino, ".".into()), folder(parent, "..".into())];
        entries.extend::<Vec<DirEntry>>(children);
        Ok((entries, month))
    }

    /// The named photos of `month`, for the thumbnail hook.
    fn views(model: &Model, month: Month) -> Vec<PhotoView> {
        model
            .entries(month)
            .into_iter()
            .filter_map(|(name, link)| {
                let photo = model.photo(&link)?;
                let meta = photo.meta.as_ref()?;
                Some(PhotoView {
                    path: format!("{}/{}/{name}", year_name(month.0), month_name(month.1)),
                    revision: meta.revision.clone(),
                    size: meta.size,
                    mtime: time_of(photo.capture),
                    link,
                })
            })
            .collect()
    }

    /// Whether the entries of folder `dir` are all there: the timeline for
    /// the top folders, the names for a month.
    fn settled(&self, dir: Ino) -> bool {
        let model = self.model.read();
        match model.node(dir) {
            Some(Node::Month(month)) => model.complete(*month),
            Some(Node::Photo(_)) => true,
            Some(Node::Root | Node::Year(_)) => self.timeline_complete.load(Ordering::Acquire),
            // A month or year that does not exist yet may come with the
            // timeline.
            None => self.timeline_complete.load(Ordering::Acquire),
        }
    }

    /// `[offset, offset + len)` of a revision from RAM or disk, as the
    /// engine's `read_blocks_local`.
    fn read_blocks_local(
        &self,
        file: &OpenPhoto,
        layout: &Layout,
        offset: u64,
        len: u64,
    ) -> Option<(ReadData, ReadFrom)> {
        let key = |index| BlockKey::new(&file.link, &file.revision, index);
        match layout.spans(offset, len) {
            [] => Some((ReadData::empty(), ReadFrom::Ram)),
            [span] => {
                let (data, from) = self.cache.get_local_from(&key(span.index))?;
                Some((ReadData::of_block(span, data, offset, len), from))
            }
            spans => {
                let mut out = self.cache.read_buf_now(offset, len);
                let mut from = ReadFrom::Ram;
                for span in spans {
                    let (data, source) = self.cache.get_local_from(&key(span.index))?;
                    out.append(span, &data);
                    from = from.max(source);
                }
                Some((out.finish(), from))
            }
        }
    }

    /// [`PhotosTree::read_blocks_local`], downloading what is missing, as
    /// the engine's `read_layout`.
    async fn read_layout(
        &self,
        file: &OpenPhoto,
        layout: &Layout,
        offset: u64,
        len: u64,
        priority: Priority,
    ) -> Result<ReadData> {
        let key = |index| BlockKey::new(&file.link, &file.revision, index);
        let spans = match layout.spans(offset, len) {
            [] => return Ok(ReadData::empty()),
            [span] => {
                let data = self.cache.get(&key(span.index), span.len, priority).await?;
                return Ok(ReadData::of_block(span, data, offset, len));
            }
            spans => spans,
        };
        let fetches: Vec<_> = spans
            .iter()
            .map(|span| {
                let (cache, key, block_len) = (Arc::clone(&self.cache), key(span.index), span.len);
                async move { cache.get(&key, block_len, priority).await }
            })
            .collect();
        let mut blocks = futures::stream::iter(fetches).buffered(READ_BLOCKS_HELD);
        let mut out = None;
        for span in spans {
            let data = blocks.next().await.expect("one block per span")?;
            let out = match &mut out {
                Some(out) => out,
                None => out.insert(self.cache.read_buf(offset, len).await),
            };
            out.append(span, &data);
        }
        Ok(out.map_or_else(ReadData::empty, |out| out.finish()))
    }

    /// Marks a request a user waits on, for the session's background turns
    /// and scan reads (as the engine's `user_wait`).
    pub(crate) fn user_wait(&self) -> UserWait<'_> {
        self.scan.user_wait(&self.gate)
    }

    /// Whether the next read through `file` is a scan read: the photo was
    /// opened during a scan and fewer than `scan.first_bytes` were read.
    fn is_scan_read(&self, file: &OpenPhoto) -> bool {
        file.scan && file.read_bytes < self.scan.first_bytes()
    }

    /// A scan read's turn (D111), when it needs the network: none for a
    /// read whose layout and blocks are here or already being downloaded.
    /// Waits at most a third of the block timeout, as the engine's.
    async fn scan_turn(&self, file: &OpenPhoto, offset: u64, len: u64) -> Option<Turn<'_>> {
        let local = self
            .cache
            .layout_local(&file.link, &file.revision, file.size)
            .is_some_and(|layout| {
                layout.spans(offset, len).iter().all(|span| {
                    let key = BlockKey::new(&file.link, &file.revision, span.index);
                    self.cache.contains(&key) || self.cache.in_flight(&key)
                })
            });
        if local {
            return None;
        }
        self.scan.turn(self.config.timeouts.block.0 / 3).await
    }

    /// After a read of `len` bytes at `offset` through `handle`: counts
    /// them on the handle and, unless it was a scan read, takes the blocks
    /// it used out of the scan blocks (the engine's readahead bookkeeping,
    /// without readahead).
    fn after_read(&self, handle: u64, file: &OpenPhoto, layout: &Layout, offset: u64, len: u64) {
        let scan = {
            let mut files = self.files.lock();
            let Some(open) = files.get_mut(&handle) else {
                return;
            };
            let scan = self.is_scan_read(open);
            open.read_bytes = open.read_bytes.saturating_add(len);
            scan
        };
        if scan || !self.cache.scan_seat().is_some_and(|seat| seat.any_blocks()) {
            return;
        }
        let keys: Vec<BlockKey> = layout
            .spans(offset, len)
            .iter()
            .map(|span| BlockKey::new(&file.link, &file.revision, span.index))
            .collect();
        self.cache.scan_used(&keys);
    }

    fn file(&self, handle: u64) -> Result<OpenPhoto> {
        self.files
            .lock()
            .get(&handle)
            .cloned()
            .ok_or(CoreError::NotFound)
    }

    /// Drops the kernel's view of folder `ino` (pages and attributes).
    fn invalidate_dir(&self, ino: Ino) {
        let _ = self.invalidations.send(Invalidation::Inode { ino });
    }
}

/// A capture time as a file time.
fn time_of(capture: i64) -> SystemTime {
    if capture >= 0 {
        UNIX_EPOCH + Duration::from_secs(capture as u64)
    } else {
        UNIX_EPOCH
            .checked_sub(Duration::from_secs(capture.unsigned_abs()))
            .unwrap_or(UNIX_EPOCH)
    }
}

/// The digest of a listing the kernel is given (D072).
fn digest(entries: &[DirEntry]) -> u64 {
    let mut hasher = std::hash::DefaultHasher::new();
    for entry in entries {
        (entry.ino, entry.kind == NodeKind::Folder, &entry.name).hash(&mut hasher);
    }
    hasher.finish()
}

#[async_trait::async_trait]
impl ReadOnlyTree for PhotosTree {
    fn lookup(&self, parent: Ino, name: &[u8]) -> Result<Lookup> {
        if name.len() > MAX_NAME_BYTES {
            return Err(CoreError::NameTooLong);
        }
        let Ok(name) = std::str::from_utf8(name) else {
            return Ok(Lookup::Negative);
        };
        let mut model = self.model.write();
        let node = model.node(parent).cloned().ok_or(CoreError::NotFound)?;
        let found = match &node {
            Node::Root => parse_year(name)
                .filter(|year| model.years().contains(year))
                .map(Node::Year),
            Node::Year(year) => parse_month(name)
                .filter(|month| model.has_month((*year, *month)))
                .map(|month| Node::Month((*year, month))),
            Node::Month(month) => {
                let found = model.find(*month, name).cloned().map(Node::Photo);
                // A hidden name (`.hidden`, `.directory`, which file
                // managers look for) is answered at once.
                if found.is_none() && !model.complete(*month) && !name.starts_with('.') {
                    drop(model);
                    self.want(*month);
                    return Err(CoreError::NeedsListing(parent));
                }
                found
            }
            Node::Photo(_) => return Err(CoreError::NotADirectory),
        };
        match found {
            Some(node) => {
                let ino = model.ino(node);
                Ok(Lookup::Found(Self::attr_of(&model, ino)?))
            }
            // A year or month the timeline read has not reached yet.
            None if matches!(node, Node::Root | Node::Year(_))
                && !self.timeline_complete.load(Ordering::Acquire)
                && (parse_year(name).is_some() || parse_month(name).is_some()) =>
            {
                Err(CoreError::NeedsListing(parent))
            }
            None => Ok(Lookup::Negative),
        }
    }

    fn getattr(&self, ino: Ino) -> Result<Attr> {
        Self::attr_of(&self.model.read(), ino)
    }

    /// Opens a folder: D072's rule, as the engine's. The kernel keeps its
    /// pages only when the entries hash the same as those it was given at
    /// the folder's last open, so a month whose names arrive in stages is
    /// listed again at each open until they are all there. Opening a month
    /// asks for its names and tells the thumbnail hook.
    fn opendir(&self, ino: Ino) -> Result<(u64, bool)> {
        let (entries, month) = self.entries(ino)?;
        let digest = digest(&entries);
        if let Some(month) = month {
            let model = self.model.read();
            if !model.complete(month) {
                drop(model);
                self.want(month);
            } else if let Some(hook) = self.hook.read().clone() {
                hook.month_opened(month, Self::views(&model, month));
            }
        }
        let handle = self.next_handle.fetch_add(1, Ordering::Relaxed);
        let keep_cache = {
            let mut dirs = self.dirs.lock();
            let alone = !dirs.open.values().any(|(open, _)| *open == ino);
            dirs.open.insert(handle, (ino, Arc::new(entries)));
            if alone {
                dirs.served.insert(ino, digest) == Some(digest)
            } else {
                dirs.served.remove(&ino);
                false
            }
        };
        Ok((handle, keep_cache))
    }

    fn dir_snapshot(&self, handle: u64) -> Result<Arc<Vec<DirEntry>>> {
        self.dirs
            .lock()
            .open
            .get(&handle)
            .map(|(_, entries)| Arc::clone(entries))
            .ok_or(CoreError::NotFound)
    }

    fn releasedir(&self, handle: u64) {
        self.dirs.lock().open.remove(&handle);
    }

    fn open(&self, ino: Ino) -> Result<(u64, bool)> {
        let file = {
            let model = self.model.read();
            match model.node(ino) {
                Some(Node::Photo(link)) => {
                    let meta = model
                        .photo(link)
                        .and_then(|photo| photo.meta.as_ref())
                        .ok_or(CoreError::NotFound)?;
                    OpenPhoto {
                        link: link.clone(),
                        revision: meta.revision.clone(),
                        size: meta.size,
                        scan: false,
                        read_bytes: 0,
                    }
                }
                Some(_) => return Err(CoreError::IsADirectory),
                None => return Err(CoreError::NotFound),
            }
        };
        let keep_cache =
            self.served.lock().insert(ino, file.revision.clone()) == Some(file.revision.clone());
        // Every open is for reading alone and asked for by a program (the
        // FUSE layer refuses the others): each counts for the guard.
        let opened = self.scan.opened(self.scan_tree, ino);
        if opened.began {
            Metrics::inc(&self.metrics.scans);
            tracing::debug!(ino, "photos: many files opened at once: a scan begins");
        }
        let file = OpenPhoto {
            scan: opened.scan,
            ..file
        };
        let handle = self.next_handle.fetch_add(1, Ordering::Relaxed);
        self.files.lock().insert(handle, file);
        Ok((handle, keep_cache))
    }

    fn read_local(
        &self,
        handle: u64,
        offset: u64,
        size: u32,
    ) -> Result<Option<(ReadData, ReadFrom)>> {
        let file = self.file(handle)?;
        if offset >= file.size {
            return Ok(Some((ReadData::empty(), ReadFrom::Ram)));
        }
        let len = u64::from(size).min(file.size - offset);
        let Some(layout) = self
            .cache
            .layout_local(&file.link, &file.revision, file.size)
        else {
            return Ok(None);
        };
        let Some(read) = self.read_blocks_local(&file, &layout, offset, len) else {
            return Ok(None);
        };
        self.after_read(handle, &file, &layout, offset, len);
        Ok(Some(read))
    }

    /// Reads a range, downloading missing blocks. A scan read (D111) waits
    /// for its turn of the bulk-read guard and downloads at scan priority;
    /// any other read is one a user waits on, which holds back the
    /// session's background work and scan reads.
    async fn read(&self, handle: u64, offset: u64, size: u32) -> Result<ReadData> {
        let file = self.file(handle)?;
        if offset >= file.size {
            return Ok(ReadData::empty());
        }
        let len = u64::from(size).min(file.size - offset);
        let scan = self.is_scan_read(&file);
        let _wait = (!scan).then(|| self.user_wait());
        let (priority, _turn) = if scan {
            Metrics::inc(&self.metrics.scan_reads);
            (Priority::Scan, self.scan_turn(&file, offset, len).await)
        } else {
            (Priority::Interactive, None)
        };
        let layout = self
            .cache
            .layout(&file.link, &file.revision, file.size)
            .await?;
        let data = self
            .read_layout(&file, &layout, offset, len, priority)
            .await?;
        self.after_read(handle, &file, &layout, offset, len);
        Ok(data)
    }

    fn release(&self, handle: u64) {
        self.files.lock().remove(&handle);
    }

    /// For a month: fetches its next names batch as the user's work
    /// ([`PhotosTree::fetch_for_user`], D119). Otherwise waits for the next
    /// change of the model, unless folder `dir` has all its entries. The
    /// caller asks again, until its deadline.
    async fn resolve(&self, error: &CoreError) -> Result<()> {
        let CoreError::NeedsListing(dir) = error else {
            return Err(error.clone());
        };
        let mut progress = self.progress.subscribe();
        progress.borrow_and_update();
        if self.settled(*dir) {
            return Ok(());
        }
        let month = match self.model.read().node(*dir) {
            Some(Node::Month(month)) => Some(*month),
            _ => None,
        };
        if let Some(month) = month
            && self.fetch_for_user(month).await?
        {
            return Ok(());
        }
        progress
            .changed()
            .await
            .map_err(|_| CoreError::Internal("the Photos tree stopped".into()))
    }

    fn statfs(&self) -> StatFs {
        StatFs {
            blocks: 0,
            bfree: 0,
            bavail: 0,
            files: self.model.read().count() as u64,
            ffree: 0,
            bsize: 4096,
            namelen: MAX_NAME_BYTES as u32,
        }
    }

    fn metrics(&self) -> &Arc<Metrics> {
        &self.metrics
    }

    fn health(&self) -> &Arc<Health> {
        &self.health
    }

    fn config(&self) -> &Config {
        &self.config
    }

    fn runtime(&self) -> &tokio::runtime::Handle {
        &self.runtime
    }
}
