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
use crate::staging::Protections;
use crate::store::Store;
use bytes::Bytes;
use chacha20poly1305::aead::{AeadCore, AeadInPlace, KeyInit, OsRng};
use chacha20poly1305::{Tag, XChaCha20Poly1305, XNonce};
use pdmount_backend::{Backend, BlockInfo, LinkId, RevisionId};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
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

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
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
        let mut statement = conn.prepare_cached(
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
    /// assumes 4 MiB blocks. Either way the blocks must add up to
    /// `file_size`. Declared sizes are the uploader's claim (§6.1): a block
    /// above 4 MiB, an empty block before the last, or a sum other than the
    /// file size is an integrity failure, so no claim sizes a buffer. No
    /// blocks at all take the second path, so they hold only an empty file.
    pub fn new(infos: &[BlockInfo], file_size: u64) -> Result<Self> {
        let integrity =
            |message: String| CoreError::Backend(pdmount_backend::BackendError::Integrity(message));
        let mut blocks = Vec::with_capacity(infos.len());
        let mut offset = 0u64;
        if !infos.is_empty() && infos.iter().all(|info| info.plain_size.is_some()) {
            for (position, info) in infos.iter().enumerate() {
                let len = info.plain_size.unwrap_or(0);
                let last = position + 1 == infos.len();
                if u64::from(len) > BLOCK_SIZE || (len == 0 && !last) {
                    return Err(integrity(format!(
                        "block {} declares {len} bytes",
                        info.index
                    )));
                }
                blocks.push(BlockSpan {
                    index: info.index,
                    offset,
                    len,
                });
                offset += u64::from(len);
            }
            if offset != file_size {
                return Err(integrity(format!(
                    "block sizes add up to {offset} bytes, the file has {file_size}"
                )));
            }
            return Ok(Self { blocks });
        }
        let count = infos.len() as u64;
        let full = count.saturating_sub(1) * BLOCK_SIZE;
        let last = file_size.checked_sub(full).filter(|&last| {
            (count == 0 && last == 0) || (count > 0 && last > 0 && last <= BLOCK_SIZE)
        });
        let Some(last) = last else {
            return Err(integrity(format!(
                "{count} blocks of at most 4 MiB cannot hold {file_size} bytes"
            )));
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

/// Where a read served without the network found its bytes; the FUSE
/// layer times it under this (§5.8). Ordered by cost: a read across blocks
/// counts as its slowest source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ReadFrom {
    Ram,
    Disk,
    Staging,
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

    /// Removes an entry. Its slot is free once the returned `Bytes` and every
    /// clone of them are dropped.
    fn remove(&mut self, key: &BlockKey) -> Option<Bytes> {
        let cached = self.entries.pop(key)?;
        self.bytes -= cached.slot_size as u64;
        Some(cached.data)
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
    /// Never evicted, and not counted against the cap (§6.2).
    pinned: bool,
}

#[derive(Default)]
struct DiskIndex {
    /// Ordered, so the blocks of a revision or a file are a range.
    entries: BTreeMap<BlockKey, DiskEntry>,
    /// The unpinned entries, oldest first.
    by_age: BTreeSet<(i64, BlockKey)>,
    /// File sizes of the unpinned entries; the watermarks apply to these.
    bytes: u64,
    pinned_bytes: u64,
    /// Revisions whose blocks are pinned, also those still to arrive.
    pinned_revs: HashSet<(LinkId, RevisionId)>,
    /// Entries whose `last_access` changed since the last flush.
    touched: HashSet<BlockKey>,
}

impl DiskIndex {
    fn insert(&mut self, key: BlockKey, entry: DiskEntry) {
        self.remove(&key);
        if entry.pinned {
            self.pinned_bytes += entry.bytes;
        } else {
            self.bytes += entry.bytes;
            self.by_age.insert((entry.last_access, key.clone()));
        }
        self.entries.insert(key, entry);
    }

    fn remove(&mut self, key: &BlockKey) -> Option<DiskEntry> {
        let entry = self.entries.remove(key)?;
        if entry.pinned {
            self.pinned_bytes -= entry.bytes;
        } else {
            self.bytes -= entry.bytes;
            self.by_age.remove(&(entry.last_access, key.clone()));
        }
        Some(entry)
    }

    /// Marks an entry used at `now`, at most once per second per entry.
    fn touch(&mut self, key: &BlockKey, now: i64) {
        let Some(entry) = self.entries.get_mut(key) else {
            return;
        };
        if entry.last_access == now {
            return;
        }
        if !entry.pinned {
            self.by_age.remove(&(entry.last_access, key.clone()));
            self.by_age.insert((now, key.clone()));
        }
        entry.last_access = now;
        self.touched.insert(key.clone());
    }
}

/// What a maintenance command removed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Evicted {
    pub blocks: u64,
    pub bytes: u64,
}

/// What an orphan sweep removed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Swept {
    pub orphan_files: u64,
    pub dangling_rows: u64,
}

/// `cache stats`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskStats {
    /// Against `max_bytes`; pinned blocks are apart.
    pub bytes: u64,
    pub max_bytes: u64,
    pub blocks: u64,
    pub pinned_bytes: u64,
    pub protected_revisions: u64,
}

/// A file this much older than the start of a sweep was not just being
/// written: `put` renames it into place before its row is queued.
const SWEEP_GRACE: Duration = Duration::from_secs(2);

/// Encrypted on-disk block cache: `blocks/xx/<blake3(key)>` holding
/// nonce ‖ XChaCha20-Poly1305(block), with the key as associated data.
struct DiskCache {
    dir: PathBuf,
    cipher: XChaCha20Poly1305,
    index: parking_lot::Mutex<DiskIndex>,
    limit: u64,
    /// Revisions eviction skips, whoever asks for it.
    protections: Protections,
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

impl DiskCache {
    fn open(
        dir: &Path,
        master_key: &[u8; 32],
        limit: u64,
        store: &Store,
        protections: Protections,
    ) -> Result<Self> {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        let key = blake3::derive_key("pdmount block cache v1", master_key);
        let rows = store.read(|conn| {
            let mut statement = conn
                .prepare("SELECT link_id, rev, idx, bytes, last_access, pin FROM cache_blocks")?;
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
                            pinned: row.get::<_, i64>(5)? != 0,
                        },
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })?;
        let mut index = DiskIndex::default();
        for (key, entry) in rows {
            if entry.pinned {
                index
                    .pinned_revs
                    .insert((key.link.clone(), key.rev.clone()));
            }
            index.insert(key, entry);
        }
        Ok(Self {
            dir: dir.to_owned(),
            cipher: XChaCha20Poly1305::new(&key.into()),
            index: parking_lot::Mutex::new(index),
            limit,
            protections,
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
        self.touch(key);
        Some((buffer, len))
    }

    fn touch(&self, key: &BlockKey) {
        self.index.lock().touch(key, now_secs());
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
        {
            // Row queued under the lock, in order with `set_pinned`'s update.
            let mut index = self.index.lock();
            let entry = DiskEntry {
                bytes: sealed.len() as u64,
                last_access: now_secs(),
                pinned: !index.pinned_revs.is_empty()
                    && index
                        .pinned_revs
                        .contains(&(key.link.clone(), key.rev.clone())),
            };
            index.insert(key.clone(), entry);
            let row_key = key.clone();
            store.write(move |conn| {
                conn.prepare_cached(
                    "INSERT OR REPLACE INTO cache_blocks(link_id, rev, idx, bytes, last_access, pin)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                )?
                .execute((
                    &row_key.link.0,
                    &row_key.rev.0,
                    row_key.index,
                    entry.bytes as i64,
                    entry.last_access,
                    entry.pinned,
                ))
                .map(drop)
            });
        }
        self.enforce_cap(store, metrics);
        Ok(())
    }

    /// Above the upper watermark, evicts down to the lower one.
    fn enforce_cap(&self, store: &Store, metrics: &Metrics) {
        let over = self.index.lock().bytes > self.limit / 10 * 9;
        if over {
            self.evict_to(self.limit / 10 * 8, store, metrics);
        }
    }

    /// Pins a revision's blocks, present and future, or lets them go; let
    /// go, they are evictable by their last use.
    fn set_pinned(&self, link: &LinkId, rev: &RevisionId, pinned: bool, store: &Store) {
        let mut index = self.index.lock();
        let revision = (link.clone(), rev.clone());
        if pinned {
            index.pinned_revs.insert(revision);
        } else {
            index.pinned_revs.remove(&revision);
        }
        let changing: Vec<(BlockKey, DiskEntry)> = index
            .entries
            .range(BlockKey::new(link, rev, 0)..)
            .take_while(|(key, _)| key.link == *link && key.rev == *rev)
            .filter(|(_, entry)| entry.pinned != pinned)
            .map(|(key, entry)| (key.clone(), *entry))
            .collect();
        for (key, entry) in changing {
            index.insert(key, DiskEntry { pinned, ..entry });
        }
        let (link, rev) = (link.0.clone(), rev.0.clone());
        store.write(move |conn| {
            conn.prepare_cached("UPDATE cache_blocks SET pin = ?3 WHERE link_id = ?1 AND rev = ?2")?
                .execute((&link, &rev, pinned))
                .map(drop)
        });
    }

    /// Evicts the least recently used entries down to `target` bytes, or as
    /// far as protected revisions allow.
    fn evict_to(&self, target: u64, store: &Store, metrics: &Metrics) -> Evicted {
        let (victims, evicted) = {
            let mut index = self.index.lock();
            let protected = self.protections.lock();
            let mut left = index.bytes;
            let mut victims = Vec::new();
            for (_, key) in &index.by_age {
                if left <= target {
                    break;
                }
                if is_protected(&protected, key) {
                    continue;
                }
                left -= index.entries.get(key).map_or(0, |entry| entry.bytes);
                victims.push(key.clone());
            }
            let evicted = take_entries(&mut index, &victims);
            (victims, evicted)
        };
        self.drop_files(victims, evicted, store, metrics)
    }

    /// Evicts every unprotected, unpinned entry; with `links`, only theirs.
    fn wipe(&self, links: Option<&HashSet<LinkId>>, store: &Store, metrics: &Metrics) -> Evicted {
        let (victims, evicted) = {
            let mut index = self.index.lock();
            let protected = self.protections.lock();
            let evictable = |key: &BlockKey| !is_protected(&protected, key);
            let victims: Vec<BlockKey> = match links {
                None => index
                    .by_age
                    .iter()
                    .map(|(_, key)| key)
                    .filter(|key| evictable(key))
                    .cloned()
                    .collect(),
                Some(links) => links
                    .iter()
                    .flat_map(|link| {
                        let start = BlockKey::new(link, &RevisionId(String::new()), 0);
                        index
                            .entries
                            .range(start..)
                            .take_while(move |(key, _)| key.link == *link)
                    })
                    .filter(|(key, entry)| !entry.pinned && evictable(key))
                    .map(|(key, _)| key.clone())
                    .collect(),
            };
            let evicted = take_entries(&mut index, &victims);
            (victims, evicted)
        };
        self.drop_files(victims, evicted, store, metrics)
    }

    /// Removes the files and rows of entries taken from the index, except
    /// for a block stored again meanwhile.
    fn drop_files(
        &self,
        mut victims: Vec<BlockKey>,
        evicted: Evicted,
        store: &Store,
        metrics: &Metrics,
    ) -> Evicted {
        victims.retain(|key| !self.index.lock().entries.contains_key(key));
        for key in &victims {
            let _ = std::fs::remove_file(self.path(key));
            Metrics::inc(&metrics.cache_evictions);
        }
        delete_rows(store, victims);
        evicted
    }

    fn remove(&self, key: &BlockKey, store: &Store) {
        self.index.lock().remove(key);
        delete_rows(store, vec![key.clone()]);
    }

    /// Removes files without a row and rows without a file, one directory
    /// at a time, without the index mutex during the walk. Rows come from
    /// the store (flushed by the caller): files written since the sweep
    /// began may have no row yet, so they are left alone.
    fn sweep(&self, store: &Store) -> Result<Swept> {
        let started = SystemTime::now() - SWEEP_GRACE;
        let rows: Vec<BlockKey> = store.read(|conn| {
            let mut statement = conn.prepare("SELECT link_id, rev, idx FROM cache_blocks")?;
            let rows = statement
                .query_map([], |row| {
                    Ok(BlockKey {
                        link: LinkId(row.get(0)?),
                        rev: RevisionId(row.get(1)?),
                        index: row.get(2)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })?;
        let mut by_dir: HashMap<String, HashMap<String, BlockKey>> = HashMap::new();
        for key in rows {
            let name = blake3::hash(&key.encode()).to_hex().to_string();
            by_dir
                .entry(name[..2].to_owned())
                .or_default()
                .insert(name, key);
        }
        let mut swept = Swept::default();
        for prefix in 0..=u8::MAX {
            let prefix = format!("{prefix:02x}");
            let rows = by_dir.remove(&prefix).unwrap_or_default();
            let mut seen = HashSet::new();
            let dir = self.dir.join(&prefix);
            let listing = match std::fs::read_dir(&dir) {
                Ok(listing) => Some(listing),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(error.into()),
            };
            for file in listing.into_iter().flatten() {
                let file = file?;
                let name = file.file_name().to_string_lossy().into_owned();
                if rows.contains_key(&name) {
                    seen.insert(name);
                    continue;
                }
                let metadata = file.metadata()?;
                if metadata.is_file() && metadata.modified()? < started {
                    tracing::debug!(file = %file.path().display(), "removing a cache file without a row");
                    let _ = std::fs::remove_file(file.path());
                    swept.orphan_files += 1;
                }
            }
            let dangling: Vec<BlockKey> = {
                let mut index = self.index.lock();
                let mut dangling = Vec::new();
                for (name, key) in &rows {
                    // Checked again: written since the listing, or evicted.
                    if !seen.contains(name)
                        && index.entries.contains_key(key)
                        && !self.path(key).exists()
                    {
                        index.remove(key);
                        dangling.push(key.clone());
                    }
                }
                dangling
            };
            swept.dangling_rows += dangling.len() as u64;
            delete_rows(store, dangling);
        }
        Ok(swept)
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
            let mut statement = conn.prepare_cached(
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

    fn stats(&self) -> DiskStats {
        let index = self.index.lock();
        DiskStats {
            bytes: index.bytes,
            max_bytes: self.limit,
            blocks: index.entries.len() as u64,
            pinned_bytes: index.pinned_bytes,
            protected_revisions: self.protections.len() as u64,
        }
    }
}

/// Takes `victims` out of the index, under the lock that chose them: a pin or
/// protection set from here on finds them already gone, not about to go.
fn take_entries(index: &mut DiskIndex, victims: &[BlockKey]) -> Evicted {
    let mut evicted = Evicted::default();
    for key in victims {
        if let Some(entry) = index.remove(key) {
            evicted.blocks += 1;
            evicted.bytes += entry.bytes;
        }
    }
    evicted
}

fn is_protected(protected: &HashMap<(LinkId, RevisionId), u32>, key: &BlockKey) -> bool {
    !protected.is_empty() && protected.contains_key(&(key.link.clone(), key.rev.clone()))
}

fn delete_rows(store: &Store, keys: Vec<BlockKey>) {
    if keys.is_empty() {
        return;
    }
    store.write(move |conn| {
        let mut statement = conn.prepare_cached(
            "DELETE FROM cache_blocks WHERE link_id = ?1 AND rev = ?2 AND idx = ?3",
        )?;
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
        protections: Protections,
    ) -> Result<Self> {
        let pool = Pool::new(options.ram_bytes)?;
        Self::with_pool(backend, store, metrics, options, protections, pool)
    }

    fn with_pool(
        backend: Arc<dyn Backend>,
        store: Arc<Store>,
        metrics: Arc<Metrics>,
        options: CacheOptions,
        protections: Protections,
        pool: Pool,
    ) -> Result<Self> {
        let disk = DiskCache::open(
            &options.dir,
            &options.master_key,
            options.disk_bytes,
            &store,
            protections,
        )?;
        // The cap may have been lowered since the last mount.
        disk.enforce_cap(&store, &metrics);
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

    /// Unpinned disk bytes and entry count.
    pub fn usage(&self) -> (u64, usize) {
        self.disk.usage()
    }

    pub fn disk_stats(&self) -> DiskStats {
        self.disk.stats()
    }

    /// `cache gc`: evicts down to `target` bytes.
    pub fn gc(&self, target: u64) -> Evicted {
        self.disk.evict_to(target, &self.store, &self.metrics)
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
            self.disk.enforce_cap(&self.store, &self.metrics);
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
    fn declared_sizes_must_fit_blocks_and_the_file() {
        let integrity = |result: Result<Layout>| {
            matches!(
                result,
                Err(CoreError::Backend(
                    pdmount_backend::BackendError::Integrity(_)
                ))
            )
        };
        let two_gib = 2u32 << 30;
        assert!(integrity(Layout::new(
            &infos(&[Some(two_gib)]),
            u64::from(two_gib)
        )));
        assert!(integrity(Layout::new(&infos(&[Some(0), Some(5)]), 5)));
        assert!(integrity(Layout::new(&infos(&[Some(10), Some(5)]), 16)));
        assert!(integrity(Layout::new(
            &infos(&[Some(10), Some(5)]),
            1 << 62
        )));
        // An empty last block, as an upload that ends on a block boundary may list.
        assert!(Layout::new(&infos(&[Some(4), Some(0)]), 4).is_ok());
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
            Protections::default(),
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

    /// Fetches every block of the 10-byte file `f` (blocks 1, 2 and 3).
    async fn fetch_all(cache: &Arc<BlockCache>, link: &LinkId, rev: &RevisionId) -> Vec<BlockKey> {
        let layout = cache.layout(link, rev, 10).await.expect("layout");
        let mut keys = Vec::new();
        for span in &layout.blocks {
            let key = BlockKey::new(link, rev, span.index);
            cache
                .get(&key, span.len, Priority::Interactive)
                .await
                .expect("get");
            keys.push(key);
        }
        keys
    }

    /// Dates an entry back, as if it was last used at `at`.
    fn age(cache: &BlockCache, key: &BlockKey, at: i64) {
        let mut index = cache.disk.index.lock();
        let entry = *index.entries.get(key).expect("entry");
        index.insert(
            key.clone(),
            DiskEntry {
                last_access: at,
                ..entry
            },
        );
    }

    fn on_disk(cache: &BlockCache, key: &BlockKey) -> bool {
        cache.disk.index.lock().entries.contains_key(key) && cache.disk.path(key).exists()
    }

    #[tokio::test]
    async fn eviction_follows_last_access_including_ram_hits() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (cache, _store, link, rev) = setup(dir.path(), 1 << 20).await;
        let keys = fetch_all(&cache, &link, &rev).await;
        age(&cache, &keys[0], 100);
        age(&cache, &keys[1], 200);
        age(&cache, &keys[2], 300);
        // The oldest block is in RAM: this hit must count on the disk too.
        assert!(in_ram(&cache, &keys[0]));
        cache.get_local(&keys[0]).expect("hit");
        // Room for the first block only.
        let room = cache.disk.index.lock().entries[&keys[0]].bytes;
        let evicted = cache.gc(room);
        assert_eq!(evicted.blocks, 2);
        assert!(on_disk(&cache, &keys[0]), "the block just read stays");
        assert!(!on_disk(&cache, &keys[1]));
        assert!(!on_disk(&cache, &keys[2]));
    }

    #[tokio::test]
    async fn last_access_updates_are_batched_into_the_store() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (cache, store, link, rev) = setup(dir.path(), 1 << 20).await;
        let keys = fetch_all(&cache, &link, &rev).await;
        age(&cache, &keys[0], 100);
        cache.get_local(&keys[0]).expect("hit");
        cache.get_local(&keys[0]).expect("hit");
        assert_eq!(cache.disk.index.lock().touched.len(), 1);
        cache.flush_access();
        assert!(cache.disk.index.lock().touched.is_empty());
        store.flush().await.expect("flush");
        let stored: i64 = store
            .read(|conn| {
                conn.query_row(
                    "SELECT last_access FROM cache_blocks WHERE idx = 1",
                    [],
                    |row| row.get(0),
                )
            })
            .expect("read");
        assert!(stored > 100);
    }

    #[tokio::test]
    async fn protected_revisions_survive_pressure_gc_and_wipe() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Arc::new(Store::open(&dir.path().join("state.db")).expect("store"));
        let staging = crate::staging::StagingDir::open(&dir.path().join("staging")).expect("dir");
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
                UploadSource::Bytes(Bytes::from_static(b"0123456789")),
                meta,
            )
            .await
            .expect("upload");
        let cache = Arc::new(
            BlockCache::with_pool(
                Arc::new(backend),
                Arc::clone(&store),
                Arc::default(),
                CacheOptions {
                    dir: dir.path().join("blocks"),
                    master_key: [7; 32],
                    ram_bytes: 16 << 20,
                    disk_bytes: 100,
                },
                staging.protections().clone(),
                Pool::new(16 << 20).expect("pool"),
            )
            .expect("cache"),
        );
        let base = crate::staging::Base {
            link: link.clone(),
            rev: rev.clone(),
            rev_size: 10,
            size: 10,
        };
        let mut staged = staging
            .create(2, Some(rev.clone()), Some(base.clone()), 10)
            .expect("stage");
        assert_eq!(cache.disk_stats().protected_revisions, 1);
        // Three blocks are 124 bytes, over the limit of 100.
        let keys = fetch_all(&cache, &link, &rev).await;
        assert!(keys.iter().all(|key| on_disk(&cache, key)));
        assert_eq!(cache.gc(0), Evicted::default());
        assert_eq!(cache.wipe(None), Evicted::default());
        let links = HashSet::from([link.clone()]);
        assert_eq!(cache.wipe(Some(&links)), Evicted::default());
        // Rebasing changes the upload's precondition, not where the bytes
        // come from: the blocks stay protected.
        staged
            .set_base_rev(Some(RevisionId("newer".into())))
            .expect("rebase");
        assert_eq!(cache.gc(0), Evicted::default());
        // Fully materialized, nothing reads the base any more.
        staged.truncate(0).expect("truncate");
        assert_eq!(cache.disk_stats().protected_revisions, 0);
        assert_eq!(cache.gc(0).blocks, 3);
        // Dropping a staging with a base also lets go.
        let staged = staging
            .create(3, Some(rev.clone()), Some(base), 10)
            .expect("stage");
        assert_eq!(cache.disk_stats().protected_revisions, 1);
        drop(staged);
        assert_eq!(cache.disk_stats().protected_revisions, 0);
        // A staging found at mount protects its base again.
        let staged = staging.reopen(3).expect("reopen");
        assert_eq!(cache.disk_stats().protected_revisions, 1);
        drop(staged);
    }

    #[tokio::test]
    async fn pinned_rows_are_kept_and_counted_apart() {
        let dir = tempfile::tempdir().expect("tempdir");
        let keys;
        {
            let (cache, store, link, rev) = setup(dir.path(), 1 << 20).await;
            keys = fetch_all(&cache, &link, &rev).await;
            store.flush().await.expect("flush");
            let index = keys[0].index;
            store.write(move |conn| {
                conn.execute("UPDATE cache_blocks SET pin = 1 WHERE idx = ?1", [index])
                    .map(drop)
            });
            store.flush().await.expect("flush");
        }
        let (cache, _store, _, _) = setup(dir.path(), 1 << 20).await;
        let stats = cache.disk_stats();
        assert_eq!(stats.pinned_bytes, 44);
        assert_eq!(stats.bytes, 44 + 42);
        assert_eq!(stats.blocks, 3);
        assert_eq!(cache.gc(0).blocks, 2);
        assert!(on_disk(&cache, &keys[0]));
        assert_eq!(cache.wipe(None), Evicted::default());
        assert!(on_disk(&cache, &keys[0]));
    }

    #[tokio::test]
    async fn pinned_revisions_hold_present_and_future_blocks() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (cache, store, link, rev) = setup(dir.path(), 100).await;
        let key = |index| BlockKey::new(&link, &rev, index);
        cache
            .get(&key(1), 4, Priority::Interactive)
            .await
            .expect("get");
        let revision = HashSet::from([(link.clone(), rev.clone())]);
        cache.set_pinned_revisions(&revision);
        assert_eq!(cache.disk_stats().pinned_bytes, 44);
        assert_eq!(cache.disk_stats().bytes, 0);
        // Blocks arriving later are pinned too, and past the cap of 100.
        for (index, len) in [(2, 4), (3, 2)] {
            cache
                .get(&key(index), len, Priority::Prefetch)
                .await
                .expect("get");
        }
        assert_eq!(cache.disk_stats().pinned_bytes, 44 + 44 + 42);
        assert_eq!(cache.gc(0), Evicted::default());
        assert_eq!(
            cache.bytes_on_disk(
                &link,
                &rev,
                &cache.layout(&link, &rev, 10).await.expect("layout")
            ),
            10
        );
        store.flush().await.expect("flush");
        let pinned: i64 = store
            .read(|conn| {
                conn.query_row(
                    "SELECT count(*) FROM cache_blocks WHERE pin = 1",
                    [],
                    |row| row.get(0),
                )
            })
            .expect("read");
        assert_eq!(pinned, 3);
        // Let go, they count against the cap again and are evicted to 80 %.
        cache.set_pinned_revisions(&HashSet::new());
        let stats = cache.disk_stats();
        assert_eq!(stats.pinned_bytes, 0);
        assert!(stats.bytes <= 80, "{stats:?}");
    }

    #[tokio::test]
    async fn a_lowered_cap_applies_when_the_cache_opens() {
        let dir = tempfile::tempdir().expect("tempdir");
        {
            let (cache, store, link, rev) = setup(dir.path(), 1 << 20).await;
            fetch_all(&cache, &link, &rev).await;
            store.flush().await.expect("flush");
        }
        let (cache, _store, _, _) = setup(dir.path(), 100).await;
        assert!(cache.usage().0 <= 80, "{:?}", cache.usage());
    }

    #[tokio::test]
    async fn gc_reaches_its_target_and_wipe_keeps_other_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (cache, _store, link, rev) = setup(dir.path(), 1 << 20).await;
        let keys = fetch_all(&cache, &link, &rev).await;
        let snapshot = dir.path().join("snapshot");
        std::fs::write(&snapshot, b"other").expect("write");
        let (other_link, other_rev) = (LinkId("other".into()), RevisionId("r".into()));
        cache
            .seed(&other_link, &other_rev, &snapshot, 4)
            .await
            .expect("seed");
        let other = BlockKey::new(&other_link, &other_rev, 1);
        assert!(on_disk(&cache, &other));

        let before = cache.disk_stats().bytes;
        let evicted = cache.gc(before - 1);
        assert_eq!(evicted.blocks, 1);
        assert!(cache.disk_stats().bytes < before);
        assert_eq!(cache.gc(1 << 30), Evicted::default());

        let wiped = cache.wipe(Some(&HashSet::from([link.clone()])));
        assert!(wiped.blocks >= 2);
        assert!(keys.iter().all(|key| !on_disk(&cache, key)));
        assert!(keys.iter().all(|key| !in_ram(&cache, key)));
        assert!(on_disk(&cache, &other));
        cache.wipe(None);
        assert_eq!(cache.usage(), (0, 0));
        assert!(!cache.disk.path(&other).exists());
    }

    /// Makes a file look older than the sweep's grace.
    fn backdate(path: &Path) {
        let file = std::fs::File::options()
            .write(true)
            .open(path)
            .expect("open");
        file.set_modified(SystemTime::now() - Duration::from_secs(60))
            .expect("set mtime");
    }

    #[tokio::test]
    async fn the_sweep_removes_orphan_files_and_dangling_rows() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (cache, store, link, rev) = setup(dir.path(), 1 << 20).await;
        let keys = fetch_all(&cache, &link, &rev).await;
        store.flush().await.expect("flush");
        // A file without a row, a crashed write, and a file too new to judge.
        let blocks = dir.path().join("blocks/ab");
        std::fs::create_dir_all(&blocks).expect("mkdir");
        let (orphan, temp, fresh) = (
            blocks.join("ab00"),
            blocks.join(".tmp-x"),
            blocks.join("ab01"),
        );
        for path in [&orphan, &temp, &fresh] {
            std::fs::write(path, b"x").expect("write");
        }
        backdate(&orphan);
        backdate(&temp);
        // A row whose file is gone.
        std::fs::remove_file(cache.disk.path(&keys[1])).expect("remove");

        let swept = cache.sweep().expect("sweep");
        assert_eq!(
            swept,
            Swept {
                orphan_files: 2,
                dangling_rows: 1
            }
        );
        assert!(!orphan.exists() && !temp.exists());
        assert!(fresh.exists());
        assert!(!cache.disk.index.lock().entries.contains_key(&keys[1]));
        assert!(on_disk(&cache, &keys[0]) && on_disk(&cache, &keys[2]));
        store.flush().await.expect("flush");
        let rows: i64 = store
            .read(|conn| conn.query_row("SELECT count(*) FROM cache_blocks", [], |row| row.get(0)))
            .expect("read");
        assert_eq!(rows, 2);
        // Bookkeeping agrees after the dangling row went.
        assert_eq!(cache.sweep().expect("sweep"), Swept::default());
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
