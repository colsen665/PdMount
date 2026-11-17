//! The [`BlockCache`] facade: RAM, disk and download behind one interface.

use super::*;

pub(super) const LAYOUT_CACHE_ENTRIES: usize = 4096;

/// How long the slow path waits for a pool slot before it allocates.
pub(super) const SLOT_WAIT: Duration = Duration::from_secs(5);

/// Block downloads in flight, all priorities together. One below the official
/// C# SDK's 6 block transfers per direction
/// (`cs/client/cs/src/Proton.Drive.Sdk/ProtonDriveClient.cs:28`); the JS SDK
/// allows 5 files with up to 10 blocks each (D033).
pub(super) const DOWNLOADS: usize = 5;

/// Per priority, within [`DOWNLOADS`]: readahead and prefetch together leave
/// interactive reads at least two slots.
pub(super) const INTERACTIVE_DOWNLOADS: usize = DOWNLOADS;

pub(super) const READAHEAD_DOWNLOADS: usize = 2;

pub(super) const PREFETCH_DOWNLOADS: usize = 1;

/// Scan reads (D111): one download at a time.
pub(super) const SCAN_DOWNLOADS: usize = 1;

/// The download permits of [`DOWNLOADS`] and its priority classes: one set
/// per session, shared by the caches of all its trees (D033, D110), so the
/// limits hold for the session, not per tree. A priority's permit is taken
/// first, then one of `all`.
pub struct DownloadSlots {
    pub(super) interactive: Arc<Semaphore>,
    pub(super) readahead: Arc<Semaphore>,
    pub(super) prefetch: Arc<Semaphore>,
    pub(super) scan: Arc<Semaphore>,
    pub(super) all: Arc<Semaphore>,
}

impl DownloadSlots {
    pub fn new() -> Self {
        Self {
            interactive: Arc::new(Semaphore::new(INTERACTIVE_DOWNLOADS)),
            readahead: Arc::new(Semaphore::new(READAHEAD_DOWNLOADS)),
            prefetch: Arc::new(Semaphore::new(PREFETCH_DOWNLOADS)),
            scan: Arc::new(Semaphore::new(SCAN_DOWNLOADS)),
            all: Arc::new(Semaphore::new(DOWNLOADS)),
        }
    }

    fn of(&self, priority: Priority) -> &Arc<Semaphore> {
        match priority {
            Priority::Interactive => &self.interactive,
            Priority::Readahead => &self.readahead,
            Priority::Prefetch => &self.prefetch,
            Priority::Scan => &self.scan,
        }
    }

    /// The permit of `priority`'s class. A scan download waits for its
    /// class only until `raised` says a read a user waits on joined it; it
    /// then waits for an interactive permit instead (D120). Answers with
    /// the priority the download goes at.
    async fn class(
        &self,
        priority: Priority,
        raised: Option<&tokio::sync::Notify>,
    ) -> Result<(OwnedSemaphorePermit, Priority)> {
        let acquire = |priority| async move {
            Arc::clone(self.of(priority))
                .acquire_owned()
                .await
                .map(|permit| (permit, priority))
                .map_err(|_| CoreError::Internal("download permits closed".into()))
        };
        match raised {
            Some(raised) if priority == Priority::Scan => {
                tokio::select! {
                    biased;
                    () = raised.notified() => acquire(Priority::Interactive).await,
                    permit = acquire(Priority::Scan) => permit,
                }
            }
            _ => acquire(priority).await,
        }
    }

    /// A download slot of `priority`: its class's permit, then one of all.
    /// For downloads outside the block cache (Photos' thumbnails, D120).
    pub async fn slot(&self, priority: Priority) -> Result<DownloadSlot> {
        let (class, _) = self.class(priority, None).await?;
        let all = self.all().await?;
        Ok(DownloadSlot {
            _class: class,
            _all: all,
        })
    }

    async fn all(&self) -> Result<OwnedSemaphorePermit> {
        Arc::clone(&self.all)
            .acquire_owned()
            .await
            .map_err(|_| CoreError::Internal("download permits closed".into()))
    }
}

/// A download in flight, counted by [`DownloadSlots`] until dropped.
pub struct DownloadSlot {
    _class: OwnedSemaphorePermit,
    _all: OwnedSemaphorePermit,
}

impl Default for DownloadSlots {
    fn default() -> Self {
        Self::new()
    }
}

pub struct CacheOptions {
    pub dir: PathBuf,
    /// `None`: the disk cache stays closed until [`BlockCache::unlock`]
    /// (a start before the keyring answers, D106).
    pub master_key: Option<[u8; 32]>,
    pub ram_bytes: u64,
    /// The disk cap of a cache alone; with `shared`, the budget's applies.
    pub disk_bytes: u64,
    /// The profile's cap shared with its other trees (D110 amended).
    pub shared: Option<SharedCap>,
    /// The session's download permits (D033, D110); `Engine::open` sets
    /// them from `EngineOptions::limits`. `None`: the cache has its own.
    pub downloads: Option<Arc<DownloadSlots>>,
}

pub struct BlockCache {
    pub(super) backend: Arc<dyn Backend>,
    pub(super) store: Arc<Store>,
    pub(super) metrics: Arc<Metrics>,
    pub(super) pool: Arc<Pool>,
    pub(super) ram: parking_lot::Mutex<RamCache>,
    pub(super) disk: Arc<DiskCache>,
    /// On the profile's [`DiskBudget`], or on one of its own.
    pub(super) seat: Seat,
    pub(super) layouts: parking_lot::Mutex<lru::LruCache<(LinkId, RevisionId), Arc<Layout>>>,
    pub(super) layout_flights: SingleFlight<(LinkId, RevisionId), Arc<Layout>>,
    pub(super) block_flights: SingleFlight<BlockKey, Bytes>,
    /// The session's download permits, or the cache's own.
    pub(super) downloads: Arc<DownloadSlots>,
    /// Scan flights still waiting for their download permit, each with the
    /// signal that raises it to interactive priority (D120). A leaf lock,
    /// taken in a flight's start under the flights' registry lock.
    pub(super) raises: parking_lot::Mutex<HashMap<BlockKey, Arc<tokio::sync::Notify>>>,
}

impl BlockCache {
    /// Fails if `ram_bytes` is too small for the pool ([`Pool::new`]).
    pub fn open(
        backend: Arc<dyn Backend>,
        store: Arc<Store>,
        metrics: Arc<Metrics>,
        options: CacheOptions,
        protections: Protections,
    ) -> Result<Self> {
        let pool = Pool::new(options.ram_bytes)?;
        Self::with_pool(backend, store, metrics, options, protections, pool)
    }

    pub(super) fn with_pool(
        backend: Arc<dyn Backend>,
        store: Arc<Store>,
        metrics: Arc<Metrics>,
        options: CacheOptions,
        protections: Protections,
        pool: Pool,
    ) -> Result<Self> {
        let disk = Arc::new(DiskCache::open(
            &options.dir,
            options.master_key.as_ref(),
            &store,
            protections,
        )?);
        let SharedCap { budget, keeps_half } = options.shared.unwrap_or_else(|| SharedCap {
            budget: DiskBudget::new(options.disk_bytes),
            keeps_half: true,
        });
        let evict = {
            // Weak: the budget never keeps a closed cache or its store.
            let disk = Arc::downgrade(&disk);
            let store = Arc::downgrade(&store);
            let metrics = Arc::clone(&metrics);
            Box::new(
                move |target, floor| match (disk.upgrade(), store.upgrade()) {
                    (Some(disk), Some(store)) => disk.evict_to(target, floor, &store, &metrics),
                    _ => Evicted::default(),
                },
            )
        };
        let bytes = Arc::clone(&disk.index.lock().shown);
        let seat = budget.join(keeps_half, bytes, evict);
        let cache = Self {
            backend,
            store,
            metrics,
            pool: Arc::new(pool),
            ram: parking_lot::Mutex::new(RamCache {
                entries: lru::LruCache::unbounded(),
                bytes: 0,
            }),
            disk,
            layouts: parking_lot::Mutex::new(lru::LruCache::new(
                NonZeroUsize::new(LAYOUT_CACHE_ENTRIES).expect("non-zero"),
            )),
            layout_flights: SingleFlight::default(),
            block_flights: SingleFlight::default(),
            downloads: options.downloads.unwrap_or_default(),
            raises: parking_lot::Mutex::default(),
            seat,
        };
        // The cap may have been lowered since the last mount, or this
        // tree's blocks take the profile over it.
        cache.enforce_cap();
        Ok(cache)
    }

    /// Above 90 % of the cap, evicts down to 80 % (§6.2): with other trees
    /// on the cap, by [`DiskBudget::enforce`]'s rule. Called with no lock
    /// held, after this cache's bytes grew.
    pub(super) fn enforce_cap(&self) {
        self.seat.budget.enforce(self.seat.id);
    }

    /// [`DiskCache::put`], then the cap.
    pub(super) fn put_disk(&self, key: &BlockKey, sealed: &[u8]) -> Result<()> {
        let put = self.disk.put(key, sealed, &self.store);
        if put.is_ok() {
            self.enforce_cap();
        }
        put
    }

    /// Layout from RAM or the store; never touches the network.
    pub fn layout_local(&self, link: &LinkId, rev: &RevisionId, size: u64) -> Option<Arc<Layout>> {
        let key = (link.clone(), rev.clone());
        if let Some(layout) = self.layouts.lock().get(&key) {
            return Some(Arc::clone(layout));
        }
        let infos = self
            .store
            .read(|conn| {
                let mut statement = conn.prepare_cached(
                    "SELECT idx, plain_size FROM revision_blocks WHERE link_id = ?1 AND rev = ?2 ORDER BY idx",
                )?;
                let rows = statement
                    .query_map((&link.0, &rev.0), |row| {
                        Ok(BlockInfo {
                            index: row.get(0)?,
                            plain_size: row.get(1)?,
                        })
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .ok()?;
        // An empty file has no blocks, so an empty result is ambiguous; only
        // a non-empty one counts as stored.
        if infos.is_empty() {
            return None;
        }
        let layout = Arc::new(Layout::new(&infos, size).ok()?);
        self.layouts.lock().put(key, Arc::clone(&layout));
        Some(layout)
    }

    /// Layout, fetching the block list if needed.
    pub async fn layout(&self, link: &LinkId, rev: &RevisionId, size: u64) -> Result<Arc<Layout>> {
        if size == 0 {
            return Ok(Arc::new(Layout { blocks: Vec::new() }));
        }
        if let Some(layout) = self.layout_local(link, rev, size) {
            return Ok(layout);
        }
        let backend = Arc::clone(&self.backend);
        let store = Arc::clone(&self.store);
        let metrics = Arc::clone(&self.metrics);
        let (link_owned, rev_owned) = (link.clone(), rev.clone());
        let layout = self
            .layout_flights
            .run((link.clone(), rev.clone()), move || async move {
                check_not_hot(&metrics, "blocks");
                let infos = backend.blocks(&link_owned, &rev_owned).await?;
                let layout = Arc::new(Layout::new(&infos, size)?);
                store_layout(&store, &link_owned, &rev_owned, &layout);
                Ok(layout)
            })
            .await?;
        self.layouts
            .lock()
            .put((link.clone(), rev.clone()), Arc::clone(&layout));
        Ok(layout)
    }

    /// A block from RAM or disk; never touches the network or waits for a
    /// pool slot.
    pub fn get_local(&self, key: &BlockKey) -> Option<Bytes> {
        self.get_local_from(key).map(|(data, _)| data)
    }

    /// [`BlockCache::get_local`], with where the block was found.
    pub fn get_local_from(&self, key: &BlockKey) -> Option<(Bytes, ReadFrom)> {
        let hit = self.ram.lock().get(key);
        if let Some(data) = hit {
            Metrics::inc(&self.metrics.cache_ram_hits);
            // A hot block must not go first from the disk.
            self.disk.touch(key);
            return Some((data, ReadFrom::Ram));
        }
        let (buffer, len) =
            self.disk
                .read(key, |len| self.buffer_now(len), &self.store, &self.metrics)?;
        Metrics::inc(&self.metrics.cache_disk_hits);
        Some((self.keep(key, buffer, len), ReadFrom::Disk))
    }

    /// A slot for a block of `len` bytes: a free one, else one freed by
    /// evicting RAM-cache entries of its class. `None` if every slot of the
    /// class is still referenced.
    pub(super) fn take_slot(&self, len: usize) -> Option<SlotMut> {
        let class = self.pool.class_for(len)?;
        loop {
            if let Some(slot) = self.pool.try_take(class) {
                return Some(slot);
            }
            // Dropped outside the lock: that may free the slot.
            let evicted = self.ram.lock().evict(class)?;
            drop(evicted);
        }
    }

    pub(super) fn fallback(&self, len: usize) -> Buffer {
        Metrics::inc(&self.metrics.cache_pool_fallbacks);
        Buffer::heap(len)
    }

    /// A buffer for the hot path: a slot if one can be had now, else the heap.
    pub(super) fn buffer_now(&self, len: usize) -> Buffer {
        match self.take_slot(len) {
            Some(slot) => Buffer::Slot(slot),
            None => self.fallback(len),
        }
    }

    /// A buffer for the slow path: waits up to [`SLOT_WAIT`] for a slot,
    /// then takes the heap.
    pub(super) async fn buffer(&self, len: usize) -> Buffer {
        if let Some(class) = self.pool.class_for(len)
            && let Some(slot) = self.wait_for(class, || self.take_slot(len)).await
        {
            return Buffer::Slot(slot);
        }
        self.fallback(len)
    }

    /// Retries `take` whenever a slot of `class` is freed or becomes
    /// evictable, for up to [`SLOT_WAIT`].
    pub(super) async fn wait_for(
        &self,
        class: usize,
        take: impl Fn() -> Option<SlotMut>,
    ) -> Option<SlotMut> {
        let deadline = tokio::time::Instant::now() + SLOT_WAIT;
        loop {
            if let Some(slot) = take() {
                return Some(slot);
            }
            tokio::time::timeout_at(deadline, self.pool.changed(class))
                .await
                .ok()?;
        }
    }

    /// A buffer to assemble a read across blocks in on the hot path: a
    /// scratch slot if one is free, else the heap.
    pub fn read_buf_now(&self, offset: u64, len: u64) -> ReadBuf {
        let buffer = if len as usize > SCRATCH_CAPACITY {
            Buffer::heap(len as usize)
        } else if let Some(slot) = self.pool.try_take(SCRATCH) {
            Buffer::Slot(slot)
        } else {
            self.fallback(len as usize)
        };
        ReadBuf::new(buffer, offset, len)
    }

    /// [`BlockCache::read_buf_now`] for the slow path: waits up to
    /// [`SLOT_WAIT`] for a scratch slot. Reads larger than a scratch slot
    /// (materialization) take the heap.
    pub async fn read_buf(&self, offset: u64, len: u64) -> ReadBuf {
        let buffer = if len as usize > SCRATCH_CAPACITY {
            Buffer::heap(len as usize)
        } else if let Some(slot) = self.wait_for(SCRATCH, || self.pool.try_take(SCRATCH)).await {
            Buffer::Slot(slot)
        } else {
            self.fallback(len as usize)
        };
        ReadBuf::new(buffer, offset, len)
    }

    /// The block's plaintext; a slot also goes into the RAM cache (a heap
    /// fallback does not).
    pub(super) fn keep(&self, key: &BlockKey, buffer: Buffer, len: usize) -> Bytes {
        let class = buffer.class();
        let data = buffer.freeze(len);
        if let Some(class) = class {
            let cached = Cached {
                data: data.clone(),
                class,
                slot_size: self.pool.slot_size(class),
            };
            let replaced = self.ram.lock().put(key.clone(), cached);
            drop(replaced);
            // Now evictable by a waiter.
            self.pool.wake(class);
        }
        data
    }

    /// Opens the disk cache with the key that came after the start (D106);
    /// `false` when it had one already.
    pub fn unlock(&self, master_key: &[u8; 32]) -> bool {
        self.disk.unlock(master_key)
    }

    /// Whether the disk cache has its key.
    pub fn unlocked(&self) -> bool {
        self.disk.unlocked()
    }

    /// Seals a downloaded block (plaintext at [`PLAIN_START`]) into the disk
    /// cache, opens it again in place and keeps it. Without the key it only
    /// goes into RAM.
    pub(super) fn store_download(
        &self,
        key: &BlockKey,
        mut buffer: Buffer,
        len: usize,
    ) -> Result<Bytes> {
        if !self.disk.unlocked() {
            return Ok(self.keep(key, buffer, len));
        }
        let sealed = &mut buffer.as_mut()[..len + SEAL_OVERHEAD];
        self.disk.seal(key, sealed)?;
        if let Err(error) = self.put_disk(key, sealed) {
            tracing::warn!(%error, "could not write a block to the disk cache");
        }
        if !self.disk.unseal(key, sealed) {
            return Err(CoreError::Internal(
                "a sealed block did not open again".into(),
            ));
        }
        Ok(self.keep(key, buffer, len))
    }

    /// Slots in use per pool class.
    pub fn pool_usage(&self) -> Vec<ClassUsage> {
        self.pool.usage()
    }

    pub fn contains(&self, key: &BlockKey) -> bool {
        self.ram.lock().entries.contains(key)
            || (self.disk.unlocked() && self.disk.index.lock().entries.contains_key(key))
    }

    /// A block from the cache or the backend. `expected_len` is checked
    /// against the downloaded plaintext.
    pub async fn get(
        self: &Arc<Self>,
        key: &BlockKey,
        expected_len: u32,
        priority: Priority,
    ) -> Result<Bytes> {
        if let Some(data) = self.get_local(key) {
            return Ok(data);
        }
        // Layouts never hold larger blocks; nothing else sizes a buffer.
        if expected_len as usize > pdmount_backend::MAX_BLOCK_BYTES {
            return Err(pdmount_backend::BackendError::Integrity(format!(
                "block {} of {} declares {expected_len} bytes",
                key.index, key.rev
            ))
            .into());
        }
        let this = Arc::clone(self);
        let owned = key.clone();
        let flight = self.block_flights.run(key.clone(), move || {
            // Under the flights' lock, so a caller that joins this flight
            // finds the signal (D120).
            let raised = (priority == Priority::Scan).then(|| {
                let raised = Arc::new(tokio::sync::Notify::new());
                this.raises
                    .lock()
                    .insert(owned.clone(), Arc::clone(&raised));
                raised
            });
            async move {
                let class = this.downloads.class(priority, raised.as_deref()).await;
                if raised.is_some() {
                    this.raises.lock().remove(&owned);
                }
                let (_permit, priority) = class?;
                let _download = this.downloads.all().await?;
                // Another flight may have filled it while this one waited.
                if let Some(data) = this.get_local(&owned) {
                    return Ok(data);
                }
                this.download(&owned, expected_len, priority).await
            }
        });
        // A read a user waits on that joined a scan flight still waiting for
        // the scan permit: the flight goes at interactive priority instead of
        // behind the scan queue (D120).
        if priority == Priority::Interactive
            && let Some(raised) = self.raises.lock().get(key)
        {
            raised.notify_one();
        }
        flight.await
    }

    /// Downloads a block into the cache (its permits held by the caller).
    /// A block a scan read downloaded counts against the scan share; one
    /// any other read downloaded is part of the working set, also if a scan
    /// had brought it in before (D111, D120).
    async fn download(
        &self,
        key: &BlockKey,
        expected_len: u32,
        priority: Priority,
    ) -> Result<Bytes> {
        check_not_hot(&self.metrics, "fetch_block");
        Metrics::inc(&self.metrics.cache_misses);
        if matches!(priority, Priority::Readahead | Priority::Prefetch) {
            Metrics::inc(&self.metrics.prefetched_blocks);
        }
        let expected = expected_len as usize;
        let mut buffer = self.buffer(expected).await;
        let plain = &mut buffer.as_mut()[PLAIN_START..PLAIN_START + expected];
        let len = match self
            .backend
            .fetch_block_into(&key.link, &key.rev, key.index, plain)
            .await
        {
            Ok(len) => len,
            Err(error) => {
                if matches!(error, pdmount_backend::BackendError::Integrity(_)) {
                    tracing::error!(link = %key.link, rev = %key.rev, index = key.index, %error,
                        "block failed verification; not cached");
                }
                return Err(error.into());
            }
        };
        if len != expected {
            let error = format!(
                "block {} of {} has {len} bytes, expected {expected_len}",
                key.index, key.rev
            );
            tracing::error!("{error}; not cached");
            return Err(pdmount_backend::BackendError::Integrity(error).into());
        }
        let data = self.store_download(key, buffer, len)?;
        if priority == Priority::Scan {
            self.scan_stored(key);
        } else {
            self.scan_used(std::iter::once(key));
        }
        Ok(data)
    }

    /// Joins the session's bulk-read guard (once; later calls are ignored).
    pub fn set_scan_seat(&self, seat: crate::scan::ScanSeat) {
        let _ = self.disk.scan_seat.set(seat);
    }

    /// The tree's place on the bulk-read guard, once set.
    pub fn scan_seat(&self) -> Option<&crate::scan::ScanSeat> {
        self.disk.scan_seat.get()
    }

    /// A scan read stored `key` on disk: it counts against the scan share,
    /// unless it is pinned.
    fn scan_stored(&self, key: &BlockKey) {
        let Some(seat) = self.scan_seat() else {
            return;
        };
        let bytes = self
            .disk
            .index
            .lock()
            .entries
            .get(key)
            .filter(|entry| !entry.pinned)
            .map(|entry| entry.bytes);
        if let Some(bytes) = bytes {
            seat.stored(key, bytes);
        }
    }

    /// An interactive read used these blocks: those a scan brought in are
    /// part of the working set now.
    pub fn scan_used<'a>(&self, keys: impl IntoIterator<Item = &'a BlockKey>) {
        if let Some(seat) = self.scan_seat().filter(|seat| seat.any_blocks()) {
            seat.forget(keys);
        }
    }

    /// Evicts one block from the disk for the scan share, unless it is
    /// pinned or protected.
    pub fn evict_scan_block(&self, key: &BlockKey) {
        let evicted = self.disk.evict_one(key, &self.store, &self.metrics);
        if evicted.blocks > 0 {
            Metrics::inc(&self.metrics.scan_evictions);
        }
    }

    /// A slot of the download permits this cache counts against (the
    /// session's), for downloads outside it.
    pub async fn download_slot(&self, priority: Priority) -> Result<DownloadSlot> {
        self.downloads.slot(priority).await
    }

    /// Whether a download of `key` runs.
    pub fn in_flight(&self, key: &BlockKey) -> bool {
        self.block_flights.running(key)
    }

    pub fn flush_access(&self) {
        self.disk.flush_access(&self.store);
    }

    /// Inserts a just-uploaded revision from its local snapshot, split into
    /// `block_size` blocks (§6.4), so reading it back downloads nothing.
    /// Each block is sealed in a pool slot; none goes into the RAM cache.
    pub async fn seed(
        &self,
        link: &LinkId,
        rev: &RevisionId,
        snapshot: &Path,
        block_size: u64,
    ) -> Result<()> {
        use std::io::Read;
        // Without the key nothing can be sealed; a read downloads it later.
        if !self.disk.unlocked() {
            return Ok(());
        }
        let mut file = std::fs::File::open(snapshot)?;
        let size = file.metadata()?.len();
        let mut infos = Vec::new();
        let mut index = 1u32;
        let mut remaining = size;
        while remaining > 0 {
            let len = remaining.min(block_size) as usize;
            // No wait for readers: the upload's commit waits on this.
            let mut buffer = self.buffer_now(len);
            let sealed = &mut buffer.as_mut()[..len + SEAL_OVERHEAD];
            file.read_exact(&mut sealed[PLAIN_START..PLAIN_START + len])?;
            let key = BlockKey::new(link, rev, index);
            self.disk.seal(&key, sealed)?;
            self.put_disk(&key, sealed)?;
            infos.push(BlockInfo {
                index,
                plain_size: Some(len as u32),
            });
            remaining -= len as u64;
            index += 1;
        }
        let layout = Arc::new(Layout::new(&infos, size)?);
        store_layout(&self.store, link, rev, &layout);
        self.layouts.lock().put((link.clone(), rev.clone()), layout);
        Ok(())
    }

    /// Unpinned disk bytes and entry count.
    pub fn usage(&self) -> (u64, usize) {
        self.disk.usage()
    }

    pub fn disk_stats(&self) -> DiskStats {
        self.disk.stats(&self.seat.budget)
    }

    /// `cache gc`: evicts this cache's blocks down to `target` bytes.
    pub fn gc(&self, target: u64) -> Evicted {
        self.disk.evict_to(target, 0, &self.store, &self.metrics)
    }

    /// `cache gc` without a target: what takes the profile's total down to
    /// the lower watermark, 80 % of the cap, with this cache's blocks only.
    /// Alone, the watermark itself.
    pub fn gc_default_target(&self) -> u64 {
        let stats = self.disk_stats();
        let excess = stats.profile_bytes.saturating_sub(stats.max_bytes / 10 * 8);
        stats.bytes.saturating_sub(excess)
    }

    /// `cache wipe`: evicts everything allowed, or with `links` only the
    /// blocks of those files, from the disk and the RAM cache.
    pub fn wipe(&self, links: Option<&HashSet<LinkId>>) -> Evicted {
        let wiped = self.disk.wipe(links, &self.store, &self.metrics);
        let held: Vec<Bytes> = {
            let mut ram = self.ram.lock();
            let keys: Vec<BlockKey> = ram
                .entries
                .iter()
                .map(|(key, _)| key)
                .filter(|key| links.is_none_or(|links| links.contains(&key.link)))
                .filter(|key| !self.disk.index.lock().entries.contains_key(*key))
                .cloned()
                .collect();
            keys.iter().filter_map(|key| ram.remove(key)).collect()
        };
        // Dropped outside the lock: that may free slots.
        drop(held);
        wiped
    }

    /// Makes `wanted` the pinned revisions; those let go may be evicted at
    /// once, so this can remove files.
    pub fn set_pinned_revisions(&self, wanted: &HashSet<(LinkId, RevisionId)>) {
        let (add, drop): (Vec<_>, Vec<_>) = {
            let index = self.disk.index.lock();
            (
                wanted.difference(&index.pinned_revs).cloned().collect(),
                index.pinned_revs.difference(wanted).cloned().collect(),
            )
        };
        for (link, rev) in &add {
            self.disk.set_pinned(link, rev, true, &self.store);
        }
        for (link, rev) in &drop {
            self.disk.set_pinned(link, rev, false, &self.store);
        }
        if !drop.is_empty() {
            self.enforce_cap();
        }
    }

    pub fn on_disk(&self, key: &BlockKey) -> bool {
        self.disk.index.lock().entries.contains_key(key)
    }

    /// Plaintext bytes of `layout` that are in the disk cache.
    pub fn bytes_on_disk(&self, link: &LinkId, rev: &RevisionId, layout: &Layout) -> u64 {
        let index = self.disk.index.lock();
        layout
            .blocks
            .iter()
            .filter(|span| {
                index
                    .entries
                    .contains_key(&BlockKey::new(link, rev, span.index))
            })
            .map(|span| u64::from(span.len))
            .sum()
    }

    /// Where the blocks are, for the free space check.
    pub fn dir(&self) -> &Path {
        &self.disk.dir
    }

    /// Removes files without a row and rows without a file (once per mount).
    pub fn sweep(&self) -> Result<Swept> {
        self.disk.sweep(&self.store)
    }
}
