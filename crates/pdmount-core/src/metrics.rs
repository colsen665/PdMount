//! Counters, latency histograms and the hot-path network guard (DESIGN.md
//! §5.1, §5.8).
//!
//! FUSE handlers that must be answered from RAM or local disk run inside
//! [`hot`]. Every backend call checks [`check_not_hot`]; a call made while a
//! hot scope is active on the same thread counts as a hot-path network call
//! and panics in debug builds.
//!
//! A FUSE request is timed by a [`Stopwatch`] from callback entry until it
//! drops, right after the reply; backend calls are timed by
//! [`TimedBackend`]. Both record into [`Latency`].

use bytes::Bytes;
use hdrhistogram::Histogram;
use parking_lot::Mutex;
use pdmount_backend::{
    AccountInfo, Backend, BackendError, BlockInfo, EventId, EventPage, FileMeta, LinkId, NameHash,
    RemoteNode, RevisionId, RevisionMeta, StorageInfo, UploadSource,
};
use std::cell::Cell;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

/// A FUSE request slower than this is logged at WARN (§5.8).
const SLOW_REQUEST: Duration = Duration::from_millis(50);
/// At most one slow-request line per operation in this interval.
const SLOW_LOG_INTERVAL: Duration = Duration::from_secs(10);
/// Histogram range in µs: 1 µs to 120 s, 3 significant digits.
const LATENCY_MAX_US: u64 = 120_000_000;
const LATENCY_DIGITS: u8 = 3;

macro_rules! counters {
    ($($(#[$meta:meta])* $name:ident),* $(,)?) => {
        #[derive(Debug, Default)]
        pub struct Metrics {
            $($(#[$meta])* pub $name: AtomicU64,)*
            pub latency: Latency,
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
    /// Scans begun by an open in this tree (D111).
    scans,
    /// Slow-path reads served as scan reads (D111).
    scan_reads,
    /// Blocks evicted to keep scan blocks within `scan.cache_share`.
    scan_evictions,
    /// Slow-path requests answered EIO because their deadline expired.
    deadline_expired,
    /// FUSE callbacks that panicked (answered EIO).
    handler_panics,
    /// Files uploaded (new files and revisions).
    uploads,
    /// Backend calls that ended in `RateLimited` (HTTP 429).
    rate_limited,
}

impl Metrics {
    pub fn inc(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// Latency summaries of the operations seen since the last reset.
    pub fn snapshot_latency(&self) -> Vec<(&'static str, Summary)> {
        self.latency.snapshot()
    }

    pub fn reset_latency(&self) {
        self.latency.reset();
    }
}

macro_rules! ops {
    ($($(#[$meta:meta])* $variant:ident = $name:literal),* $(,)?) => {
        /// Keys of the latency histograms.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum Op {
            $($(#[$meta])* $variant,)*
        }

        impl Op {
            pub const ALL: &[Op] = &[$(Op::$variant,)*];

            pub fn as_str(self) -> &'static str {
                match self {
                    $(Op::$variant => $name,)*
                }
            }
        }
    };
}

ops! {
    Lookup = "lookup",
    Getattr = "getattr",
    Setattr = "setattr",
    Readdir = "readdir",
    Readdirplus = "readdirplus",
    Opendir = "opendir",
    Releasedir = "releasedir",
    Open = "open",
    Create = "create",
    Mkdir = "mkdir",
    Unlink = "unlink",
    Rmdir = "rmdir",
    Rename = "rename",
    Write = "write",
    Flush = "flush",
    Fsync = "fsync",
    Fsyncdir = "fsyncdir",
    Release = "release",
    Statfs = "statfs",
    Getxattr = "getxattr",
    Listxattr = "listxattr",
    Access = "access",
    Forget = "forget",
    /// A read answered from the RAM cache (or past the end of the file).
    ReadRam = "read_ram",
    /// A read that took at least one block from the disk cache.
    ReadDisk = "read_disk",
    /// A read of a file with local changes.
    ReadStaging = "read_staging",
    /// A read that waited on the slow path.
    ReadSlow = "read_slow",
    BackendAccount = "backend_account",
    BackendRoot = "backend_root",
    BackendListChildren = "backend_list_children",
    BackendGetNodes = "backend_get_nodes",
    BackendBlocks = "backend_blocks",
    BackendFetchBlock = "backend_fetch_block",
    BackendFetchBlockInto = "backend_fetch_block_into",
    BackendUploadNew = "backend_upload_new",
    BackendUploadRevision = "backend_upload_revision",
    BackendCreateFolder = "backend_create_folder",
    BackendRename = "backend_rename",
    BackendTrash = "backend_trash",
    BackendRevisions = "backend_revisions",
    BackendLatestEventId = "backend_latest_event_id",
    BackendPollEvents = "backend_poll_events",
    BackendStorageInfo = "backend_storage_info",
}

/// Quantiles of one histogram, in µs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Summary {
    pub count: u64,
    pub p50: u64,
    pub p90: u64,
    pub p99: u64,
    pub max: u64,
}

/// When an operation last logged a slow request, and the lines held back
/// since.
#[derive(Debug, Default)]
struct SlowLog {
    last: Option<Instant>,
    suppressed: u64,
}

/// One HDR histogram per [`Op`], in µs.
#[derive(Debug)]
pub struct Latency {
    histograms: Vec<Mutex<Histogram<u64>>>,
    slow: Vec<Mutex<SlowLog>>,
}

impl Default for Latency {
    fn default() -> Self {
        let histogram = || {
            Histogram::new_with_bounds(1, LATENCY_MAX_US, LATENCY_DIGITS)
                .expect("valid histogram bounds")
        };
        Self {
            histograms: Op::ALL.iter().map(|_| Mutex::new(histogram())).collect(),
            slow: Op::ALL.iter().map(|_| Mutex::default()).collect(),
        }
    }
}

impl Latency {
    pub fn record(&self, op: Op, elapsed: Duration) {
        let us = u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX);
        self.histograms[op as usize].lock().saturating_record(us);
    }

    /// Summaries of the operations recorded at least once.
    pub fn snapshot(&self) -> Vec<(&'static str, Summary)> {
        Op::ALL
            .iter()
            .filter_map(|&op| {
                let histogram = self.histograms[op as usize].lock();
                (!histogram.is_empty()).then(|| {
                    (
                        op.as_str(),
                        Summary {
                            count: histogram.len(),
                            p50: histogram.value_at_quantile(0.5),
                            p90: histogram.value_at_quantile(0.9),
                            p99: histogram.value_at_quantile(0.99),
                            max: histogram.max(),
                        },
                    )
                })
            })
            .collect()
    }

    pub fn reset(&self) {
        for histogram in &self.histograms {
            histogram.lock().reset();
        }
    }

    /// Whether a slow request of `op` may be logged now; if so, with the
    /// number of lines held back since the last one.
    fn slow_line(&self, op: Op, now: Instant) -> Option<u64> {
        let mut log = self.slow[op as usize].lock();
        match log.last {
            Some(last) if now.saturating_duration_since(last) < SLOW_LOG_INTERVAL => {
                log.suppressed += 1;
                None
            }
            _ => {
                log.last = Some(now);
                Some(std::mem::take(&mut log.suppressed))
            }
        }
    }
}

/// Times one FUSE request from callback entry until it drops, which the
/// handler arranges to happen right after the reply, including a reply
/// deferred until the store commit.
pub struct Stopwatch {
    op: Op,
    start: Instant,
    metrics: Arc<Metrics>,
    unique: u64,
    ino: Option<u64>,
    /// The request went to the slow path (network).
    slow_path: bool,
}

impl Stopwatch {
    pub fn new(metrics: Arc<Metrics>, op: Op, unique: u64, ino: Option<u64>) -> Self {
        Self {
            op,
            start: Instant::now(),
            metrics,
            unique,
            ino,
            slow_path: false,
        }
    }

    pub fn op(&self) -> Op {
        self.op
    }

    /// Records the request under another key (a read, by its source).
    pub fn set_op(&mut self, op: Op) {
        self.op = op;
    }

    /// The request continues on the slow path; returns the span its future
    /// runs in.
    pub fn slow_path(&mut self) -> tracing::Span {
        self.slow_path = true;
        tracing::debug_span!(
            "fuse",
            op = self.op.as_str(),
            unique = self.unique,
            ino = self.ino
        )
    }
}

impl Drop for Stopwatch {
    fn drop(&mut self) {
        let elapsed = self.start.elapsed();
        self.metrics.latency.record(self.op, elapsed);
        // Only the hot path has a budget (§5.7); flush and release wait for
        // the store commit, fsync for the disk.
        if elapsed <= SLOW_REQUEST
            || self.slow_path
            || matches!(self.op, Op::Fsync | Op::Flush | Op::Release)
        {
            return;
        }
        if let Some(suppressed) = self.metrics.latency.slow_line(self.op, Instant::now()) {
            tracing::warn!(
                op = self.op.as_str(),
                ino = self.ino,
                elapsed_ms = elapsed.as_millis() as u64,
                suppressed,
                "slow request"
            );
        }
    }
}

/// The backend with every call timed under its [`Op`], and `RateLimited`
/// answers counted.
pub struct TimedBackend {
    inner: Arc<dyn Backend>,
    metrics: Arc<Metrics>,
}

impl TimedBackend {
    pub fn new(inner: Arc<dyn Backend>, metrics: Arc<Metrics>) -> Self {
        Self { inner, metrics }
    }

    async fn timed<T>(
        &self,
        op: Op,
        call: impl Future<Output = pdmount_backend::Result<T>>,
    ) -> pdmount_backend::Result<T> {
        let start = Instant::now();
        let result = call.await;
        self.metrics.latency.record(op, start.elapsed());
        if let Err(BackendError::RateLimited { .. }) = &result {
            Metrics::inc(&self.metrics.rate_limited);
        }
        result
    }
}

#[async_trait::async_trait]
impl Backend for TimedBackend {
    async fn account(&self) -> pdmount_backend::Result<AccountInfo> {
        self.timed(Op::BackendAccount, self.inner.account()).await
    }

    fn upload_block_size(&self) -> u64 {
        self.inner.upload_block_size()
    }

    async fn root(&self) -> pdmount_backend::Result<RemoteNode> {
        self.timed(Op::BackendRoot, self.inner.root()).await
    }

    async fn list_children(&self, folder: &LinkId) -> pdmount_backend::Result<Vec<RemoteNode>> {
        self.timed(Op::BackendListChildren, self.inner.list_children(folder))
            .await
    }

    async fn get_nodes(&self, ids: &[LinkId]) -> pdmount_backend::Result<Vec<Option<RemoteNode>>> {
        self.timed(Op::BackendGetNodes, self.inner.get_nodes(ids))
            .await
    }

    async fn get_nodes_checked(
        &self,
        ids: &[LinkId],
    ) -> pdmount_backend::Result<Vec<pdmount_backend::NodeLookup>> {
        self.timed(Op::BackendGetNodes, self.inner.get_nodes_checked(ids))
            .await
    }

    async fn blocks(
        &self,
        file: &LinkId,
        revision: &RevisionId,
    ) -> pdmount_backend::Result<Vec<BlockInfo>> {
        self.timed(Op::BackendBlocks, self.inner.blocks(file, revision))
            .await
    }

    async fn fetch_block_into(
        &self,
        file: &LinkId,
        revision: &RevisionId,
        index: u32,
        buf: &mut [u8],
    ) -> pdmount_backend::Result<usize> {
        self.timed(
            Op::BackendFetchBlockInto,
            self.inner.fetch_block_into(file, revision, index, buf),
        )
        .await
    }

    async fn fetch_block(
        &self,
        file: &LinkId,
        revision: &RevisionId,
        index: u32,
    ) -> pdmount_backend::Result<Bytes> {
        self.timed(
            Op::BackendFetchBlock,
            self.inner.fetch_block(file, revision, index),
        )
        .await
    }

    async fn upload_new(
        &self,
        parent: &LinkId,
        name: &str,
        source: UploadSource,
        meta: FileMeta,
    ) -> pdmount_backend::Result<(LinkId, RevisionId)> {
        self.timed(
            Op::BackendUploadNew,
            self.inner.upload_new(parent, name, source, meta),
        )
        .await
    }

    async fn upload_revision(
        &self,
        file: &LinkId,
        base: &RevisionId,
        source: UploadSource,
        meta: FileMeta,
    ) -> pdmount_backend::Result<RevisionId> {
        self.timed(
            Op::BackendUploadRevision,
            self.inner.upload_revision(file, base, source, meta),
        )
        .await
    }

    async fn create_folder(
        &self,
        parent: &LinkId,
        name: &str,
        mtime: SystemTime,
    ) -> pdmount_backend::Result<LinkId> {
        self.timed(
            Op::BackendCreateFolder,
            self.inner.create_folder(parent, name, mtime),
        )
        .await
    }

    async fn rename(
        &self,
        node: &LinkId,
        new_parent: Option<&LinkId>,
        new_name: &str,
        expected: Option<&NameHash>,
    ) -> pdmount_backend::Result<()> {
        self.timed(
            Op::BackendRename,
            self.inner.rename(node, new_parent, new_name, expected),
        )
        .await
    }

    async fn trash(
        &self,
        nodes: &[LinkId],
    ) -> pdmount_backend::Result<Vec<pdmount_backend::Result<()>>> {
        self.timed(Op::BackendTrash, self.inner.trash(nodes)).await
    }

    async fn revisions(&self, file: &LinkId) -> pdmount_backend::Result<Vec<RevisionMeta>> {
        self.timed(Op::BackendRevisions, self.inner.revisions(file))
            .await
    }

    async fn latest_event_id(&self) -> pdmount_backend::Result<EventId> {
        self.timed(Op::BackendLatestEventId, self.inner.latest_event_id())
            .await
    }

    async fn poll_events(&self, since: &EventId) -> pdmount_backend::Result<EventPage> {
        self.timed(Op::BackendPollEvents, self.inner.poll_events(since))
            .await
    }

    async fn storage_info(&self) -> pdmount_backend::Result<StorageInfo> {
        self.timed(Op::BackendStorageInfo, self.inner.storage_info())
            .await
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
    fn latency_records_and_summarises() {
        let latency = Latency::default();
        for us in 1..=100 {
            latency.record(Op::Getattr, Duration::from_micros(us));
        }
        // Beyond the range: kept as its top.
        latency.record(Op::Lookup, Duration::from_secs(500));
        let snapshot = latency.snapshot();
        let names: Vec<&str> = snapshot.iter().map(|(name, _)| *name).collect();
        assert_eq!(names, ["lookup", "getattr"]);
        assert_eq!(
            snapshot[1].1,
            Summary {
                count: 100,
                p50: 50,
                p90: 90,
                p99: 99,
                max: 100
            }
        );
        let top = snapshot[0].1.max;
        assert!(
            top.abs_diff(LATENCY_MAX_US) < LATENCY_MAX_US / 1000,
            "{top}"
        );

        latency.reset();
        assert!(latency.snapshot().is_empty());
    }

    #[test]
    fn a_stopwatch_records_once_when_its_deferred_reply_drops() {
        let metrics = Arc::new(Metrics::default());
        let mut watch = Stopwatch::new(Arc::clone(&metrics), Op::ReadRam, 7, Some(2));
        watch.set_op(Op::ReadDisk);
        let deferred = move || drop(watch);
        std::thread::sleep(Duration::from_millis(2));
        assert!(metrics.snapshot_latency().is_empty());
        deferred();
        let snapshot = metrics.snapshot_latency();
        assert_eq!(snapshot.len(), 1);
        let (name, summary) = snapshot[0];
        assert_eq!(name, "read_disk");
        assert_eq!(summary.count, 1);
        assert!(summary.max >= 2000, "{summary:?}");
    }

    #[test]
    fn slow_requests_log_once_per_interval_with_a_count_of_the_rest() {
        let latency = Latency::default();
        let start = Instant::now();
        assert_eq!(latency.slow_line(Op::Create, start), Some(0));
        assert_eq!(latency.slow_line(Op::Create, start), None);
        // Other operations have their own budget.
        assert_eq!(latency.slow_line(Op::Mkdir, start), Some(0));
        assert_eq!(
            latency.slow_line(Op::Create, start + Duration::from_secs(9)),
            None
        );
        assert_eq!(
            latency.slow_line(Op::Create, start + SLOW_LOG_INTERVAL),
            Some(2)
        );
    }

    #[tokio::test]
    async fn timed_backend_times_calls_and_counts_rate_limits() {
        use pdmount_backend::fake::{FakeBackend, FakeOp};
        let fake = FakeBackend::new();
        fake.inject_failure(
            FakeOp::ListChildren,
            BackendError::RateLimited { retry_after: None },
        );
        let metrics = Arc::new(Metrics::default());
        let timed = TimedBackend::new(Arc::new(fake), Arc::clone(&metrics));
        let root = timed.root().await.expect("root").link_id;
        assert!(matches!(
            timed.list_children(&root).await,
            Err(BackendError::RateLimited { .. })
        ));
        timed.list_children(&root).await.expect("second listing");
        assert_eq!(metrics.rate_limited.load(Ordering::Relaxed), 1);
        let counts: Vec<(&str, u64)> = metrics
            .snapshot_latency()
            .into_iter()
            .map(|(name, summary)| (name, summary.count))
            .collect();
        assert_eq!(counts, [("backend_root", 1), ("backend_list_children", 2)]);
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "on the hot path")]
    fn calls_inside_hot_scope_panic_in_debug_builds() {
        let metrics = Metrics::default();
        hot(|| check_not_hot(&metrics, "fetch_block"));
    }
}
