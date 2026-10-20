//! Block cache (DESIGN.md §6).
//!
//! Content is cached per block of an immutable revision, so entries are only
//! ever evicted, never invalidated. Lookups go RAM LRU → encrypted disk cache
//! → download. Downloads of the same block are shared ([`SingleFlight`]) and
//! limited per priority class.

use crate::errors::{CoreError, Result};
use crate::metrics::{Metrics, check_not_hot};
use crate::singleflight::SingleFlight;
use crate::store::Store;
use bytes::Bytes;
use chacha20poly1305::aead::{Aead, AeadCore, KeyInit, OsRng, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use pdmount_backend::{Backend, BlockInfo, LinkId, RevisionId};
use std::collections::HashMap;
use std::io::Write;
use std::num::NonZeroUsize;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Semaphore;

/// Proton's plaintext block size (`sdk/node/revision.rs:34`).
pub const BLOCK_SIZE: u64 = 4 * 1024 * 1024;
const NONCE_LEN: usize = 24;
const LAYOUT_CACHE_ENTRIES: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BlockKey {
    pub link: LinkId,
    pub rev: RevisionId,
    /// 1-based, as on the server.
    pub index: u32,
}

impl BlockKey {
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

/// Where each block of a revision sits in the file (§6.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub blocks: Vec<BlockSpan>,
}

impl Layout {
    /// Uses the declared plaintext sizes when every block has one; otherwise
    /// assumes 4 MiB blocks and checks that they add up to `file_size`.
    pub fn new(infos: &[BlockInfo], file_size: u64) -> Result<Self> {
        let mut blocks = Vec::with_capacity(infos.len());
        let mut offset = 0u64;
        if infos.iter().all(|info| info.plain_size.is_some()) {
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

/// Copies `[offset, offset + len)` out of the given blocks.
pub fn assemble(spans: &[BlockSpan], blocks: &[Bytes], offset: u64, len: u64) -> Vec<u8> {
    let end = offset + len;
    let mut out = Vec::with_capacity(len as usize);
    for (span, data) in spans.iter().zip(blocks) {
        let start = offset.max(span.offset) - span.offset;
        let stop =
            (end.min(span.offset + u64::from(span.len)) - span.offset).min(data.len() as u64);
        if start < stop {
            out.extend_from_slice(&data[start as usize..stop as usize]);
        }
    }
    out
}

struct RamCache {
    entries: lru::LruCache<BlockKey, Bytes>,
    bytes: u64,
    limit: u64,
}

impl RamCache {
    fn get(&mut self, key: &BlockKey) -> Option<Bytes> {
        self.entries.get(key).cloned()
    }

    fn put(&mut self, key: BlockKey, data: Bytes) {
        if data.len() as u64 > self.limit {
            return;
        }
        self.bytes += data.len() as u64;
        if let Some(old) = self.entries.put(key, data) {
            self.bytes -= old.len() as u64;
        }
        while self.bytes > self.limit {
            let Some((_, old)) = self.entries.pop_lru() else {
                break;
            };
            self.bytes -= old.len() as u64;
        }
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

    /// Reads and authenticates a block. Missing or corrupt entries are
    /// dropped and reported as a miss.
    fn get(&self, key: &BlockKey, store: &Store, metrics: &Metrics) -> Option<Bytes> {
        if !self.index.lock().entries.contains_key(key) {
            return None;
        }
        let path = self.path(key);
        let sealed = match std::fs::read(&path) {
            Ok(sealed) => sealed,
            Err(error) => {
                tracing::warn!(%error, path = %path.display(), "cached block unreadable; fetching again");
                self.remove(key, store);
                return None;
            }
        };
        let opened = (sealed.len() >= NONCE_LEN)
            .then(|| {
                let (nonce, ciphertext) = sealed.split_at(NONCE_LEN);
                self.cipher
                    .decrypt(
                        XNonce::from_slice(nonce),
                        Payload {
                            msg: ciphertext,
                            aad: &key.encode(),
                        },
                    )
                    .ok()
            })
            .flatten();
        let Some(plain) = opened else {
            tracing::warn!(path = %path.display(), "cached block failed authentication; fetching again");
            Metrics::inc(&metrics.cache_corrupt);
            let _ = std::fs::remove_file(&path);
            self.remove(key, store);
            return None;
        };
        let mut index = self.index.lock();
        if let Some(entry) = index.entries.get_mut(key) {
            entry.last_access = now_secs();
            index.touched.push(key.clone());
        }
        Some(Bytes::from(plain))
    }

    fn put(&self, key: &BlockKey, data: &[u8], store: &Store, metrics: &Metrics) -> Result<()> {
        let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
        let ciphertext = self
            .cipher
            .encrypt(
                &nonce,
                Payload {
                    msg: data,
                    aad: &key.encode(),
                },
            )
            .map_err(|_| CoreError::Internal("block encryption failed".into()))?;
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
            file.write_all(&nonce)?;
            file.write_all(&ciphertext)?;
            std::fs::rename(&temp, &path)
        })();
        if let Err(error) = written {
            let _ = std::fs::remove_file(&temp);
            return Err(error.into());
        }
        let entry = DiskEntry {
            bytes: (NONCE_LEN + ciphertext.len()) as u64,
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
    ram: parking_lot::Mutex<RamCache>,
    disk: DiskCache,
    layouts: parking_lot::Mutex<lru::LruCache<(LinkId, RevisionId), Arc<Layout>>>,
    layout_flights: SingleFlight<(LinkId, RevisionId), Arc<Layout>>,
    block_flights: SingleFlight<BlockKey, Bytes>,
    interactive: Arc<Semaphore>,
    readahead: Arc<Semaphore>,
    prefetch: Arc<Semaphore>,
}

impl BlockCache {
    pub fn open(
        backend: Arc<dyn Backend>,
        store: Arc<Store>,
        metrics: Arc<Metrics>,
        options: CacheOptions,
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
            ram: parking_lot::Mutex::new(RamCache {
                entries: lru::LruCache::unbounded(),
                bytes: 0,
                limit: options.ram_bytes,
            }),
            disk,
            layouts: parking_lot::Mutex::new(lru::LruCache::new(
                NonZeroUsize::new(LAYOUT_CACHE_ENTRIES).expect("non-zero"),
            )),
            layout_flights: SingleFlight::default(),
            block_flights: SingleFlight::default(),
            interactive: Arc::new(Semaphore::new(8)),
            readahead: Arc::new(Semaphore::new(4)),
            prefetch: Arc::new(Semaphore::new(2)),
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
                let rows: Vec<(u32, u32)> = layout.blocks.iter().map(|b| (b.index, b.len)).collect();
                store.write(move |conn| {
                    let mut statement = conn.prepare(
                        "INSERT OR REPLACE INTO revision_blocks(link_id, rev, idx, plain_size) VALUES (?1, ?2, ?3, ?4)",
                    )?;
                    for (index, len) in &rows {
                        statement.execute((&link_owned.0, &rev_owned.0, index, len))?;
                    }
                    Ok(())
                });
                Ok(layout)
            })
            .await?;
        self.layouts
            .lock()
            .put((link.clone(), rev.clone()), Arc::clone(&layout));
        Ok(layout)
    }

    /// A block from RAM or disk; never touches the network.
    pub fn get_local(&self, key: &BlockKey) -> Option<Bytes> {
        if let Some(data) = self.ram.lock().get(key) {
            Metrics::inc(&self.metrics.cache_ram_hits);
            return Some(data);
        }
        let data = self.disk.get(key, &self.store, &self.metrics)?;
        Metrics::inc(&self.metrics.cache_disk_hits);
        self.ram.lock().put(key.clone(), data.clone());
        Some(data)
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
                let _permit = Arc::clone(permits)
                    .acquire_owned()
                    .await
                    .map_err(|_| CoreError::Internal("download permits closed".into()))?;
                // Another flight may have filled it while this one waited.
                if let Some(data) = this.get_local(&owned) {
                    return Ok(data);
                }
                check_not_hot(&this.metrics, "fetch_block");
                Metrics::inc(&this.metrics.cache_misses);
                if priority != Priority::Interactive {
                    Metrics::inc(&this.metrics.prefetched_blocks);
                }
                let data = match this.backend.fetch_block(&owned.link, &owned.rev, owned.index).await {
                    Ok(data) => data,
                    Err(error) => {
                        if matches!(error, pdmount_backend::BackendError::Integrity(_)) {
                            tracing::error!(link = %owned.link, rev = %owned.rev, index = owned.index, %error,
                                "block failed verification; not cached");
                        }
                        return Err(error.into());
                    }
                };
                if data.len() != expected_len as usize {
                    let error = format!(
                        "block {} of {} has {} bytes, expected {expected_len}",
                        owned.index,
                        owned.rev,
                        data.len()
                    );
                    tracing::error!("{error}; not cached");
                    return Err(pdmount_backend::BackendError::Integrity(error).into());
                }
                if let Err(error) = this.disk.put(&owned, &data, &this.store, &this.metrics) {
                    tracing::warn!(%error, "could not write a block to the disk cache");
                }
                this.ram.lock().put(owned, data.clone());
                Ok(data)
            })
            .await
    }

    pub fn flush_access(&self) {
        self.disk.flush_access(&self.store);
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
    }

    #[test]
    fn spans_and_assembly_cover_partial_ranges() {
        let layout = Layout::new(&infos(&[Some(4), Some(4), Some(4)]), 12).expect("layout");
        let spans = layout.spans(3, 6);
        assert_eq!(
            spans.iter().map(|s| s.index).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        let blocks = [
            Bytes::from_static(b"abcd"),
            Bytes::from_static(b"efgh"),
            Bytes::from_static(b"ijkl"),
        ];
        assert_eq!(assemble(spans, &blocks, 3, 6), b"defghi");
        assert!(layout.spans(12, 5).is_empty());
        assert_eq!(layout.spans(4, 4).len(), 1);
    }

    async fn setup(
        dir: &Path,
        disk_bytes: u64,
    ) -> (Arc<BlockCache>, Arc<Store>, LinkId, RevisionId) {
        let backend = FakeBackend::new();
        backend.set_block_size(4);
        let root = backend.root().await.expect("root").link_id;
        let meta = FileMeta {
            mtime: UNIX_EPOCH,
            mode: None,
            media_type: "application/octet-stream".into(),
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
        let store = Arc::new(Store::open(&dir.join("state.db")).expect("store"));
        let cache = BlockCache::open(
            Arc::new(backend),
            Arc::clone(&store),
            Arc::default(),
            CacheOptions {
                dir: dir.join("blocks"),
                master_key: [7; 32],
                ram_bytes: 1 << 20,
                disk_bytes,
            },
        )
        .expect("cache");
        (Arc::new(cache), store, link, rev)
    }

    #[tokio::test]
    async fn blocks_are_fetched_once_then_served_locally() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (cache, _store, link, rev) = setup(dir.path(), 1 << 20).await;
        let layout = cache.layout(&link, &rev, 10).await.expect("layout");
        assert_eq!(layout.blocks.len(), 3);
        let key = BlockKey {
            link: link.clone(),
            rev: rev.clone(),
            index: 2,
        };
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
        assert_eq!(
            cache
                .disk
                .get(&key, &cache.store, &cache.metrics)
                .expect("disk"),
            &b"0123"[..]
        );
        let path = cache.disk.path(&key);
        let mut sealed = std::fs::read(&path).expect("read");
        let last = sealed.len() - 1;
        sealed[last] ^= 1;
        std::fs::write(&path, sealed).expect("write");
        assert!(cache.disk.get(&key, &cache.store, &cache.metrics).is_none());
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn the_disk_cache_evicts_down_to_80_percent() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Each sealed block is 24 + 4 + 16 = 44 bytes; the limit fits two.
        let (cache, _store, link, rev) = setup(dir.path(), 100).await;
        for index in 1..=3 {
            let key = BlockKey {
                link: link.clone(),
                rev: rev.clone(),
                index,
            };
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
}
