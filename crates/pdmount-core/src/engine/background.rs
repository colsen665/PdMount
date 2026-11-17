//! Background tasks of an open engine: the task set, the health loop, relisting, revalidation and the periodic jobs.

use super::*;
use crate::events::Revalidated;
use pdmount_backend::BackendError;

impl Engine {
    /// Asks for `dirs` to be listed again, in the background. The store
    /// keeps them until each is listed, so a stop or crash before that
    /// lists them at the next start (D069).
    pub(crate) fn list_later(&self, dirs: Vec<Ino>) {
        self.journal.list_later(dirs, |open| self.save_relist(open));
    }

    /// Replaces the directories the store keeps to list again; queued like
    /// the rows of the change that asked for them.
    pub(crate) fn save_relist(&self, dirs: Vec<Ino>) {
        let value = dirs
            .iter()
            .map(Ino::to_string)
            .collect::<Vec<_>>()
            .join(" ");
        self.store
            .write(move |conn| crate::store::meta_set(conn, RELIST_KEY, &value));
    }

    /// The directories the last run left to list again, if they are still
    /// listed folders. The store forgets them: queuing saves them again.
    pub(super) fn saved_relist(&self) -> Vec<Ino> {
        let saved = self.store.meta_get(RELIST_KEY).unwrap_or_else(|error| {
            tracing::warn!(%error, "the directories to list again could not be read");
            None
        });
        let saved = saved.unwrap_or_default();
        if saved.is_empty() {
            return Vec::new();
        }
        self.save_relist(Vec::new());
        let tree = self.tree.read();
        saved
            .split_whitespace()
            .filter_map(|dir| dir.parse().ok())
            .filter(|&dir| {
                tree.get(dir)
                    .is_some_and(|node| node.attached && node.is_dir() && node.listed)
            })
            .collect()
    }

    /// Adds a task started later (a refresh) to the background tasks, which
    /// a shutdown aborts; drops those that ended.
    pub(crate) fn keep_task(&self, task: tokio::task::JoinHandle<()>) {
        let mut tasks = self.background.lock();
        tasks.retain(|task| !task.is_finished());
        tasks.push(task);
    }

    /// Starts the event task (which also catches up after mounting), the
    /// journal executor, quota refresh and cache bookkeeping.
    pub fn start_background(self: &Arc<Self>) {
        self.start_tasks(true);
    }

    /// [`Engine::start_background`] without the event task: a
    /// [`crate::poller::Poller`] of the tree's volume hands this engine its
    /// event pages (D080, D110).
    pub fn start_background_polled(self: &Arc<Self>) {
        self.start_tasks(false);
    }

    pub(super) fn start_tasks(self: &Arc<Self>, own_events: bool) {
        // Remote nodes a yield left hidden before the last stop (D055), and
        // listings it cut short (D069).
        let mut dirs = self.folders_missing_an_other_copy();
        dirs.extend(self.saved_relist());
        self.list_later(dirs);
        let mut tasks = self.background.lock();
        if own_events {
            let this = Arc::clone(self);
            tasks.push(self.runtime.spawn(async move { this.run_events().await }));
        }
        let this = Arc::clone(self);
        tasks.push(self.runtime.spawn(async move { this.run_journal().await }));
        let this = Arc::clone(self);
        tasks.push(self.runtime.spawn(async move { this.run_health().await }));
        let this = Arc::clone(self);
        tasks.push(self.runtime.spawn(async move {
            loop {
                this.refresh_storage().await;
                tokio::time::sleep(STATFS_REFRESH).await;
            }
        }));
        let this = Arc::clone(self);
        tasks.push(self.runtime.spawn(async move {
            loop {
                tokio::time::sleep(ACCESS_FLUSH).await;
                this.cache.flush_access();
            }
        }));
        let this = Arc::clone(self);
        tasks.push(self.runtime.spawn(async move { this.sweep_cache().await }));
        let this = Arc::clone(self);
        tasks.push(self.runtime.spawn(async move { this.run_pins().await }));
        let this = Arc::clone(self);
        tasks.push(self.runtime.spawn(async move { this.run_markers().await }));
        let this = Arc::clone(self);
        tasks.push(
            self.runtime
                .spawn(async move { this.run_own_marker().await }),
        );
        let this = Arc::clone(self);
        tasks.push(
            self.runtime
                .spawn(async move { this.keep_inos_reserved().await }),
        );
        let this = Arc::clone(self);
        tasks.push(
            self.runtime
                .spawn(async move { this.keep_ignore_rules().await }),
        );
        self.refresh_ignore();
        let this = Arc::clone(self);
        tasks.push(
            self.runtime
                .spawn(async move { this.import_config_rules().await }),
        );
    }

    /// The probe task: while the drive cannot be reached, asks for the quota
    /// every `sync.probe_interval` (or at once after [`Health::kick`]); the
    /// first answer ends `Offline`. Work that waits for `Online` is woken on
    /// the transition. A rejected session is not probed: only a login ends it.
    pub(crate) async fn run_health(self: Arc<Self>) {
        let interval = self.config.sync.probe_interval.0;
        let mut changes = self.health.subscribe();
        let mut state = self.health.state();
        if !self.owns_health {
            // The engine that made the shared state probes it (D110).
            while changes.changed().await.is_ok() {
                let now = self.health.state();
                if now == Connectivity::Online && state != Connectivity::Online {
                    self.journal.wake_all();
                }
                state = now;
            }
            return;
        }
        loop {
            match state {
                Connectivity::Online => {
                    if changes.changed().await.is_err() {
                        return;
                    }
                }
                Connectivity::Offline => {
                    tokio::select! {
                        () = tokio::time::sleep(interval) => {}
                        () = self.health.kicked() => {}
                        _ = changes.changed() => {}
                    }
                    if self.health.state() == Connectivity::Offline {
                        let _turn = self.gate.background().await;
                        if let Err(error) = self.monitored.probe().await {
                            tracing::debug!(%error, "probe failed");
                        }
                    }
                }
                Connectivity::NeedsLogin => {
                    tokio::select! {
                        () = self.health.kicked() => {
                            // A new session was set: the first answer ends the state.
                            if self.health.state() == Connectivity::NeedsLogin {
                                let _turn = self.gate.background().await;
                                match self.monitored.probe().await {
                                    Ok(()) => self.health.set(Connectivity::Online),
                                    Err(error) => tracing::debug!(%error, "probe failed"),
                                }
                            }
                        }
                        _ = changes.changed() => {}
                    }
                }
            }
            let now = self.health.state();
            if now == Connectivity::Online && state != Connectivity::Online {
                self.journal.wake_all();
            }
            state = now;
        }
    }

    /// Re-lists every listed directory, root first, as background work
    /// (D027, D034): for a `refresh` from the event stream, or a profile
    /// without a cursor. Fails with the last error after trying them all,
    /// or at once with the root's when the root is gone or refused.
    pub async fn revalidate(self: &Arc<Self>) -> Result<()> {
        match self.revalidate_pass().await {
            Revalidated::Done => Ok(()),
            Revalidated::Failed(error) | Revalidated::RootGone(error) => Err(error),
        }
    }

    /// [`Engine::revalidate`], telling a root that is gone or refused (a
    /// revoked share) from other failures: the pass stops there, since
    /// every other listing of the tree would fail the same way (D120).
    pub(crate) async fn revalidate_pass(self: &Arc<Self>) -> Revalidated {
        let dirs = self.tree.read().listed_dirs();
        let count = dirs.len();
        let mut failed = None;
        for dir in dirs {
            let _turn = self.gate.background().await;
            match self.list(dir).await {
                // Removed by the listing of an ancestor.
                Ok(()) | Err(CoreError::NotFound) => {}
                Err(
                    error @ CoreError::Backend(
                        BackendError::NotFound | BackendError::PermissionDenied,
                    ),
                ) if dir == ROOT_INO => {
                    tracing::warn!(%error, "the tree's root cannot be listed");
                    return Revalidated::RootGone(error);
                }
                Err(error) => {
                    tracing::warn!(dir, %error, "revalidation listing failed");
                    failed = Some(error);
                }
            }
        }
        tracing::info!(directories = count, "revalidation finished");
        failed.map_or(Revalidated::Done, Revalidated::Failed)
    }

    pub async fn refresh_storage(&self) {
        check_not_hot(&self.metrics, "storage_info");
        match self.backend.storage_info().await {
            Ok(info) => *self.storage.lock() = Some(info),
            Err(error) => tracing::warn!(%error, "cannot refresh the quota"),
        }
    }

    /// Hands every node with pending work to the journal (after loading).
    pub(super) fn queue_pending_work(&self) {
        let pending: Vec<Ino> = {
            let tree = self.tree.read();
            tree.inos()
                .filter(|&ino| tree.has_pending_work(ino))
                .collect()
        };
        for ino in pending {
            self.journal.touch(ino);
        }
    }
}
