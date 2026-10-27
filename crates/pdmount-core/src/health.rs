//! Connectivity state (DESIGN.md §9.6, D041).
//!
//! One [`Health`] per engine says whether the drive can be reached:
//! `Online`, `Offline` (connection failures) or `NeedsLogin` (the server
//! rejected the session). [`Monitored`] wraps the backend and feeds every
//! result to [`Health::observe`], so each caller (journal, event task,
//! reads, probe) contributes to one failure count instead of keeping its
//! own. The journal and the event task wait for `Online`; reads fail fast
//! (`timeouts.offline_read`); the probe task in `Engine::run_health` finds
//! the way back.

use bytes::Bytes;
use pdmount_backend::{
    AccountInfo, Backend, BackendError, BlockInfo, EventId, EventPage, FileMeta, LinkId, NameHash,
    RemoteNode, Result, RevisionId, RevisionMeta, StorageInfo, UploadSource,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::SystemTime;
use tokio::sync::{Notify, watch};

/// Connection failures in a row after which the state is `Offline`.
pub const OFFLINE_AFTER: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Connectivity {
    Online,
    Offline,
    NeedsLogin,
}

impl Connectivity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Online => "online",
            Self::Offline => "offline",
            Self::NeedsLogin => "needs_login",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        [Self::Online, Self::Offline, Self::NeedsLogin]
            .into_iter()
            .find(|state| state.as_str() == text)
    }
}

pub struct Health {
    tx: watch::Sender<Connectivity>,
    failures: AtomicU32,
    since: parking_lot::Mutex<SystemTime>,
    kick: Notify,
}

impl Default for Health {
    fn default() -> Self {
        Self::new(Connectivity::Online)
    }
}

impl Health {
    pub fn new(state: Connectivity) -> Self {
        Self {
            tx: watch::channel(state).0,
            failures: AtomicU32::new(0),
            since: parking_lot::Mutex::new(SystemTime::now()),
            kick: Notify::new(),
        }
    }

    pub fn state(&self) -> Connectivity {
        *self.tx.borrow()
    }

    /// When the current state began.
    pub fn since(&self) -> SystemTime {
        *self.since.lock()
    }

    pub fn subscribe(&self) -> watch::Receiver<Connectivity> {
        self.tx.subscribe()
    }

    /// Resolves once the state is `Online`.
    pub async fn wait_online(&self) {
        let mut state = self.tx.subscribe();
        // The sender lives in `self`, so the channel cannot close.
        let _ = state.wait_for(|state| *state == Connectivity::Online).await;
    }

    /// Asks the probe task to probe now (a login, `pdmount sync`).
    pub fn kick(&self) {
        self.kick.notify_one();
    }

    pub(crate) async fn kicked(&self) {
        self.kick.notified().await;
    }

    /// Sets the state and logs a transition.
    pub fn set(&self, state: Connectivity) {
        if state == Connectivity::Online {
            self.failures.store(0, Ordering::Relaxed);
        }
        let changed = self.tx.send_if_modified(|current| {
            if *current == state {
                return false;
            }
            *current = state;
            true
        });
        if changed {
            *self.since.lock() = SystemTime::now();
            match state {
                Connectivity::Online => tracing::info!("connection to Proton restored"),
                Connectivity::Offline => {
                    tracing::warn!("Proton cannot be reached; working offline");
                }
                Connectivity::NeedsLogin => {
                    tracing::warn!("the session was rejected; run `pdmount login`");
                }
            }
        }
    }

    /// Feeds one backend result into the state.
    pub fn observe<T>(&self, result: &Result<T>) {
        match result {
            Err(BackendError::Offline(_)) => {
                let failures = self.failures.fetch_add(1, Ordering::Relaxed) + 1;
                if failures >= OFFLINE_AFTER && self.state() == Connectivity::Online {
                    self.set(Connectivity::Offline);
                }
            }
            Err(BackendError::Unauthorized) => self.set(Connectivity::NeedsLogin),
            // Any other answer, an error included, came from the server.
            _ => {
                self.failures.store(0, Ordering::Relaxed);
                if self.state() == Connectivity::Offline {
                    self.set(Connectivity::Online);
                }
            }
        }
    }
}

/// The backend, watched: results go to [`Health`], and nothing is sent
/// while the session is rejected.
pub struct Monitored {
    inner: Arc<dyn Backend>,
    health: Arc<Health>,
}

impl Monitored {
    pub fn new(inner: Arc<dyn Backend>, health: Arc<Health>) -> Self {
        Self { inner, health }
    }

    /// A cheap request to find out whether the server answers.
    pub async fn probe(&self) -> Result<()> {
        let result = self.inner.storage_info().await.map(drop);
        self.health.observe(&result);
        result
    }

    fn allowed(&self) -> Result<()> {
        match self.health.state() {
            Connectivity::NeedsLogin => Err(BackendError::Unauthorized),
            _ => Ok(()),
        }
    }

    fn seen<T>(&self, result: Result<T>) -> Result<T> {
        self.health.observe(&result);
        result
    }
}

#[async_trait::async_trait]
impl Backend for Monitored {
    async fn account(&self) -> Result<AccountInfo> {
        self.allowed()?;
        self.seen(self.inner.account().await)
    }

    fn upload_block_size(&self) -> u64 {
        self.inner.upload_block_size()
    }

    async fn root(&self) -> Result<RemoteNode> {
        self.allowed()?;
        self.seen(self.inner.root().await)
    }

    async fn list_children(&self, folder: &LinkId) -> Result<Vec<RemoteNode>> {
        self.allowed()?;
        self.seen(self.inner.list_children(folder).await)
    }

    async fn get_nodes(&self, ids: &[LinkId]) -> Result<Vec<Option<RemoteNode>>> {
        self.allowed()?;
        self.seen(self.inner.get_nodes(ids).await)
    }

    async fn blocks(&self, file: &LinkId, revision: &RevisionId) -> Result<Vec<BlockInfo>> {
        self.allowed()?;
        self.seen(self.inner.blocks(file, revision).await)
    }

    async fn fetch_block(&self, file: &LinkId, revision: &RevisionId, index: u32) -> Result<Bytes> {
        self.allowed()?;
        self.seen(self.inner.fetch_block(file, revision, index).await)
    }

    async fn fetch_block_into(
        &self,
        file: &LinkId,
        revision: &RevisionId,
        index: u32,
        buf: &mut [u8],
    ) -> Result<usize> {
        self.allowed()?;
        self.seen(
            self.inner
                .fetch_block_into(file, revision, index, buf)
                .await,
        )
    }

    async fn upload_new(
        &self,
        parent: &LinkId,
        name: &str,
        source: UploadSource,
        meta: FileMeta,
    ) -> Result<(LinkId, RevisionId)> {
        self.allowed()?;
        self.seen(self.inner.upload_new(parent, name, source, meta).await)
    }

    async fn upload_revision(
        &self,
        file: &LinkId,
        base: &RevisionId,
        source: UploadSource,
        meta: FileMeta,
    ) -> Result<RevisionId> {
        self.allowed()?;
        self.seen(self.inner.upload_revision(file, base, source, meta).await)
    }

    async fn create_folder(
        &self,
        parent: &LinkId,
        name: &str,
        mtime: SystemTime,
    ) -> Result<LinkId> {
        self.allowed()?;
        self.seen(self.inner.create_folder(parent, name, mtime).await)
    }

    async fn rename(
        &self,
        node: &LinkId,
        new_parent: Option<&LinkId>,
        new_name: &str,
        expected: Option<&NameHash>,
    ) -> Result<()> {
        self.allowed()?;
        self.seen(
            self.inner
                .rename(node, new_parent, new_name, expected)
                .await,
        )
    }

    async fn trash(&self, nodes: &[LinkId]) -> Result<Vec<Result<()>>> {
        self.allowed()?;
        self.seen(self.inner.trash(nodes).await)
    }

    async fn revisions(&self, file: &LinkId) -> Result<Vec<RevisionMeta>> {
        self.allowed()?;
        self.seen(self.inner.revisions(file).await)
    }

    async fn latest_event_id(&self) -> Result<EventId> {
        self.allowed()?;
        self.seen(self.inner.latest_event_id().await)
    }

    async fn poll_events(&self, since: &EventId) -> Result<EventPage> {
        self.allowed()?;
        self.seen(self.inner.poll_events(since).await)
    }

    async fn storage_info(&self) -> Result<StorageInfo> {
        self.allowed()?;
        self.seen(self.inner.storage_info().await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn offline() -> Result<()> {
        Err(BackendError::Offline("no route".into()))
    }

    #[test]
    fn three_connection_failures_in_a_row_go_offline() {
        let health = Health::default();
        health.observe(&offline());
        health.observe(&offline());
        assert_eq!(health.state(), Connectivity::Online);
        health.observe(&offline());
        assert_eq!(health.state(), Connectivity::Offline);
    }

    #[test]
    fn an_answer_resets_the_count() {
        let health = Health::default();
        health.observe(&offline());
        health.observe(&offline());
        health.observe(&Ok(()));
        health.observe(&offline());
        health.observe(&offline());
        assert_eq!(health.state(), Connectivity::Online);
    }

    #[test]
    fn any_server_error_proves_the_server_is_reachable() {
        let health = Health::default();
        health.set(Connectivity::Offline);
        health.observe::<()>(&Err(BackendError::NotFound));
        assert_eq!(health.state(), Connectivity::Online);
        health.set(Connectivity::Offline);
        health.observe::<()>(&Err(BackendError::RateLimited { retry_after: None }));
        assert_eq!(health.state(), Connectivity::Online);
    }

    #[test]
    fn unauthorized_needs_a_login_and_only_set_ends_it() {
        let health = Health::default();
        health.observe::<()>(&Err(BackendError::Unauthorized));
        assert_eq!(health.state(), Connectivity::NeedsLogin);
        health.observe(&Ok(()));
        health.observe::<()>(&Err(BackendError::NotFound));
        assert_eq!(health.state(), Connectivity::NeedsLogin);
        health.set(Connectivity::Online);
        assert_eq!(health.state(), Connectivity::Online);
    }

    #[test]
    fn offline_failures_do_not_leave_needs_login() {
        let health = Health::default();
        health.set(Connectivity::NeedsLogin);
        for _ in 0..5 {
            health.observe(&offline());
        }
        assert_eq!(health.state(), Connectivity::NeedsLogin);
    }

    #[tokio::test]
    async fn wait_online_resolves_after_the_transition() {
        let health = Arc::new(Health::new(Connectivity::Offline));
        let waiting = {
            let health = Arc::clone(&health);
            tokio::spawn(async move { health.wait_online().await })
        };
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());
        health.observe(&Ok(()));
        tokio::time::timeout(Duration::from_secs(5), waiting)
            .await
            .expect("woken")
            .expect("joined");
    }
}
