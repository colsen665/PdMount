//! Block cache (DESIGN.md §6).
//!
//! Content is cached per block of an immutable revision, so entries are only
//! ever evicted, never invalidated. Lookups go RAM LRU → encrypted disk cache
//! → download. Downloads of the same block are shared ([`SingleFlight`]) and
//! limited per priority class. Block buffers come from the [`Pool`] reserved
//! at startup (D049).

use crate::errors::{CoreError, Result};
use crate::metrics::{Metrics, check_not_hot};
use crate::pool::{
    Buffer, ClassUsage, NONCE_LEN, PLAIN_START, Pool, SCRATCH, SCRATCH_CAPACITY, SEAL_OVERHEAD,
    SlotMut, TAG_LEN,
};
use crate::singleflight::SingleFlight;
use crate::store::Store;
use bytes::Bytes;
use chacha20poly1305::aead::{AeadCore, AeadInPlace, KeyInit, OsRng};
use chacha20poly1305::{Tag, XChaCha20Poly1305, XNonce};
use pdmount_backend::{Backend, BlockInfo, LinkId, RevisionId};
use std::collections::HashMap;
use std::io::Write;
use std::num::NonZeroUsize;
use std::ops::Range;
use std::os::unix::fs::{DirBuilderExt, FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Semaphore;

/// Proton's plaintext block size (`sdk/node/revision.rs:34`).
pub const BLOCK_SIZE: u64 = 4 * 1024 * 1024;
const LAYOUT_CACHE_ENTRIES: usize = 4096;
/// How long the slow path waits for a pool slot before it allocates.
const SLOT_WAIT: Duration = Duration::from_secs(5);
/// Block downloads in flight, all priorities together. One below the official
/// C# SDK's 6 block transfers per direction
/// (`cs/client/cs/src/Proton.Drive.Sdk/ProtonDriveClient.cs:28`); the JS SDK
/// allows 5 files with up to 10 blocks each (D033).
const DOWNLOADS: usize = 5;
/// Per priority, within [`DOWNLOADS`]: readahead and prefetch together leave
/// interactive reads at least two slots.
const INTERACTIVE_DOWNLOADS: usize = DOWNLOADS;
const READAHEAD_DOWNLOADS: usize = 2;
const PREFETCH_DOWNLOADS: usize = 1;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BlockKey {
    pub link: LinkId,
    pub rev: RevisionId,
    /// 1-based, as on the server.
    pub index: u32,
}

impl BlockKey {
    pub fn new(link: &LinkId, rev: &RevisionId, index: u32) -> Self {
        Self {
            link: link.clone(),
            rev: rev.clone(),
            index,
        }
    }

    /// Bytes used as file name source and AEAD associated data.
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.link.0.len() + self.rev.0.len() + 6);
        out.extend_from_slice(self.link.0.as_bytes());
        out.push(0);
        out.extend_from_slice(self.rev.0.as_bytes());
        out.push(0);
        out.extend_from_slice(&self.index.to_le_bytes());
        out
    }
}

/// Priority classes with separate download permits (§6.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    Interactive,
    Readahead,
    Prefetch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockSpan {
    pub index: u32,
    pub offset: u64,
    pub len: u32,
}

/// Queues the rows of a revision's layout (`revision_blocks`).
fn store_layout(store: &Store, link: &LinkId, rev: &RevisionId, layout: &Layout) {
    let rows: Vec<(u32, u32)> = layout.blocks.iter().map(|b| (b.index, b.len)).collect();
    let (link, rev) = (link.0.clone(), rev.0.clone());
    store.write(move |conn| {
        let mut statement = conn.prepare(
            "INSERT OR REPLACE INTO revision_blocks(link_id, rev, idx, plain_size) VALUES (?1, ?2, ?3, ?4)",
        )?;
        for (index, len) in &rows {
            statement.execute((&link, &rev, index, len))?;
        }
        Ok(())
    });
}

/// Where each block of a revision sits in the file (§6.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub blocks: Vec<BlockSpan>,
}

impl Layout {
    /// Uses the declared plaintext sizes when every block has one; otherwise
    /// assumes 4 MiB blocks and checks that they add up to `file_size`. No
    /// blocks at all take the second path, so they hold only an empty file.
    pub fn new(infos: &[BlockInfo], file_size: u64) -> Result<Self> {
        let mut blocks = Vec::with_capacity(infos.len());
        let mut offset = 0u64;
        if !infos.is_empty() && infos.iter().all(|info| info.plain_size.is_some()) {
            for info in infos {
                let len = info.plain_size.unwrap_or(0);
                blocks.push(BlockSpan {
                    index: info.index,
                    offset,
                    len,
                });
                offset += u64::from(len);
            }
            if offset != file_size {
                tracing::warn!(
                    declared = offset,
                    file_size,
                    "block sizes do not add up to the file size"
                );
            }
            return Ok(Self { blocks });
        }
        let count = infos.len() as u64;
        let full = count.saturating_sub(1) * BLOCK_SIZE;
        let last = file_size.checked_sub(full).filter(|&last| {
            (count == 0 && last == 0) || (count > 0 && last > 0 && last <= BLOCK_SIZE)
        });
        let Some(last) = last else {
            return Err(CoreError::Backend(
                pdmount_backend::BackendError::Integrity(format!(
                    "{count} blocks of at most 4 MiB cannot hold {file_size} bytes"
                )),
            ));
        };
        for (position, info) in infos.iter().enumerate() {
            let len = if position + 1 == infos.len() {
                last
            } else {
                BLOCK_SIZE
            };
            blocks.push(BlockSpan {
                index: info.index,
                offset,
                len: len as u32,
            });
            offset += len;
        }
        Ok(Self { blocks })
    }

    pub fn size(&self) -> u64 {
        self.blocks
            .last()
            .map_or(0, |b| b.offset + u64::from(b.len))
    }

    /// Blocks overlapping `[offset, offset + len)`.
    pub fn spans(&self, offset: u64, len: u64) -> &[BlockSpan] {
        let end = offset.saturating_add(len);
        let first = self
            .blocks
            .partition_point(|b| b.offset + u64::from(b.len) <= offset);
        let last = self.blocks.partition_point(|b| b.offset < end);
        &self.blocks[first..last.max(first)]
    }

    /// The block after `index`, if any.
    pub fn next_after(&self, index: u32) -> Option<&BlockSpan> {
        let position = self.blocks.iter().position(|b| b.index == index)?;
        self.blocks.get(position + 1)
    }
}

/// Blocks a read holds at once: readers copy each block out as it arrives
/// and drop it, so a read over many small blocks does not pin a pool class
/// (§6.2). Two, so a read across a block boundary fetches both together.
pub const READ_BLOCKS_HELD: usize = 2;

/// The part of block `span` (`data_len` bytes) inside `[offset, offset +
/// len)`, relative to the block.
fn span_range(span: &BlockSpan, data_len: usize, offset: u64, len: u64) -> Range<usize> {
    let end = offset + len;
    let start = offset.max(span.offset) - span.offset;
    let stop = (end.min(span.offset + u64::from(span.len)) - span.offset).min(data_len as u64);
    start as usize..stop.max(start) as usize
}

/// The bytes of a read (§6.2): a slice of a pool slot (the one block the
/// read lies in, or the scratch slot a read across blocks was assembled
/// in), or a heap buffer (staging, fallbacks).
#[derive(Debug, Clone)]
pub enum ReadData {
    Slice(Bytes),
    Owned(Vec<u8>),
}

impl ReadData {
    pub fn empty() -> Self {
        Self::Owned(Vec::new())
    }

    /// `[offset, offset + len)` of the one block `span` it lies in, without
    /// copying.
    pub fn of_block(span: &BlockSpan, data: Bytes, offset: u64, len: u64) -> Self {
        Self::Slice(data.slice(span_range(span, data.len(), offset, len)))
    }

    pub fn into_vec(self) -> Vec<u8> {
        match self {
            Self::Slice(data) => data.to_vec(),
            Self::Owned(data) => data,
        }
    }
}

impl std::ops::Deref for ReadData {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match self {
            Self::Slice(data) => data,
            Self::Owned(data) => data,
        }
    }
}

impl<T: AsRef<[u8]> + ?Sized> PartialEq<T> for ReadData {
    fn eq(&self, other: &T) -> bool {
        **self == *other.as_ref()
    }
}

/// A read across blocks, assembled in a scratch slot (or a heap buffer)
/// one block at a time.
pub struct ReadBuf {
    buffer: Buffer,
    filled: usize,
    offset: u64,
    len: u64,
}

impl ReadBuf {
    fn new(buffer: Buffer, offset: u64, len: u64) -> Self {
        Self {
            buffer,
            filled: 0,
            offset,
            len,
        }
    }

    /// Appends the part of block `span` (`data`) inside the read. Blocks
    /// come in order.
    pub fn append(&mut self, span: &BlockSpan, data: &[u8]) {
        let range = span_range(span, data.len(), self.offset, self.len);
        let start = PLAIN_START + self.filled;
        self.buffer.as_mut()[start..start + range.len()].copy_from_slice(&data[range.clone()]);
        self.filled += range.len();
    }

    pub fn finish(self) -> ReadData {
        ReadData::Slice(self.buffer.freeze(self.filled))
    }
}

struct Cached {
    /// The plaintext, inside a pool slot.
    data: Bytes,
    class: usize,
    slot_size: usize,
}

/// Blocks in pool slots, least recently used first out. Its size is bounded
/// by the pool, so it never evicts on its own: taking a slot does.
struct RamCache {
    entries: lru::LruCache<BlockKey, Cached>,
    /// Slot sizes of the entries, not block lengths.
    bytes: u64,
}

impl RamCache {
    fn get(&mut self, key: &BlockKey) -> Option<Bytes> {
        self.entries.get(key).map(|cached| cached.data.clone())
    }

    /// Returns a replaced entry, to be dropped outside the lock.
    fn put(&mut self, key: BlockKey, cached: Cached) -> Option<Bytes> {
        self.bytes += cached.slot_size as u64;
        let old = self.entries.put(key, cached)?;
        self.bytes -= old.slot_size as u64;
        Some(old.data)
    }

    /// Removes the least recently used entry of `class`. Its slot is free
    /// once the returned `Bytes` and every clone of them are dropped.
    fn evict(&mut self, class: usize) -> Option<Bytes> {
        let key = self
            .entries
            .iter()
            .rev()
            .find(|(_, cached)| cached.class == class)?
            .0
            .clone();
        let cached = self.entries.pop(&key)?;
        self.bytes -= cached.slot_size as u64;
        Some(cached.data)
    }
}

#[derive(Debug, Clone, Copy)]
struct DiskEntry {
    bytes: u64,
    last_access: i64,
}

struct DiskIndex {
    entries: HashMap<BlockKey, DiskEntry>,
    bytes: u64,
    /// Entries whose `last_access` changed since the last flush.
    touched: Vec<BlockKey>,
}

/// Encrypted on-disk block cache: `blocks/xx/<blake3(key)>` holding
/// nonce ‖ XChaCha20-Poly1305(block), with the key as associated data.
struct DiskCache {
    dir: PathBuf,
    cipher: XChaCha20Poly1305,
    index: parking_lot::Mutex<DiskIndex>,
    limit: u64,
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

impl DiskCache {
    fn open(dir: &Path, master_key: &[u8; 32], limit: u64, store: &Store) -> Result<Self> {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        let key = blake3::derive_key("pdmount block cache v1", master_key);
        let rows = store.read(|conn| {
            let mut statement =
                conn.prepare("SELECT link_id, rev, idx, bytes, last_access FROM cache_blocks")?;
            let rows = statement
                .query_map([], |row| {
                    Ok((
                        BlockKey {
                            link: LinkId(row.get(0)?),
                            rev: RevisionId(row.get(1)?),
                            index: row.get(2)?,
                        },
                        DiskEntry {
                            bytes: row.get::<_, i64>(3)? as u64,
                            last_access: row.get(4)?,
                        },
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })?;
        let bytes = rows.iter().map(|(_, entry)| entry.bytes).sum();
        Ok(Self {
            dir: dir.to_owned(),
            cipher: XChaCha20Poly1305::new(&key.into()),
            index: parking_lot::Mutex::new(DiskIndex {
                entries: rows.into_iter().collect(),
                bytes,
                touched: Vec::new(),
            }),
            limit,
        })
    }

    fn path(&self, key: &BlockKey) -> PathBuf {
        let hash = blake3::hash(&key.encode()).to_hex();
        self.dir.join(&hash[..2]).join(hash.as_str())
    }

    /// Reads and authenticates a block into a buffer from `buffer` (called
    /// with the block's length); the plaintext is at [`PLAIN_START`].
    /// Missing or corrupt entries are dropped and reported as a miss.
    fn read(
        &self,
        key: &BlockKey,
        buffer: impl FnOnce(usize) -> Buffer,
        store: &Store,
        metrics: &Metrics,
    ) -> Option<(Buffer, usize)> {
        if !self.index.lock().entries.contains_key(key) {
            return None;
        }
        let path = self.path(key);
        let unreadable = |error: std::io::Error| {
            tracing::warn!(%error, path = %path.display(), "cached block unreadable; fetching again");
            self.remove(key, store);
        };
        let corrupt = || {
            tracing::warn!(path = %path.display(), "cached block failed authentication; fetching again");
            Metrics::inc(&metrics.cache_corrupt);
            let _ = std::fs::remove_file(&path);
            self.remove(key, store);
        };
        let file = match std::fs::File::open(&path).and_then(|file| Ok((file.metadata()?, file))) {
            Ok(opened) => opened,
            Err(error) => {
                unreadable(error);
                return None;
            }
        };
        let (metadata, file) = file;
        let sealed = metadata.len() as usize;
        let Some(len) = sealed
            .checked_sub(SEAL_OVERHEAD)
            .filter(|&len| len as u64 <= BLOCK_SIZE)
        else {
            corrupt();
            return None;
        };
        let mut buffer = buffer(len);
        let target = &mut buffer.as_mut()[..sealed];
        if let Err(error) = file.read_exact_at(target, 0) {
            unreadable(error);
            return None;
        }
        if !self.unseal(key, target) {
            corrupt();
            return None;
        }
        let mut index = self.index.lock();
        if let Some(entry) = index.entries.get_mut(key) {
            entry.last_access = now_secs();
            index.touched.push(key.clone());
        }
        Some((buffer, len))
    }

    /// Encrypts `sealed` (nonce space ‖ plaintext ‖ tag space) in place.
    fn seal(&self, key: &BlockKey, sealed: &mut [u8]) -> Result<()> {
        let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
        let (head, tag) = sealed.split_at_mut(sealed.len() - TAG_LEN);
        let (nonce_out, text) = head.split_at_mut(NONCE_LEN);
        nonce_out.copy_from_slice(&nonce);
        let computed = self
            .cipher
            .encrypt_in_place_detached(&nonce, &key.encode(), text)
            .map_err(|_| CoreError::Internal("block encryption failed".into()))?;
        tag.copy_from_slice(&computed);
        Ok(())
    }

    /// Authenticates and decrypts `sealed` in place; `false` if it fails.
    fn unseal(&self, key: &BlockKey, sealed: &mut [u8]) -> bool {
        let Some(text_end) = sealed
            .len()
            .checked_sub(TAG_LEN)
            .filter(|&end| end >= NONCE_LEN)
        else {
            return false;
        };
        let (head, tag) = sealed.split_at_mut(text_end);
        let (nonce, text) = head.split_at_mut(NONCE_LEN);
        self.cipher
            .decrypt_in_place_detached(
                XNonce::from_slice(nonce),
                &key.encode(),
                text,
                Tag::from_slice(tag),
            )
            .is_ok()
    }

    /// Stores a block sealed by [`DiskCache::seal`].
    fn put(&self, key: &BlockKey, sealed: &[u8], store: &Store, metrics: &Metrics) -> Result<()> {
        let path = self.path(key);
        let dir = path.parent().expect("block paths have a parent");
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        let temp = dir.join(format!(".tmp-{}", uuid::Uuid::new_v4()));
        let written = (|| {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temp)?;
            file.write_all(sealed)?;
            std::fs::rename(&temp, &path)
        })();
        if let Err(error) = written {
            let _ = std::fs::remove_file(&temp);
            return Err(error.into());
        }
        let entry = DiskEntry {
            bytes: sealed.len() as u64,
            last_access: now_secs(),
        };
        let row_key = key.clone();
        store.write(move |conn| {
            conn.execute(
                "INSERT OR REPLACE INTO cache_blocks(link_id, rev, idx, bytes, last_access, pin)
                 VALUES (?1, ?2, ?3, ?4, ?5, 0)",
                (
                    &row_key.link.0,
                    &row_key.rev.0,
                    row_key.index,
                    entry.bytes as i64,
                    entry.last_access,
                ),
            )
            .map(drop)
        });
        let over = {
            let mut index = self.index.lock();
            if let Some(old) = index.entries.insert(key.clone(), entry) {
                index.bytes -= old.bytes;
            }
            index.bytes += entry.bytes;
            index.bytes > self.limit / 10 * 9
        };
        if over {
            self.evict(store, metrics);
        }
        Ok(())
    }

    /// Evicts least recently used entries down to 80 % of the limit.
    fn evict(&self, store: &Store, metrics: &Metrics) {
        let target = self.limit / 10 * 8;
        let victims: Vec<BlockKey> = {
            let mut index = self.index.lock();
            let mut by_age: Vec<(i64, BlockKey, u64)> = index
                .entries
                .iter()
                .map(|(key, entry)| (entry.last_access, key.clone(), entry.bytes))
                .collect();
            by_age.sort_by_key(|(age, _, _)| *age);
            let mut victims = Vec::new();
            for (_, key, bytes) in by_age {
                if index.bytes <= target {
                    break;
                }
                index.entries.remove(&key);
                index.bytes -= bytes;
                victims.push(key);
            }
            victims
        };
        for key in &victims {
            let _ = std::fs::remove_file(self.path(key));
            Metrics::inc(&metrics.cache_evictions);
        }
        delete_rows(store, victims);
    }

    fn remove(&self, key: &BlockKey, store: &Store) {
        let mut index = self.index.lock();
        if let Some(entry) = index.entries.remove(key) {
            index.bytes -= entry.bytes;
        }
        drop(index);
        delete_rows(store, vec![key.clone()]);
    }

    /// Persists batched `last_access` updates.
    fn flush_access(&self, store: &Store) {
        let updates: Vec<(BlockKey, i64)> = {
            let mut index = self.index.lock();
            let touched = std::mem::take(&mut index.touched);
            touched
                .into_iter()
                .filter_map(|key| index.entries.get(&key).map(|e| (key, e.last_access)))
                .collect()
        };
        if updates.is_empty() {
            return;
        }
        store.write(move |conn| {
            let mut statement = conn.prepare(
                "UPDATE cache_blocks SET last_access = ?4 WHERE link_id = ?1 AND rev = ?2 AND idx = ?3",
            )?;
            for (key, last_access) in &updates {
                statement.execute((&key.link.0, &key.rev.0, key.index, last_access))?;
            }
            Ok(())
        });
    }

    fn usage(&self) -> (u64, usize) {
        let index = self.index.lock();
        (index.bytes, index.entries.len())
    }
}

fn delete_rows(store: &Store, keys: Vec<BlockKey>) {
    if keys.is_empty() {
        return;
    }
    store.write(move |conn| {
        let mut statement =
            conn.prepare("DELETE FROM cache_blocks WHERE link_id = ?1 AND rev = ?2 AND idx = ?3")?;
        for key in &keys {
            statement.execute((&key.link.0, &key.rev.0, key.index))?;
        }
        Ok(())
    });
}

pub struct CacheOptions {
    pub dir: PathBuf,
    pub master_key: [u8; 32],
    pub ram_bytes: u64,
    pub disk_bytes: u64,
}

pub struct BlockCache {
    backend: Arc<dyn Backend>,
    store: Arc<Store>,
    metrics: Arc<Metrics>,
    pool: Arc<Pool>,
    ram: parking_lot::Mutex<RamCache>,
    disk: DiskCache,
    layouts: parking_lot::Mutex<lru::LruCache<(LinkId, RevisionId), Arc<Layout>>>,
    layout_flights: SingleFlight<(LinkId, RevisionId), Arc<Layout>>,
    block_flights: SingleFlight<BlockKey, Bytes>,
    interactive: Arc<Semaphore>,
    readahead: Arc<Semaphore>,
    prefetch: Arc<Semaphore>,
    /// Taken after the priority's own permit.
    downloads: Arc<Semaphore>,
}

impl BlockCache {
    /// Fails if `ram_bytes` is too small for the pool ([`Pool::new`]).
    pub fn open(
        backend: Arc<dyn Backend>,
        store: Arc<Store>,
        metrics: Arc<Metrics>,
        options: CacheOptions,
    ) -> Result<Self> {
        let pool = Pool::new(options.ram_bytes)?;
        Self::with_pool(backend, store, metrics, options, pool)
    }

    fn with_pool(
        backend: Arc<dyn Backend>,
        store: Arc<Store>,
        metrics: Arc<Metrics>,
        options: CacheOptions,
        pool: Pool,
    ) -> Result<Self> {
        let disk = DiskCache::open(
            &options.dir,
            &options.master_key,
            options.disk_bytes,
            &store,
        )?;
        Ok(Self {
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
            interactive: Arc::new(Semaphore::new(INTERACTIVE_DOWNLOADS)),
            readahead: Arc::new(Semaphore::new(READAHEAD_DOWNLOADS)),
            prefetch: Arc::new(Semaphore::new(PREFETCH_DOWNLOADS)),
            downloads: Arc::new(Semaphore::new(DOWNLOADS)),
        })
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
                let mut statement = conn.prepare(
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
        if let Some(data) = self.ram.lock().get(key) {
            Metrics::inc(&self.metrics.cache_ram_hits);
            return Some(data);
        }
        let (buffer, len) =
            self.disk
                .read(key, |len| self.buffer_now(len), &self.store, &self.metrics)?;
        Metrics::inc(&self.metrics.cache_disk_hits);
        Some(self.keep(key, buffer, len))
    }

    /// A slot for a block of `len` bytes: a free one, else one freed by
    /// evicting RAM-cache entries of its class. `None` if every slot of the
    /// class is still referenced.
    fn take_slot(&self, len: usize) -> Option<SlotMut> {
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

    fn fallback(&self, len: usize) -> Buffer {
        Metrics::inc(&self.metrics.cache_pool_fallbacks);
        Buffer::heap(len)
    }

    /// A buffer for the hot path: a slot if one can be had now, else the heap.
    fn buffer_now(&self, len: usize) -> Buffer {
        match self.take_slot(len) {
            Some(slot) => Buffer::Slot(slot),
            None => self.fallback(len),
        }
    }

    /// A buffer for the slow path: waits up to [`SLOT_WAIT`] for a slot,
    /// then takes the heap.
    async fn buffer(&self, len: usize) -> Buffer {
        if let Some(class) = self.pool.class_for(len)
            && let Some(slot) = self.wait_for(class, || self.take_slot(len)).await
        {
            return Buffer::Slot(slot);
        }
        self.fallback(len)
    }

    /// Retries `take` whenever a slot of `class` is freed or becomes
    /// evictable, for up to [`SLOT_WAIT`].
    async fn wait_for(&self, class: usize, take: impl Fn() -> Option<SlotMut>) -> Option<SlotMut> {
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
    fn keep(&self, key: &BlockKey, buffer: Buffer, len: usize) -> Bytes {
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

    /// Seals a downloaded block (plaintext at [`PLAIN_START`]) into the disk
    /// cache, opens it again in place and keeps it.
    fn store_download(&self, key: &BlockKey, mut buffer: Buffer, len: usize) -> Result<Bytes> {
        let sealed = &mut buffer.as_mut()[..len + SEAL_OVERHEAD];
        self.disk.seal(key, sealed)?;
        if let Err(error) = self.disk.put(key, sealed, &self.store, &self.metrics) {
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
        self.ram.lock().entries.contains(key) || self.disk.index.lock().entries.contains_key(key)
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
        let this = Arc::clone(self);
        let owned = key.clone();
        self.block_flights
            .run(key.clone(), move || async move {
                let permits = match priority {
                    Priority::Interactive => &this.interactive,
                    Priority::Readahead => &this.readahead,
                    Priority::Prefetch => &this.prefetch,
                };
                let closed = |_| CoreError::Internal("download permits closed".into());
                let _permit = Arc::clone(permits).acquire_owned().await.map_err(closed)?;
                let _download = Arc::clone(&this.downloads)
                    .acquire_owned()
                    .await
                    .map_err(closed)?;
                // Another flight may have filled it while this one waited.
                if let Some(data) = this.get_local(&owned) {
                    return Ok(data);
                }
                check_not_hot(&this.metrics, "fetch_block");
                Metrics::inc(&this.metrics.cache_misses);
                if priority != Priority::Interactive {
                    Metrics::inc(&this.metrics.prefetched_blocks);
                }
                let expected = expected_len as usize;
                let mut buffer = this.buffer(expected).await;
                let plain = &mut buffer.as_mut()[PLAIN_START..PLAIN_START + expected];
                let len = match this
                    .backend
                    .fetch_block_into(&owned.link, &owned.rev, owned.index, plain)
                    .await
                {
                    Ok(len) => len,
                    Err(error) => {
                        if matches!(error, pdmount_backend::BackendError::Integrity(_)) {
                            tracing::error!(link = %owned.link, rev = %owned.rev, index = owned.index, %error,
                                "block failed verification; not cached");
                        }
                        return Err(error.into());
                    }
                };
                if len != expected {
                    let error = format!(
                        "block {} of {} has {len} bytes, expected {expected_len}",
                        owned.index, owned.rev
                    );
                    tracing::error!("{error}; not cached");
                    return Err(pdmount_backend::BackendError::Integrity(error).into());
                }
                this.store_download(&owned, buffer, len)
            })
            .await
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
            self.disk.put(&key, sealed, &self.store, &self.metrics)?;
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

    /// Disk bytes and entry count.
    pub fn usage(&self) -> (u64, usize) {
        self.disk.usage()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pdmount_backend::fake::FakeBackend;
    use pdmount_backend::{FileMeta, UploadSource};
    use std::time::Duration;

    fn infos(sizes: &[Option<u32>]) -> Vec<BlockInfo> {
        sizes
            .iter()
            .enumerate()
            .map(|(i, &plain_size)| BlockInfo {
                index: i as u32 + 1,
                plain_size,
            })
            .collect()
    }

    #[test]
    fn layout_uses_declared_sizes() {
        let layout = Layout::new(&infos(&[Some(10), Some(5)]), 15).expect("layout");
        assert_eq!(
            layout.blocks[1],
            BlockSpan {
                index: 2,
                offset: 10,
                len: 5
            }
        );
        assert_eq!(layout.size(), 15);
    }

    #[test]
    fn layout_without_sizes_assumes_4_mib_blocks() {
        let size = BLOCK_SIZE * 2 + 7;
        let layout = Layout::new(&infos(&[None, None, None]), size).expect("layout");
        assert_eq!(layout.blocks[2].offset, BLOCK_SIZE * 2);
        assert_eq!(layout.blocks[2].len, 7);
        assert!(Layout::new(&infos(&[None, None]), BLOCK_SIZE * 2 + 1).is_err());
        assert!(Layout::new(&infos(&[None, None]), BLOCK_SIZE).is_err());
        assert!(Layout::new(&infos(&[]), 0).is_ok());
        assert!(
            Layout::new(&infos(&[]), 5).is_err(),
            "no blocks cannot hold a non-empty file"
        );
    }

    #[test]
    fn spans_and_assembly_cover_partial_ranges() {
        let layout = Layout::new(&infos(&[Some(4), Some(4), Some(4)]), 12).expect("layout");
        let spans = layout.spans(3, 6);
        assert_eq!(
            spans.iter().map(|s| s.index).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        let blocks = [&b"abcd"[..], b"efgh", b"ijkl"];
        let mut read = ReadBuf::new(Buffer::heap(6), 3, 6);
        for (span, data) in spans.iter().zip(blocks) {
            read.append(span, data);
        }
        assert_eq!(read.finish(), b"defghi");
        let one = ReadData::of_block(&spans[1], Bytes::from_static(b"efgh"), 5, 2);
        assert_eq!(one, b"fg");
        assert!(layout.spans(12, 5).is_empty());
        assert_eq!(layout.spans(4, 4).len(), 1);
    }

    fn disk_get(cache: &BlockCache, key: &BlockKey) -> Option<Bytes> {
        let (buffer, len) = cache
            .disk
            .read(key, Buffer::heap, &cache.store, &cache.metrics)?;
        Some(buffer.freeze(len))
    }

    async fn setup(
        dir: &Path,
        disk_bytes: u64,
    ) -> (Arc<BlockCache>, Arc<Store>, LinkId, RevisionId) {
        let (cache, store, link, rev, _) = setup_with(dir, disk_bytes, b"0123456789").await;
        (cache, store, link, rev)
    }

    /// A cache over a fake drive holding one file `f` with 4-byte blocks.
    async fn setup_with(
        dir: &Path,
        disk_bytes: u64,
        content: &'static [u8],
    ) -> (Arc<BlockCache>, Arc<Store>, LinkId, RevisionId, FakeBackend) {
        let pool = Pool::new(16 << 20).expect("pool");
        setup_pooled(dir, disk_bytes, content, pool).await
    }

    async fn setup_pooled(
        dir: &Path,
        disk_bytes: u64,
        content: &'static [u8],
        pool: Pool,
    ) -> (Arc<BlockCache>, Arc<Store>, LinkId, RevisionId, FakeBackend) {
        let backend = FakeBackend::new();
        backend.set_block_size(4);
        let root = backend.root().await.expect("root").link_id;
        let meta = FileMeta {
            mtime: UNIX_EPOCH,
            mode: None,
            media_type: "application/octet-stream".into(),
            sha1: None,
        };
        let (link, rev) = backend
            .upload_new(
                &root,
                "f",
                UploadSource::Bytes(Bytes::from_static(content)),
                meta,
            )
            .await
            .expect("upload");
        let store = Arc::new(Store::open(&dir.join("state.db")).expect("store"));
        let cache = BlockCache::with_pool(
            Arc::new(backend.clone()),
            Arc::clone(&store),
            Arc::default(),
            CacheOptions {
                dir: dir.join("blocks"),
                master_key: [7; 32],
                ram_bytes: 16 << 20,
                disk_bytes,
            },
            pool,
        )
        .expect("cache");
        (Arc::new(cache), store, link, rev, backend)
    }

    #[tokio::test]
    async fn at_most_five_blocks_download_at_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (cache, _store, link, rev, backend) = setup_with(
            dir.path(),
            1 << 20,
            b"0123456789abcdefghijklmnopqrstuvwxyzABCD",
        )
        .await;
        let layout = cache.layout(&link, &rev, 40).await.expect("layout");
        assert_eq!(layout.blocks.len(), 10);
        backend.set_latency(Duration::from_secs(1));
        let calls = || {
            cache
                .metrics
                .backend_calls
                .load(std::sync::atomic::Ordering::Relaxed)
        };
        let before = calls();
        let priorities = [
            Priority::Interactive,
            Priority::Readahead,
            Priority::Prefetch,
        ];
        let gets: Vec<_> = (1..=10)
            .map(|index: u32| {
                let cache = Arc::clone(&cache);
                let key = BlockKey::new(&link, &rev, index);
                tokio::spawn(
                    async move { cache.get(&key, 4, priorities[index as usize % 3]).await },
                )
            })
            .collect();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(calls() - before, DOWNLOADS as u64);
        for get in gets {
            get.await.expect("join").expect("get");
        }
        assert_eq!(calls() - before, 10);
    }

    #[tokio::test]
    async fn blocks_are_fetched_once_then_served_locally() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (cache, _store, link, rev) = setup(dir.path(), 1 << 20).await;
        let layout = cache.layout(&link, &rev, 10).await.expect("layout");
        assert_eq!(layout.blocks.len(), 3);
        let key = BlockKey::new(&link, &rev, 2);
        assert_eq!(
            cache
                .get(&key, 4, Priority::Interactive)
                .await
                .expect("get"),
            &b"4567"[..]
        );
        assert_eq!(cache.get_local(&key).expect("local"), &b"4567"[..]);
        assert_eq!(
            cache
                .metrics
                .cache_misses
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
    }

    #[tokio::test]
    async fn the_disk_cache_survives_reopen_and_rejects_tampering() {
        let dir = tempfile::tempdir().expect("tempdir");
        let key;
        {
            let (cache, store, link, rev) = setup(dir.path(), 1 << 20).await;
            key = BlockKey {
                link,
                rev,
                index: 1,
            };
            cache
                .get(&key, 4, Priority::Interactive)
                .await
                .expect("get");
            store.flush().await.expect("flush");
        }
        let (cache, _store, _, _) = setup(dir.path(), 1 << 20).await;
        // Different fake upload ids are irrelevant: the key is reused as is.
        assert_eq!(disk_get(&cache, &key).expect("disk"), &b"0123"[..]);
        let path = cache.disk.path(&key);
        let mut sealed = std::fs::read(&path).expect("read");
        let last = sealed.len() - 1;
        sealed[last] ^= 1;
        std::fs::write(&path, sealed).expect("write");
        assert!(disk_get(&cache, &key).is_none());
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn the_disk_cache_evicts_down_to_80_percent() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Each sealed block is 24 + 4 + 16 = 44 bytes; the limit fits two.
        let (cache, _store, link, rev) = setup(dir.path(), 100).await;
        for index in 1..=3 {
            let key = BlockKey::new(&link, &rev, index);
            let len = if index == 3 { 2 } else { 4 };
            cache
                .get(&key, len, Priority::Interactive)
                .await
                .expect("get");
        }
        let (bytes, _) = cache.usage();
        assert!(bytes <= 80, "{bytes} bytes left");
        assert!(
            cache
                .metrics
                .cache_evictions
                .load(std::sync::atomic::Ordering::Relaxed)
                > 0
        );
    }

    #[tokio::test]
    async fn a_short_block_is_an_integrity_error_and_not_cached() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (cache, _store, link, rev) = setup(dir.path(), 1 << 20).await;
        let key = BlockKey {
            link,
            rev,
            index: 3,
        };
        let error = cache
            .get(&key, 4, Priority::Interactive)
            .await
            .expect_err("short");
        assert_eq!(error.errno(), nix::libc::EIO);
        assert!(!cache.contains(&key));
    }

    fn counter(counter: &std::sync::atomic::AtomicU64) -> u64 {
        counter.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn in_ram(cache: &BlockCache, key: &BlockKey) -> bool {
        cache.ram.lock().entries.contains(key)
    }

    #[tokio::test]
    async fn held_blocks_keep_their_slots_and_the_hot_path_falls_back() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pool = Pool::with_slots([2, 0, 0]).expect("pool");
        let (cache, _store, link, rev, _) =
            setup_pooled(dir.path(), 1 << 20, b"0123456789", pool).await;
        let key = |index| BlockKey::new(&link, &rev, index);
        for (index, len) in [(1, 4), (2, 4), (3, 2)] {
            cache
                .get(&key(index), len, Priority::Interactive)
                .await
                .expect("get");
        }
        // Two slots: block 3 took block 1's.
        assert!(!in_ram(&cache, &key(1)));
        let held = (
            cache.get_local(&key(2)).expect("2"),
            cache.get_local(&key(3)).expect("3"),
        );
        assert_eq!(cache.pool_usage()[0].in_use, 2);
        // Evicting blocks 2 and 3 frees no slot while they are held, so block
        // 1 comes from the heap and is not kept.
        assert_eq!(cache.get_local(&key(1)).expect("1"), &b"0123"[..]);
        assert_eq!(counter(&cache.metrics.cache_pool_fallbacks), 1);
        assert!(!in_ram(&cache, &key(1)) && !in_ram(&cache, &key(2)));
        assert_eq!(cache.ram.lock().bytes, 0);
        assert_eq!(&held.0[..], b"4567");
        drop(held);
        assert_eq!(cache.pool_usage()[0].in_use, 0);
        assert_eq!(cache.get_local(&key(1)).expect("1"), &b"0123"[..]);
        assert_eq!(counter(&cache.metrics.cache_pool_fallbacks), 1);
        assert!(in_ram(&cache, &key(1)));
        assert_eq!(cache.ram.lock().bytes, cache.pool.slot_size(0) as u64);
    }

    #[tokio::test]
    async fn a_download_waits_for_a_held_slot() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pool = Pool::with_slots([1, 0, 0]).expect("pool");
        let (cache, _store, link, rev, _) =
            setup_pooled(dir.path(), 1 << 20, b"0123456789", pool).await;
        let key = |index| BlockKey::new(&link, &rev, index);
        let held = cache
            .get(&key(1), 4, Priority::Interactive)
            .await
            .expect("get");
        let download = tokio::spawn({
            let cache = Arc::clone(&cache);
            let key = key(2);
            async move { cache.get(&key, 4, Priority::Interactive).await }
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!download.is_finished());
        drop(held);
        let data = download.await.expect("join").expect("get");
        assert_eq!(&data[..], b"4567");
        assert_eq!(counter(&cache.metrics.cache_pool_fallbacks), 0);
        assert!(in_ram(&cache, &key(2)));
    }

    #[tokio::test]
    async fn seed_round_trips_through_the_disk_cache() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (cache, _store, _, _) = setup(dir.path(), 1 << 20).await;
        let snapshot = dir.path().join("snapshot");
        std::fs::write(&snapshot, b"seeded content!").expect("write");
        let (link, rev) = (LinkId("seeded".into()), RevisionId("r1".into()));
        cache.seed(&link, &rev, &snapshot, 4).await.expect("seed");
        assert!(cache.pool_usage().iter().all(|class| class.in_use == 0));
        let layout = cache.layout_local(&link, &rev, 15).expect("layout");
        assert_eq!(layout.blocks.len(), 4);
        let mut read = Vec::new();
        for span in &layout.blocks {
            let key = BlockKey::new(&link, &rev, span.index);
            read.extend_from_slice(&cache.get_local(&key).expect("seeded"));
        }
        assert_eq!(read, b"seeded content!");
        assert_eq!(counter(&cache.metrics.cache_misses), 0);
        assert_eq!(counter(&cache.metrics.cache_disk_hits), 4);
    }
}
