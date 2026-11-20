//! Sequential readahead (DESIGN.md §5.3, D135): the per-handle detector and
//! the background fetches, for the engine's trees and the Photos tree.
//!
//! Readahead starts only once a handle's reads, each starting where the one
//! before ended, cross into the next block. Reads within one block say
//! nothing yet: a video player reads the file's head, then jumps to the
//! index at its end, and the kernel's 128 KiB reads make two adjacent reads
//! at once; readahead started there would compete with that jump. A copy
//! crosses its first block boundary after one block, and the crossing read
//! starts the readahead before it waits for its own block.

use crate::cache::{BlockCache, BlockKey, Layout, Priority};
use pdmount_backend::{LinkId, RevisionId};
use std::sync::Arc;

/// Blocks fetched ahead once a run crossed one block boundary.
const FIRST_AHEAD: usize = 2;
/// Blocks fetched ahead once it crossed two or more.
const RUN_AHEAD: usize = 4;

/// A handle's reads in order (§5.3).
#[derive(Debug, Clone, Copy, Default)]
pub struct Sequential {
    /// Where the last read ended.
    last_end: u64,
    /// Where the current run of reads in order began.
    run_start: u64,
}

impl Sequential {
    /// Records a read of `len` bytes at `offset`; answers how many blocks to
    /// fetch after the read's last block: none until a run of reads in order
    /// crosses into another block, then [`FIRST_AHEAD`], then [`RUN_AHEAD`]
    /// once it crossed two.
    pub fn record(&mut self, layout: &Layout, offset: u64, len: u64) -> usize {
        let in_order = offset > 0 && offset == self.last_end;
        if !in_order {
            self.run_start = offset;
        }
        self.last_end = offset.saturating_add(len);
        if !in_order {
            return 0;
        }
        let first = layout
            .spans(self.run_start, 1)
            .first()
            .map(|span| span.index);
        let last = layout.spans(offset, len).last().map(|span| span.index);
        match (first, last) {
            (Some(first), Some(last)) if last >= first + 2 => RUN_AHEAD,
            (Some(first), Some(last)) if last > first => FIRST_AHEAD,
            _ => 0,
        }
    }
}

/// Fetches the `ahead` blocks after the last block of the read at
/// `[offset, offset + len)` that `cache` does not hold, in the background
/// at readahead priority (the session's readahead permits, D033, D110).
pub(crate) fn fetch_ahead(
    cache: &Arc<BlockCache>,
    runtime: &tokio::runtime::Handle,
    link: &LinkId,
    rev: &RevisionId,
    layout: &Layout,
    (offset, len): (u64, u64),
    ahead: usize,
) {
    let Some(mut index) = layout.spans(offset, len).last().map(|span| span.index) else {
        return;
    };
    for _ in 0..ahead {
        let Some(next) = layout.next_after(index) else {
            break;
        };
        index = next.index;
        let key = BlockKey::new(link, rev, next.index);
        if cache.contains(&key) || cache.in_flight(&key) {
            continue;
        }
        let cache = Arc::clone(cache);
        let len = next.len;
        runtime.spawn(async move {
            if let Err(error) = cache.get(&key, len, Priority::Readahead).await {
                tracing::debug!(%error, "readahead failed");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::BlockSpan;

    const MIB: u64 = 1 << 20;

    /// Four blocks of 4 MiB.
    fn layout() -> Layout {
        Layout {
            blocks: (0..4)
                .map(|i| BlockSpan {
                    index: i + 1,
                    offset: u64::from(i) * 4 * MIB,
                    len: 4 << 20,
                })
                .collect(),
        }
    }

    /// 128 KiB reads in order: nothing within the first block, then two
    /// ahead at the crossing, four once a second boundary is crossed.
    #[test]
    fn readahead_waits_for_reads_in_order_to_cross_a_block() {
        let layout = layout();
        let mut seq = Sequential::default();
        let step = 128 << 10;
        let mut offset = 0;
        while offset + step <= 4 * MIB {
            assert_eq!(seq.record(&layout, offset, step), 0, "at {offset}");
            offset += step;
        }
        assert_eq!(seq.record(&layout, offset, step), FIRST_AHEAD);
        offset += step;
        while offset + step <= 8 * MIB {
            assert_eq!(seq.record(&layout, offset, step), FIRST_AHEAD);
            offset += step;
        }
        assert_eq!(seq.record(&layout, offset, step), RUN_AHEAD);
    }

    /// A player: the head, a jump to the end, then the head again; no
    /// readahead until its reads in order cross a block.
    #[test]
    fn a_jump_starts_a_new_run() {
        let layout = layout();
        let mut seq = Sequential::default();
        let step = 128 << 10;
        assert_eq!(seq.record(&layout, 0, step), 0);
        assert_eq!(seq.record(&layout, step, step), 0);
        assert_eq!(seq.record(&layout, 16 * MIB - step, step), 0);
        assert_eq!(seq.record(&layout, 2 * step, step), 0);
        // A run that starts near a boundary crosses it soon.
        assert_eq!(seq.record(&layout, 4 * MIB - step, step), 0);
        assert_eq!(seq.record(&layout, 4 * MIB, step), FIRST_AHEAD);
    }

    /// One read that spans a boundary is not a run.
    #[test]
    fn a_single_read_across_a_boundary_is_not_a_run() {
        let layout = layout();
        let mut seq = Sequential::default();
        assert_eq!(seq.record(&layout, 4 * MIB - 4096, 8192), 0);
    }
}
