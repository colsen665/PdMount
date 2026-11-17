//! The RAM LRU of cached blocks.

use super::*;

pub(super) struct Cached {
    /// The plaintext, inside a pool slot.
    pub(super) data: Bytes,
    pub(super) class: usize,
    pub(super) slot_size: usize,
}

/// Blocks in pool slots, least recently used first out. Its size is bounded
/// by the pool, so it never evicts on its own: taking a slot does.
pub(super) struct RamCache {
    pub(super) entries: lru::LruCache<BlockKey, Cached>,
    /// Slot sizes of the entries, not block lengths.
    pub(super) bytes: u64,
}

impl RamCache {
    pub(super) fn get(&mut self, key: &BlockKey) -> Option<Bytes> {
        self.entries.get(key).map(|cached| cached.data.clone())
    }

    /// Returns a replaced entry, to be dropped outside the lock.
    pub(super) fn put(&mut self, key: BlockKey, cached: Cached) -> Option<Bytes> {
        self.bytes += cached.slot_size as u64;
        let old = self.entries.put(key, cached)?;
        self.bytes -= old.slot_size as u64;
        Some(old.data)
    }

    /// Removes an entry. Its slot is free once the returned `Bytes` and every
    /// clone of them are dropped.
    pub(super) fn remove(&mut self, key: &BlockKey) -> Option<Bytes> {
        let cached = self.entries.pop(key)?;
        self.bytes -= cached.slot_size as u64;
        Some(cached.data)
    }

    /// Removes the least recently used entry of `class`. Its slot is free
    /// once the returned `Bytes` and every clone of them are dropped.
    pub(super) fn evict(&mut self, class: usize) -> Option<Bytes> {
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
