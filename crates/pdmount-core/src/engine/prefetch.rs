//! Readahead and directory prefetch.

use super::*;

impl Engine {
    /// For a read of remote content: counts its bytes on the handle and,
    /// unless it is a scan read (D111), takes the blocks it uses out of the
    /// scan blocks and runs sequential readahead (§5.3, D135): once the
    /// handle's reads in order cross into the next block, fetch the next
    /// blocks (2, then 4 once a second block is crossed) in the background.
    /// Scan reads get no readahead.
    pub(super) fn readahead(
        self: &Arc<Self>,
        handle: u64,
        link: &LinkId,
        rev: &RevisionId,
        layout: &Arc<Layout>,
        offset: u64,
        len: u64,
    ) {
        let (ahead, scan) = {
            let mut files = self.files.lock();
            let Some(open) = files.get_mut(&handle) else {
                return;
            };
            let scan = self.is_scan_read(open);
            open.read_bytes = open.read_bytes.saturating_add(len);
            (open.sequential.record(layout, offset, len), scan)
        };
        if scan {
            return;
        }
        self.scan_used(link, rev, layout, offset, len);
        if ahead > 0 {
            crate::readahead::fetch_ahead(
                &self.cache,
                &self.runtime,
                link,
                rev,
                layout,
                (offset, len),
                ahead,
            );
        }
    }

    /// Listing and small-file prefetch (§5.3) for a directory the user
    /// opened. Runs once per directory per mount.
    pub(super) fn prefetch_dir(self: &Arc<Self>, dir: Ino) {
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

    pub(super) async fn prefetch_file(
        &self,
        link: &LinkId,
        rev: &RevisionId,
        size: u64,
    ) -> Result<()> {
        let layout = self.cache.layout(link, rev, size).await?;
        for span in &layout.blocks {
            let key = BlockKey::new(link, rev, span.index);
            self.cache.get(&key, span.len, Priority::Prefetch).await?;
        }
        Ok(())
    }

    /// Lists subdirectories to depth 2, at most 32 listings.
    pub(super) async fn prefetch_listings(self: Arc<Self>, subdirs: Vec<Ino>) {
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
