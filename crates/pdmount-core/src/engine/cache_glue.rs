//! The engine's side of the block cache: usage, garbage collection, wipe and the orphan sweep.

use super::*;

impl Engine {
    /// Block pool slots in use per class (§6.2).
    pub fn cache_pool_usage(&self) -> Vec<crate::pool::ClassUsage> {
        self.cache.pool_usage()
    }

    /// Opens the disk cache with a key that came after the start (D106);
    /// `false` when it had one already.
    pub fn unlock_cache(&self, master_key: &[u8; 32]) -> bool {
        self.cache.unlock(master_key)
    }

    /// Disk cache bytes and entries.
    pub fn cache_usage(&self) -> (u64, usize) {
        self.cache.usage()
    }

    /// `cache stats`: the disk cache, without the pool slots.
    pub fn cache_stats(&self) -> DiskStats {
        self.cache.disk_stats()
    }

    /// `cache gc`: evicts this tree's blocks down to `target` bytes, by
    /// default as far as takes the profile's total to the lower watermark
    /// (D110 amended). Never pinned or protected blocks.
    pub async fn cache_gc(&self, target: Option<u64>) -> Result<Evicted> {
        let cache = Arc::clone(&self.cache);
        let target = target.unwrap_or_else(|| cache.gc_default_target());
        tokio::task::spawn_blocking(move || cache.gc(target))
            .await
            .map_err(|error| CoreError::Internal(format!("cache gc: {error}")))
    }

    /// `cache wipe [<path>]`: evicts every block that may go, or with a path
    /// only those of files at or below it.
    pub async fn cache_wipe(self: &Arc<Self>, path: Option<&str>) -> Result<Evicted> {
        let links = match path {
            None => None,
            Some(path) => {
                let top = self.resolve(path).await?;
                let tree = self.tree.read();
                let links: HashSet<LinkId> = tree
                    .subtree(top)
                    .filter_map(|ino| tree.get(ino))
                    .filter(|node| !node.is_dir())
                    .filter_map(|node| node.link().cloned())
                    .collect();
                Some(links)
            }
        };
        let cache = Arc::clone(&self.cache);
        tokio::task::spawn_blocking(move || cache.wipe(links.as_ref()))
            .await
            .map_err(|error| CoreError::Internal(format!("cache wipe: {error}")))
    }

    /// Clears what a crash left in the disk cache; once per mount (§6.2).
    pub(super) async fn sweep_cache(&self) {
        if let Err(error) = self.store.flush().await {
            tracing::warn!(%error, "cache sweep skipped: the store did not commit");
            return;
        }
        let cache = Arc::clone(&self.cache);
        match tokio::task::spawn_blocking(move || cache.sweep()).await {
            Ok(Ok(swept)) => tracing::info!(
                orphan_files = swept.orphan_files,
                dangling_rows = swept.dangling_rows,
                "cache sweep finished"
            ),
            Ok(Err(error)) => tracing::warn!(%error, "cache sweep failed"),
            Err(error) => tracing::warn!(%error, "cache sweep did not finish"),
        }
        // Pinned blocks may have been among the dangling rows.
        self.pins_changed();
    }
}
