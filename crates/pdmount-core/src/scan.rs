//! The bulk-read guard (DESIGN.md §6.4, D081, D111).
//!
//! A thumbnailer, a recursive `grep` or a content-type sniffer opens many
//! different files in quick succession and reads the start of each. Read
//! as interactive, those reads take every download slot, readahead and the
//! block cache. One [`ScanGuard`] per session sees the opens of every tree:
//!
//! - **Detection:** a scan begins when `scan.opens` different files were
//!   opened within `scan.window`, and ends at an open after which at most
//!   one file was opened within the window. A file is classed when it is
//!   opened: one opened while no scan runs stays interactive.
//! - **Scan reads:** the first `scan.first_bytes` read through a handle
//!   opened during a scan. Their downloads take a turn of the guard's
//!   [`Gate`]: one at a time over the session, starts `scan.interval`
//!   apart, none while an interactive read or listing waits, and no turn
//!   longer than a third of the block timeout. No readahead.
//! - **Cache share:** blocks those reads download are tracked here; above
//!   `scan.cache_share` percent of the disk cap, the oldest of them are
//!   evicted, from whichever tree holds them. A block an interactive read
//!   uses, or that another read downloads again, leaves the scan blocks, and
//!   so does one that leaves the disk by another path (D120).
//!
//! Locks: `detector`, `blocks` and `members` are leaves, never held while
//! calling a cache or each other; an eviction runs with none held.

use crate::cache::BlockKey;
use crate::config::ScanConfig;
use crate::gate::{Foreground, Gate, Turn};
use crate::vfs::Ino;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Evicts one block of a tree's cache (unless pinned or protected).
pub type EvictBlock = Arc<dyn Fn(&BlockKey) + Send + Sync>;

pub struct ScanGuard {
    opens: usize,
    window: Duration,
    first_bytes: u64,
    /// Bytes of scan blocks the disk caches may hold together.
    limit: u64,
    detector: parking_lot::Mutex<Detector>,
    gate: Gate,
    blocks: parking_lot::Mutex<ScanBlocks>,
    /// `blocks`' length, read without the lock on the hot path.
    count: AtomicUsize,
    members: parking_lot::Mutex<HashMap<u64, EvictBlock>>,
    next_seat: AtomicU64,
}

/// The most recently opened different files, oldest first, at most
/// `opens` of them: enough to tell whether `opens` fell in the window.
#[derive(Default)]
struct Detector {
    recent: VecDeque<((u64, Ino), Instant)>,
    scanning: bool,
}

/// Scan blocks by age, with their sizes on disk.
struct ScanBlocks {
    order: lru::LruCache<(u64, BlockKey), u64>,
    bytes: u64,
}

/// What an open was classed as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Opened {
    /// The file opens during a scan.
    pub scan: bool,
    /// This open began the scan.
    pub began: bool,
}

impl ScanGuard {
    /// The guard of a session whose disk caches share `cache_cap` bytes.
    pub fn new(config: &ScanConfig, cache_cap: u64) -> Arc<Self> {
        Arc::new(Self {
            opens: config.opens as usize,
            window: config.window.0,
            first_bytes: config.first_bytes.0,
            limit: cache_cap / 100 * u64::from(config.cache_share.min(100)),
            detector: parking_lot::Mutex::default(),
            gate: Gate::new(config.interval.0),
            blocks: parking_lot::Mutex::new(ScanBlocks {
                order: lru::LruCache::unbounded(),
                bytes: 0,
            }),
            count: AtomicUsize::new(0),
            members: parking_lot::Mutex::default(),
            next_seat: AtomicU64::new(1),
        })
    }

    /// Notes that tree `tree` opened file `ino` for reading now.
    pub fn opened(&self, tree: u64, ino: Ino) -> Opened {
        self.opened_at(tree, ino, Instant::now())
    }

    pub(crate) fn opened_at(&self, tree: u64, ino: Ino, now: Instant) -> Opened {
        if self.opens == 0 {
            return Opened {
                scan: false,
                began: false,
            };
        }
        let mut detector = self.detector.lock();
        let file = (tree, ino);
        if let Some(at) = detector.recent.iter().position(|(seen, _)| *seen == file) {
            detector.recent.remove(at);
        }
        detector.recent.push_back((file, now));
        if detector.recent.len() > self.opens {
            detector.recent.pop_front();
        }
        let since = now.checked_sub(self.window);
        let in_window = detector
            .recent
            .iter()
            .filter(|(_, at)| since.is_none_or(|since| *at >= since))
            .count();
        let was = detector.scanning;
        if in_window >= self.opens {
            detector.scanning = true;
        } else if in_window <= 1 {
            detector.scanning = false;
        }
        Opened {
            scan: detector.scanning,
            began: detector.scanning && !was,
        }
    }

    /// Whether a scan runs (as of the last open).
    pub fn scanning(&self) -> bool {
        self.detector.lock().scanning
    }

    /// `scan.first_bytes`.
    pub fn first_bytes(&self) -> u64 {
        self.first_bytes
    }

    /// The scan-block bytes the caches may hold together.
    pub fn limit(&self) -> u64 {
        self.limit
    }

    /// Bytes of scan blocks held now, over all trees.
    pub fn held(&self) -> u64 {
        self.blocks.lock().bytes
    }

    /// Marks an interactive request in flight: scan downloads wait for it.
    pub fn foreground(&self) -> Foreground<'_> {
        self.gate.foreground()
    }

    /// Marks a request a user waits on (a read, a listing, a write that
    /// fetches) in any tree: the session's background turns (`background`,
    /// [`crate::limits::SessionLimits`], D027) and scan reads wait for it.
    pub(crate) fn user_wait<'a>(&'a self, background: &'a Gate) -> UserWait<'a> {
        UserWait {
            _background: background.foreground(),
            _scan: self.gate.foreground(),
        }
    }

    /// A scan download's turn: waits for the one before it, the spacing and
    /// for interactive requests to end, at most `patience` (then `None` or
    /// a turn anyway, [`Gate::background_within`]).
    pub async fn turn(&self, patience: Duration) -> Option<Turn<'_>> {
        self.gate.background_within(patience).await
    }

    /// A tree's place: its id for [`ScanGuard::opened`] and the eviction
    /// of its blocks.
    pub fn join(self: &Arc<Self>, evict: EvictBlock) -> ScanSeat {
        let id = self.next_seat.fetch_add(1, Ordering::Relaxed);
        self.members.lock().insert(id, evict);
        ScanSeat {
            guard: Arc::clone(self),
            id,
        }
    }

    /// Takes the oldest scan blocks off until they fit the limit.
    fn over_limit(&self, blocks: &mut ScanBlocks) -> Vec<(u64, BlockKey)> {
        let mut victims = Vec::new();
        while blocks.bytes > self.limit {
            let Some((key, bytes)) = blocks.order.pop_lru() else {
                break;
            };
            blocks.bytes -= bytes;
            victims.push(key);
        }
        self.count.store(blocks.order.len(), Ordering::Relaxed);
        victims
    }

    fn evict(&self, victims: Vec<(u64, BlockKey)>) {
        for (seat, key) in victims {
            let evict = self.members.lock().get(&seat).cloned();
            if let Some(evict) = evict {
                evict(&key);
            }
        }
    }
}

/// A request a user waits on ([`ScanGuard::user_wait`]): background work
/// and scan reads of the session wait for it to end.
pub(crate) struct UserWait<'a> {
    _background: Foreground<'a>,
    _scan: Foreground<'a>,
}

/// A tree's place on the [`ScanGuard`]; it leaves, with its scan blocks
/// forgotten, when dropped.
pub struct ScanSeat {
    guard: Arc<ScanGuard>,
    id: u64,
}

impl ScanSeat {
    pub fn guard(&self) -> &Arc<ScanGuard> {
        &self.guard
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    /// A scan read stored `key` on disk, `bytes` long: the oldest scan
    /// blocks beyond the share are evicted (this one too if it alone is
    /// over).
    pub fn stored(&self, key: &BlockKey, bytes: u64) {
        let victims = {
            let mut blocks = self.guard.blocks.lock();
            if let Some(old) = blocks.order.put((self.id, key.clone()), bytes) {
                blocks.bytes -= old;
            }
            blocks.bytes += bytes;
            self.guard.over_limit(&mut blocks)
        };
        self.guard.evict(victims);
    }

    /// These blocks are no scan blocks any more: an interactive read used
    /// them or another read downloaded them again (the working set now), or
    /// they left the tree's disk cache by another path than the share's
    /// eviction (the cap, `cache gc`, a wipe, a pin; D120).
    pub fn forget<'a>(&self, keys: impl IntoIterator<Item = &'a BlockKey>) {
        if self.guard.count.load(Ordering::Relaxed) == 0 {
            return;
        }
        let mut blocks = self.guard.blocks.lock();
        for key in keys {
            if let Some(bytes) = blocks.order.pop(&(self.id, key.clone())) {
                blocks.bytes -= bytes;
            }
        }
        self.guard
            .count
            .store(blocks.order.len(), Ordering::Relaxed);
    }

    /// Whether any scan block is tracked (cheap; for the hot path).
    pub fn any_blocks(&self) -> bool {
        self.guard.count.load(Ordering::Relaxed) > 0
    }
}

impl Drop for ScanSeat {
    fn drop(&mut self) {
        self.guard.members.lock().remove(&self.id);
        let mut blocks = self.guard.blocks.lock();
        let mine: Vec<(u64, BlockKey)> = blocks
            .order
            .iter()
            .filter(|((seat, _), _)| *seat == self.id)
            .map(|(key, _)| key.clone())
            .collect();
        for key in mine {
            if let Some(bytes) = blocks.order.pop(&key) {
                blocks.bytes -= bytes;
            }
        }
        self.guard
            .count
            .store(blocks.order.len(), Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::units::{ByteSize, HumanDuration};
    use pdmount_backend::{LinkId, RevisionId};

    fn guard(opens: u32, share: u8, cap: u64) -> Arc<ScanGuard> {
        ScanGuard::new(
            &ScanConfig {
                opens,
                window: HumanDuration(Duration::from_secs(5)),
                first_bytes: ByteSize(8 << 20),
                interval: HumanDuration(Duration::from_millis(100)),
                cache_share: share,
            },
            cap,
        )
    }

    fn key(n: u32) -> BlockKey {
        BlockKey::new(&LinkId(format!("l{n}")), &RevisionId("r".into()), 1)
    }

    #[test]
    fn many_different_files_opened_quickly_begin_a_scan() {
        let guard = guard(10, 10, 1 << 30);
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        for ino in 1..10 {
            let opened = guard.opened_at(1, ino, at(ino * 100));
            assert!(!opened.scan, "open {ino} of 9 is interactive");
        }
        let tenth = guard.opened_at(1, 10, at(1000));
        assert_eq!(
            tenth,
            Opened {
                scan: true,
                began: true
            }
        );
        // It goes on while files keep coming, even slowly (paced).
        for ino in 11..20 {
            let opened = guard.opened_at(1, ino, at(1000 + (ino - 10) * 1000));
            assert!(
                opened.scan && !opened.began,
                "open {ino} continues the scan"
            );
        }
    }

    #[test]
    fn a_single_open_stays_interactive_and_ends_a_finished_scan() {
        let guard = guard(10, 10, 1 << 30);
        let start = Instant::now();
        assert!(!guard.opened_at(1, 1, start).scan, "a lone open");
        for ino in 2..=10 {
            guard.opened_at(1, ino, start + Duration::from_millis(10 * ino));
        }
        assert!(guard.scanning());
        // A window later, one photo opened alone: interactive.
        let photo = guard.opened_at(1, 99, start + Duration::from_secs(7));
        assert!(!photo.scan);
        assert!(!guard.scanning());
    }

    #[test]
    fn the_same_file_opened_again_counts_once_and_trees_count_together() {
        let guard = guard(4, 10, 1 << 30);
        let now = Instant::now();
        for _ in 0..20 {
            assert!(!guard.opened_at(1, 7, now).scan, "one file, opened often");
        }
        // The same inode number in two trees is two files.
        assert!(!guard.opened_at(2, 7, now).scan);
        assert!(!guard.opened_at(2, 8, now).scan);
        assert!(
            guard.opened_at(3, 1, now).scan,
            "four files over three trees"
        );
    }

    #[test]
    fn opens_zero_turns_the_guard_off() {
        let guard = guard(0, 10, 1 << 30);
        let now = Instant::now();
        assert!((1..1000).all(|ino| !guard.opened_at(1, ino, now).scan));
    }

    #[test]
    fn scan_blocks_stay_within_their_share_oldest_evicted_first() {
        // 10 % of 1000 bytes: 100.
        let guard = guard(10, 10, 1000);
        let evicted = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let seat = |tree: &'static str| {
            let evicted = Arc::clone(&evicted);
            guard.join(Arc::new(move |key: &BlockKey| {
                evicted.lock().push((tree, key.clone()));
            }))
        };
        let (a, b) = (seat("a"), seat("b"));
        a.stored(&key(1), 40);
        b.stored(&key(2), 40);
        assert!(evicted.lock().is_empty());
        a.stored(&key(3), 40);
        assert_eq!(
            *evicted.lock(),
            vec![("a", key(1))],
            "the oldest, of tree a"
        );
        assert_eq!(guard.held(), 80);
        // Used by an interactive read: no longer a scan block.
        b.forget([&key(2)]);
        assert_eq!(guard.held(), 40);
        a.stored(&key(4), 40);
        a.stored(&key(5), 40);
        assert_eq!(evicted.lock().last(), Some(&("a", key(3))));
        assert!(guard.held() <= guard.limit());
        drop(a);
        assert_eq!(guard.held(), 0, "a gone, its blocks are forgotten");
        assert!(!b.any_blocks());
    }

    #[test]
    fn a_share_of_zero_keeps_no_scan_block() {
        let guard = guard(10, 0, 1 << 30);
        let evicted = Arc::new(AtomicUsize::new(0));
        let seat = {
            let evicted = Arc::clone(&evicted);
            guard.join(Arc::new(move |_: &BlockKey| {
                evicted.fetch_add(1, Ordering::Relaxed);
            }))
        };
        seat.stored(&key(1), 4096);
        assert_eq!(evicted.load(Ordering::Relaxed), 1);
        assert_eq!(guard.held(), 0);
    }
}
