//! Counters and the hot-path network guard (DESIGN.md §5.1, §5.8).
//!
//! FUSE handlers that must be answered from RAM or local disk run inside
//! [`hot`]. Every backend call checks [`check_not_hot`]; a call made while a
//! hot scope is active on the same thread counts as a hot-path network call
//! and panics in debug builds. Latency histograms follow in M4.

use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};

macro_rules! counters {
    ($($(#[$meta:meta])* $name:ident),* $(,)?) => {
        #[derive(Debug, Default)]
        pub struct Metrics {
            $($(#[$meta])* pub $name: AtomicU64,)*
        }

        impl Metrics {
            /// Current values, by name.
            pub fn snapshot(&self) -> Vec<(&'static str, u64)> {
                vec![$((stringify!($name), self.$name.load(Ordering::Relaxed)),)*]
            }
        }
    };
}

counters! {
    /// Backend calls made from a hot-path handler. Must stay 0.
    hot_path_network_calls,
    /// Backend calls, all kinds.
    backend_calls,
    /// Directory listings fetched from the backend.
    listings,
    /// Block requests answered from RAM.
    cache_ram_hits,
    /// Block requests answered from the disk cache.
    cache_disk_hits,
    /// Blocks downloaded.
    cache_misses,
    /// Blocks removed from the disk cache to stay under the size limit.
    cache_evictions,
    /// Disk-cache entries that failed authentication and were dropped.
    cache_corrupt,
    /// Block buffers taken from the heap because the pool had no slot free
    /// (§6.2).
    cache_pool_fallbacks,
    /// Blocks fetched by readahead or prefetch.
    prefetched_blocks,
    /// Slow-path requests answered EIO because their deadline expired.
    deadline_expired,
    /// FUSE callbacks that panicked (answered EIO).
    handler_panics,
    /// Files uploaded (new files and revisions).
    uploads,
}

impl Metrics {
    pub fn inc(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

thread_local! {
    static HOT: Cell<bool> = const { Cell::new(false) };
}

/// Runs `f` as a hot-path handler.
pub fn hot<R>(f: impl FnOnce() -> R) -> R {
    struct Reset(bool);
    impl Drop for Reset {
        fn drop(&mut self) {
            HOT.with(|hot| hot.set(self.0));
        }
    }
    let _reset = Reset(HOT.with(|hot| hot.replace(true)));
    f()
}

pub fn is_hot() -> bool {
    HOT.with(Cell::get)
}

/// Called before every backend call.
pub fn check_not_hot(metrics: &Metrics, op: &'static str) {
    Metrics::inc(&metrics.backend_calls);
    if is_hot() {
        Metrics::inc(&metrics.hot_path_network_calls);
        tracing::error!(op, "backend call on the hot path");
        debug_assert!(false, "backend call `{op}` on the hot path");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hot_scope_is_restored_after_nesting() {
        assert!(!is_hot());
        hot(|| {
            assert!(is_hot());
            hot(|| assert!(is_hot()));
            assert!(is_hot());
        });
        assert!(!is_hot());
    }

    #[test]
    fn calls_outside_hot_scope_are_not_flagged() {
        let metrics = Metrics::default();
        check_not_hot(&metrics, "list_children");
        assert_eq!(metrics.hot_path_network_calls.load(Ordering::Relaxed), 0);
        assert_eq!(metrics.backend_calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "on the hot path")]
    fn calls_inside_hot_scope_panic_in_debug_builds() {
        let metrics = Metrics::default();
        hot(|| check_not_hot(&metrics, "fetch_block"));
    }
}
