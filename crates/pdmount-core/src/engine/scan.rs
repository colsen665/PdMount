//! The engine's side of the bulk-read guard ([`crate::scan`], DESIGN.md
//! §6.4, D081, D111): classing opens and reads, marking the requests a user
//! waits on, and the turns of scan reads.

use super::*;
use crate::gate::Turn;
use crate::scan::UserWait;

impl Engine {
    /// The session's bulk-read guard, for the further trees (D111).
    pub fn scan_guard(&self) -> &Arc<ScanGuard> {
        &self.scan
    }

    /// Whether a file opened with `mode` opens as part of a scan: only
    /// reads count, and only opens a program asked for.
    pub(super) fn opens_as_scan(&self, ino: Ino, mode: OpenMode) -> bool {
        if !mode.read || mode.write {
            return false;
        }
        let opened = self.scan.opened(self.scan_tree, ino);
        if opened.began {
            Metrics::inc(&self.metrics.scans);
            tracing::debug!(ino, "many files opened at once: a scan begins");
        }
        opened.scan
    }

    /// Marks a request a user waits on (a read, a listing, a write that
    /// fetches), for the session's background gate and scan reads.
    pub(crate) fn user_wait(&self) -> UserWait<'_> {
        self.scan.user_wait(&self.gate)
    }

    /// Whether the next read through `file` is a scan read: the file was
    /// opened during a scan and fewer than `scan.first_bytes` were read.
    pub(super) fn is_scan_read(&self, file: &OpenFile) -> bool {
        file.scan && file.read_bytes < self.scan.first_bytes()
    }

    /// A scan read's turn (D111), when it needs the network: no turn for a
    /// read whose layout and blocks are here or already being downloaded.
    /// Waits at most a third of the block timeout, so the guard slows a
    /// scan but never makes it time out.
    pub(super) async fn scan_turn(
        &self,
        link: &LinkId,
        rev: &RevisionId,
        size: u64,
        offset: u64,
        len: u64,
    ) -> Option<Turn<'_>> {
        let local = self
            .cache
            .layout_local(link, rev, size)
            .is_some_and(|layout| {
                layout.spans(offset, len).iter().all(|span| {
                    let key = BlockKey::new(link, rev, span.index);
                    self.cache.contains(&key) || self.cache.in_flight(&key)
                })
            });
        if local {
            return None;
        }
        self.scan.turn(self.config.timeouts.block.0 / 3).await
    }

    /// An interactive read used these blocks of `layout`: none of them is a
    /// scan block any more.
    pub(super) fn scan_used(
        &self,
        link: &LinkId,
        rev: &RevisionId,
        layout: &Layout,
        offset: u64,
        len: u64,
    ) {
        if self.cache.scan_seat().is_some_and(|seat| seat.any_blocks()) {
            let keys: Vec<BlockKey> = layout
                .spans(offset, len)
                .iter()
                .map(|span| BlockKey::new(link, rev, span.index))
                .collect();
            self.cache.scan_used(&keys);
        }
    }
}
