//! The session's own limits on API work (DESIGN.md §5.3, §6.4; D027, D033,
//! D058), one set per daemon and shared by every tree's engine and the
//! Photos tree (D110, D113).
//!
//! Proton's rate limits are per session and user (`cs/README.md:40`), and the
//! official SDKs bound transfers per client. A daemon has one session and one
//! drive client for all its trees (`ProtonBackend::tree`), so what PdMount
//! counts itself is counted per session too: the block download permits of
//! every tree's [`BlockCache`](crate::cache::BlockCache), the uploads of every
//! tree's journal (`upload.concurrency`), and the background turns of D027.
//! An engine or Photos tree opened without one makes its own, which is a
//! tree alone as before.

use crate::cache::DownloadSlots;
use crate::config::Config;
use crate::gate::Gate;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;

/// Spacing of speculative API requests (prefetch, revalidation): at most
/// two per second over the session, one at a time, and none while a user
/// request waits (D027).
pub(crate) const BACKGROUND_INTERVAL: Duration = Duration::from_millis(500);

pub struct SessionLimits {
    /// Block downloads in flight, per priority and in total (D033).
    pub(crate) downloads: Arc<DownloadSlots>,
    /// Files uploading at once: `upload.concurrency` (D058).
    pub(crate) uploads: Arc<Semaphore>,
    /// Background turns: one at a time, spaced, and none while a request a
    /// user waits on is in flight in any tree (D027).
    pub(crate) gate: Arc<Gate>,
}

impl SessionLimits {
    /// Limits from the profile's config (`upload.concurrency`).
    pub fn new(config: &Config) -> Arc<Self> {
        Arc::new(Self {
            downloads: Arc::new(DownloadSlots::new()),
            uploads: Arc::new(Semaphore::new(config.upload.concurrency.max(1) as usize)),
            gate: Arc::new(Gate::new(BACKGROUND_INTERVAL)),
        })
    }

    /// The download permits, for a cache opened without an engine.
    pub fn downloads(&self) -> &Arc<DownloadSlots> {
        &self.downloads
    }
}
