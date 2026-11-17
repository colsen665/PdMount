//! The encrypted disk cache: its index, reads, writes, eviction, wipe and the orphan sweep.

use super::*;

#[derive(Debug, Clone, Copy)]
pub(super) struct DiskEntry {
    pub(super) bytes: u64,
    pub(super) last_access: i64,
    /// Never evicted, and not counted against the cap (§6.2).
    pub(super) pinned: bool,
}

#[derive(Default)]
pub(super) struct DiskIndex {
    /// Ordered, so the blocks of a revision or a file are a range.
    pub(super) entries: BTreeMap<BlockKey, DiskEntry>,
    /// The unpinned entries, oldest first.
    pub(super) by_age: BTreeSet<(i64, BlockKey)>,
    /// File sizes of the unpinned entries; the watermarks apply to these.
    pub(super) bytes: u64,
    /// `bytes`, for the [`DiskBudget`] to read without this lock.
    pub(super) shown: Arc<AtomicU64>,
    pub(super) pinned_bytes: u64,
    /// Revisions whose blocks are pinned, also those still to arrive.
    pub(super) pinned_revs: HashSet<(LinkId, RevisionId)>,
    /// Entries whose `last_access` changed since the last flush.
    pub(super) touched: HashSet<BlockKey>,
}

impl DiskIndex {
    pub(super) fn insert(&mut self, key: BlockKey, entry: DiskEntry) {
        self.remove(&key);
        if entry.pinned {
            self.pinned_bytes += entry.bytes;
        } else {
            self.bytes += entry.bytes;
            self.shown.store(self.bytes, Ordering::Relaxed);
            self.by_age.insert((entry.last_access, key.clone()));
        }
        self.entries.insert(key, entry);
    }

    pub(super) fn remove(&mut self, key: &BlockKey) -> Option<DiskEntry> {
        let entry = self.entries.remove(key)?;
        if entry.pinned {
            self.pinned_bytes -= entry.bytes;
        } else {
            self.bytes -= entry.bytes;
            self.shown.store(self.bytes, Ordering::Relaxed);
            self.by_age.remove(&(entry.last_access, key.clone()));
        }
        Some(entry)
    }

    /// Marks an entry used at `now`, at most once per second per entry.
    pub(super) fn touch(&mut self, key: &BlockKey, now: i64) {
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
    /// This cache's unpinned bytes; pinned blocks are apart.
    pub bytes: u64,
    /// The cap of the [`DiskBudget`]: the profile's `cache.max_bytes`, for
    /// every tree together.
    pub max_bytes: u64,
    /// The unpinned bytes of every cache on the budget, this one included;
    /// the cap applies to these.
    pub profile_bytes: u64,
    /// Caches on the budget, this one included.
    pub caches: u64,
    pub blocks: u64,
    pub pinned_bytes: u64,
    pub protected_revisions: u64,
}

/// A file this much older than the start of a sweep was not just being
/// written: `put` renames it into place before its row is queued.
pub(super) const SWEEP_GRACE: Duration = Duration::from_secs(2);

/// Encrypted on-disk block cache: `blocks/xx/<blake3(key)>` holding
/// nonce ‖ XChaCha20-Poly1305(block), with the key as associated data.
///
/// The cipher can come later than the index (D106): a daemon that starts
/// before the keyring answers opens the index, so eviction, pins and `cache`
/// commands work, but reads no block and writes none until
/// [`BlockCache::unlock`]. Nothing is deleted for being unreadable meanwhile.
pub(super) struct DiskCache {
    pub(super) dir: PathBuf,
    pub(super) cipher: std::sync::OnceLock<XChaCha20Poly1305>,
    pub(super) index: parking_lot::Mutex<DiskIndex>,
    /// Revisions eviction skips, whoever asks for it.
    pub(super) protections: Protections,
    /// The tree's place on the session's bulk-read guard (D111), set once
    /// by the engine: blocks downloaded by scan reads are reported there,
    /// and blocks that leave the disk are forgotten there (D120).
    pub(super) scan_seat: std::sync::OnceLock<crate::scan::ScanSeat>,
}

pub(super) fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

impl DiskCache {
    pub(super) fn open(
        dir: &Path,
        master_key: Option<&[u8; 32]>,
        store: &Store,
        protections: Protections,
    ) -> Result<Self> {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
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
        let cache = Self {
            dir: dir.to_owned(),
            cipher: std::sync::OnceLock::new(),
            index: parking_lot::Mutex::new(index),
            protections,
            scan_seat: std::sync::OnceLock::new(),
        };
        if let Some(master_key) = master_key {
            cache.unlock(master_key);
        }
        Ok(cache)
    }

    /// Sets the key; `false` when one was set before (it stays).
    pub(super) fn unlock(&self, master_key: &[u8; 32]) -> bool {
        let key = blake3::derive_key("pdmount block cache v1", master_key);
        self.cipher.set(XChaCha20Poly1305::new(&key.into())).is_ok()
    }

    pub(super) fn unlocked(&self) -> bool {
        self.cipher.get().is_some()
    }

    pub(super) fn path(&self, key: &BlockKey) -> PathBuf {
        let hash = blake3::hash(&key.encode()).to_hex();
        self.dir.join(&hash[..2]).join(hash.as_str())
    }

    /// Reads and authenticates a block into a buffer from `buffer` (called
    /// with the block's length); the plaintext is at [`PLAIN_START`].
    /// Missing or corrupt entries are dropped and reported as a miss.
    pub(super) fn read(
        &self,
        key: &BlockKey,
        buffer: impl FnOnce(usize) -> Buffer,
        store: &Store,
        metrics: &Metrics,
    ) -> Option<(Buffer, usize)> {
        // Without the key a block is a miss, never corrupt (D106).
        if !self.unlocked() || !self.index.lock().entries.contains_key(key) {
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

    pub(super) fn touch(&self, key: &BlockKey) {
        self.index.lock().touch(key, now_secs());
    }

    /// Encrypts `sealed` (nonce space ‖ plaintext ‖ tag space) in place.
    pub(super) fn seal(&self, key: &BlockKey, sealed: &mut [u8]) -> Result<()> {
        let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
        let (head, tag) = sealed.split_at_mut(sealed.len() - TAG_LEN);
        let (nonce_out, text) = head.split_at_mut(NONCE_LEN);
        nonce_out.copy_from_slice(&nonce);
        let cipher = self
            .cipher
            .get()
            .ok_or_else(|| CoreError::Internal("the block cache has no key yet".into()))?;
        let computed = cipher
            .encrypt_in_place_detached(&nonce, &key.encode(), text)
            .map_err(|_| CoreError::Internal("block encryption failed".into()))?;
        tag.copy_from_slice(&computed);
        Ok(())
    }

    /// Authenticates and decrypts `sealed` in place; `false` if it fails.
    pub(super) fn unseal(&self, key: &BlockKey, sealed: &mut [u8]) -> bool {
        let Some(text_end) = sealed
            .len()
            .checked_sub(TAG_LEN)
            .filter(|&end| end >= NONCE_LEN)
        else {
            return false;
        };
        let (head, tag) = sealed.split_at_mut(text_end);
        let (nonce, text) = head.split_at_mut(NONCE_LEN);
        let Some(cipher) = self.cipher.get() else {
            return false;
        };
        cipher
            .decrypt_in_place_detached(
                XNonce::from_slice(nonce),
                &key.encode(),
                text,
                Tag::from_slice(tag),
            )
            .is_ok()
    }

    /// Stores a block sealed by [`DiskCache::seal`]; the caller then
    /// enforces the cap ([`BlockCache::enforce_cap`]).
    pub(super) fn put(&self, key: &BlockKey, sealed: &[u8], store: &Store) -> Result<()> {
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
        Ok(())
    }

    /// Pins a revision's blocks, present and future, or lets them go; let
    /// go, they are evictable by their last use.
    pub(super) fn set_pinned(&self, link: &LinkId, rev: &RevisionId, pinned: bool, store: &Store) {
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
        let keys: Vec<BlockKey> = changing.iter().map(|(key, _)| key.clone()).collect();
        for (key, entry) in changing {
            index.insert(key, DiskEntry { pinned, ..entry });
        }
        drop(index);
        if pinned {
            // Pinned blocks are outside the cap, and so outside the scan
            // share.
            self.forget_scan_blocks(&keys);
        }
        let (link, rev) = (link.0.clone(), rev.0.clone());
        store.write(move |conn| {
            conn.prepare_cached("UPDATE cache_blocks SET pin = ?3 WHERE link_id = ?1 AND rev = ?2")?
                .execute((&link, &rev, pinned))
                .map(drop)
        });
    }

    /// Evicts the least recently used entries down to `target` bytes, or as
    /// far as protected revisions allow, and never below `floor` bytes.
    pub(super) fn evict_to(
        &self,
        target: u64,
        floor: u64,
        store: &Store,
        metrics: &Metrics,
    ) -> Evicted {
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
                let bytes = index.entries.get(key).map_or(0, |entry| entry.bytes);
                if left - bytes < floor {
                    break;
                }
                left -= bytes;
                victims.push(key.clone());
            }
            let evicted = take_entries(&mut index, &victims);
            (victims, evicted)
        };
        self.drop_files(victims, evicted, store, metrics)
    }

    /// Evicts `key` unless it is pinned or protected.
    pub(super) fn evict_one(&self, key: &BlockKey, store: &Store, metrics: &Metrics) -> Evicted {
        let (victims, evicted) = {
            let mut index = self.index.lock();
            let protected = self.protections.lock();
            if !index
                .entries
                .get(key)
                .is_some_and(|entry| !entry.pinned && !is_protected(&protected, key))
            {
                return Evicted::default();
            }
            let victims = vec![key.clone()];
            let evicted = take_entries(&mut index, &victims);
            (victims, evicted)
        };
        self.drop_files(victims, evicted, store, metrics)
    }

    /// Evicts every unprotected, unpinned entry; with `links`, only theirs.
    pub(super) fn wipe(
        &self,
        links: Option<&HashSet<LinkId>>,
        store: &Store,
        metrics: &Metrics,
    ) -> Evicted {
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
    pub(super) fn drop_files(
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
        self.forget_scan_blocks(&victims);
        delete_rows(store, victims);
        evicted
    }

    pub(super) fn remove(&self, key: &BlockKey, store: &Store) {
        self.index.lock().remove(key);
        self.forget_scan_blocks(std::slice::from_ref(key));
        delete_rows(store, vec![key.clone()]);
    }

    /// Blocks gone from this cache by any path (the cap, `cache gc`, a
    /// wipe, a corrupt file, a pin) are no scan blocks any more (D120).
    /// Called with no lock of the cache held.
    fn forget_scan_blocks(&self, keys: &[BlockKey]) {
        if let Some(seat) = self.scan_seat.get().filter(|seat| seat.any_blocks()) {
            seat.forget(keys);
        }
    }

    /// Removes files without a row and rows without a file, one directory
    /// at a time, without the index mutex during the walk. Rows come from
    /// the store (flushed by the caller): files written since the sweep
    /// began may have no row yet, so they are left alone.
    pub(super) fn sweep(&self, store: &Store) -> Result<Swept> {
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
    pub(super) fn flush_access(&self, store: &Store) {
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

    pub(super) fn usage(&self) -> (u64, usize) {
        let index = self.index.lock();
        (index.bytes, index.entries.len())
    }

    pub(super) fn stats(&self, budget: &DiskBudget) -> DiskStats {
        let (bytes, blocks, pinned_bytes) = {
            let index = self.index.lock();
            (index.bytes, index.entries.len() as u64, index.pinned_bytes)
        };
        DiskStats {
            bytes,
            max_bytes: budget.cap(),
            profile_bytes: budget.used(),
            caches: budget.caches() as u64,
            blocks,
            pinned_bytes,
            protected_revisions: self.protections.len() as u64,
        }
    }
}

/// Takes `victims` out of the index, under the lock that chose them: a pin or
/// protection set from here on finds them already gone, not about to go.
pub(super) fn take_entries(index: &mut DiskIndex, victims: &[BlockKey]) -> Evicted {
    let mut evicted = Evicted::default();
    for key in victims {
        if let Some(entry) = index.remove(key) {
            evicted.blocks += 1;
            evicted.bytes += entry.bytes;
        }
    }
    evicted
}

pub(super) fn is_protected(protected: &HashMap<(LinkId, RevisionId), u32>, key: &BlockKey) -> bool {
    !protected.is_empty() && protected.contains_key(&(key.link.clone(), key.rev.clone()))
}

pub(super) fn delete_rows(store: &Store, keys: Vec<BlockKey>) {
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
