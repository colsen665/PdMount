//! Event-based sync (DESIGN.md §9; D033, D034).
//!
//! One task follows the volume event stream from the stored cursor: a poll
//! every `sync.poll_interval`, again at once while the server says `more`,
//! and after failures the interval times 1, 1, 2, 3, 5, 8, 13, as the
//! official event manager does. Events carry ids only (F12): the links they
//! name are resolved with `get_nodes`, nodes whose remote state the tree
//! already holds are echoes and skipped, and every listed directory a real
//! change touches is listed again. Remote changes so go through the same
//! reconciliation as listings (`Tree::apply_listing_at`), which keeps local
//! changes. A directory a node left is listed after the one it moved into,
//! so a node moved between two listed directories keeps its inode and its
//! listed subtree.
//!
//! The polling itself is the [`crate::poller`]'s: one poller per volume
//! hands each page to every tree on the volume (D110), and an engine
//! without a daemon's pollers runs one of its own with itself as the only
//! tree.
//!
//! A `refresh` page, or a profile without a cursor, starts a refresh
//! (§9.4): a cursor is taken, events are followed from it at once, and a
//! background task lists every listed directory again (D027), paced. Only
//! when that is done is a cursor stored again; a crash before that finds
//! the old cursor, and the next mount refreshes again. A refresh asked for
//! while one runs makes it run once more, because listings done before the
//! new cursor may have missed changes. The refresh task is one of the
//! engine's background tasks, so a shutdown ends it; it also ends, without
//! storing a cursor, when the tree's root is gone or refused (a revoked
//! share), which the daemon handles by removing the tree (D120).
//!
//! The steps of a sync (a poll, a `get_nodes` batch, a listing) take turns
//! of the session's gate that wait for user requests at most
//! [`EVENT_PATIENCE`] ([`crate::gate::Gate::background_patient`]), so a
//! steady load in one tree cannot stop every tree's sync (D120).

use crate::engine::{Engine, MY_FILES_VOLUME};
use crate::errors::{CoreError, Result};
use crate::metrics::check_not_hot;
use crate::store::Store;
use crate::vfs::Ino;
use pdmount_backend::{BackendError, EventId, EventKind, LinkId, RemoteEvent};
use rusqlite::OptionalExtension;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

/// Links resolved per `get_nodes` call.
pub(crate) const NODE_BATCH: usize = 100;
/// Wait before listing again after a failed refresh.
const REFRESH_RETRY: Duration = Duration::from_secs(30);
/// How long a step of the event sync waits for user requests to end before
/// it takes its turn anyway (D120): the sync is not a guess, and remote
/// changes must arrive under any load (D037).
pub(crate) const EVENT_PATIENCE: Duration = Duration::from_secs(2);

/// Whether a refresh runs, whether it must run once more, and whether one
/// is owed.
#[derive(Debug, Default)]
pub(crate) struct RefreshState {
    /// The refresh task runs.
    running: bool,
    /// It must run once more: a refresh was asked for while it ran.
    again: bool,
    /// A refresh was asked for and has not finished: no cursor is stored.
    owed: bool,
    /// The engine stops: no refresh task starts any more.
    stopped: bool,
}

/// How a pass over the listed directories ended
/// ([`Engine::revalidate_pass`]).
pub(crate) enum Revalidated {
    Done,
    /// Some listings failed; the last error.
    Failed(CoreError),
    /// The tree's root is gone or refused (a revoked share): the pass
    /// stopped at its first listing.
    RootGone(CoreError),
}

/// Stores the cursor of My Files.
pub(crate) fn write_cursor(conn: &rusqlite::Connection, cursor: &EventId) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO event_cursor(volume, event_id, updated_at)
         VALUES (?1, ?2, strftime('%s','now'))",
        (MY_FILES_VOLUME, cursor.as_str()),
    )
    .map(drop)
}

/// The stored cursor of My Files; `None` before the first mount recorded one.
pub(crate) fn load_cursor(store: &Store) -> Result<Option<EventId>> {
    let cursor = store.read(|conn| {
        conn.query_row(
            "SELECT event_id FROM event_cursor WHERE volume = ?1",
            [MY_FILES_VOLUME],
            |row| row.get::<_, String>(0),
        )
        .optional()
    })?;
    Ok(cursor.map(EventId::new))
}

/// `dirs` without duplicates, each source of a move after its destination.
/// A cycle (two nodes swapped between two directories) is broken in event
/// order: the node listed second comes back with a new inode.
fn listing_order(dirs: Vec<Ino>, moves: &[(Ino, Ino)]) -> Vec<Ino> {
    let mut seen = HashSet::new();
    let mut waiting: Vec<Ino> = dirs.into_iter().filter(|dir| seen.insert(*dir)).collect();
    let mut order = Vec::with_capacity(waiting.len());
    while !waiting.is_empty() {
        let ready = waiting
            .iter()
            .position(|dir| {
                moves
                    .iter()
                    .all(|(destination, source)| source != dir || !waiting.contains(destination))
            })
            .unwrap_or(0);
        order.push(waiting.remove(ready));
    }
    order
}

impl Engine {
    /// The event task of an engine that polls its own volume: a
    /// [`Poller`](crate::poller::Poller) with this one tree, so it runs the
    /// same code as the pollers of a daemon with several trees (D110).
    pub(crate) async fn run_events(self: Arc<Self>) {
        let volume = crate::poller::Poller::new(
            MY_FILES_VOLUME,
            true,
            Arc::new(crate::poller::Cadence::default()),
        );
        volume.add(self);
        volume.run().await;
    }

    /// Applies the events waiting on the server, page by page, until none
    /// is left. The cursor advances only past applied pages; it is queued
    /// to the store after the rows a page changed, so a crash never stores
    /// it without them. While a refresh runs it is not stored at all.
    pub async fn sync_events(self: &Arc<Self>) -> Result<()> {
        crate::poller::sync_trees(MY_FILES_VOLUME, std::slice::from_ref(self)).await
    }

    /// The pass of [`Self::sync_events`] is done: the stream was read to its
    /// end.
    pub(crate) fn events_caught_up(&self) {
        *self.last_poll.lock() = Some(std::time::Instant::now());
        // The pending markers of other devices (D076).
        self.markers.scan.notify_one();
    }

    /// How long ago the event stream was last read to its end (`status`).
    pub fn events_last_poll(&self) -> Option<std::time::Duration> {
        self.last_poll.lock().map(|at| at.elapsed())
    }

    /// Whether a refresh is listing directories again, or is owed: no
    /// cursor is stored meanwhile.
    pub fn events_refreshing(&self) -> bool {
        self.refresh.lock().owed
    }

    /// A cursor at the end of the stream, for a refresh (§9.4).
    pub(crate) async fn fresh_cursor(&self) -> Result<EventId> {
        let _turn = self.gate.background_patient(EVENT_PATIENCE).await;
        check_not_hot(&self.metrics, "latest_event_id");
        Ok(self.backend.latest_event_id().await?)
    }

    /// Starts the refresh task, or makes the running one go once more.
    /// Called with the cursor lock held, which the task takes to finish.
    /// After [`Engine::stop_refresh`] none starts; the refresh stays owed,
    /// so the next mount refreshes.
    pub(crate) fn begin_refresh(self: &Arc<Self>) {
        let start = {
            let mut state = self.refresh.lock();
            state.owed = true;
            state.again = state.running;
            !state.stopped && !std::mem::replace(&mut state.running, true)
        };
        if start {
            let this = Arc::clone(self);
            let task = self.runtime().spawn(async move { this.refresh().await });
            let abort = task.abort_handle();
            self.keep_task(task);
            // A shutdown sets `stopped` before it aborts the background
            // tasks, so one that aborted them before the push above is seen
            // here.
            if self.refresh.lock().stopped {
                abort.abort();
            }
        }
    }

    /// No refresh task starts from now on: the first step of a shutdown,
    /// which then aborts the running one with the other background tasks.
    pub(crate) fn stop_refresh(&self) {
        self.refresh.lock().stopped = true;
    }

    /// Lists every listed directory again until a pass succeeds with no
    /// refresh asked for meanwhile, then stores the cursor reached. Ends
    /// without one when the tree's root is gone or refused.
    async fn refresh(self: Arc<Self>) {
        loop {
            let failed = match self.revalidate_pass().await {
                Revalidated::Done => None,
                Revalidated::Failed(error) => Some(error),
                Revalidated::RootGone(error) => {
                    // The daemon removes such a tree (a revoked share,
                    // D112); listing it again every 30 s would only keep the
                    // engine alive. The refresh stays owed.
                    tracing::warn!(%error, "the tree's root is gone or refused; the refresh ends");
                    let mut state = self.refresh.lock();
                    state.running = false;
                    state.again = false;
                    return;
                }
            };
            if let Some(error) = failed {
                tracing::warn!(%error, "refresh incomplete; listing again later");
                tokio::time::sleep(REFRESH_RETRY).await;
                self.health.wait_online().await;
                continue;
            }
            // Refreshes start only under the cursor lock (the poller's
            // pass), so none can be asked for between this check and the end.
            let cursor = self.event_cursor.lock().await;
            {
                let mut state = self.refresh.lock();
                if std::mem::take(&mut state.again) {
                    continue;
                }
                state.running = false;
                state.owed = false;
            }
            if let Some(cursor) = cursor.as_ref() {
                self.store_cursor(cursor);
            }
            tracing::info!("refresh finished");
            return;
        }
    }

    pub(crate) fn store_cursor(&self, cursor: &EventId) {
        let cursor = cursor.clone();
        self.store.write(move |conn| write_cursor(conn, &cursor));
    }

    /// Lists again the listed directories that a page of events changes.
    pub(crate) async fn apply_events(self: &Arc<Self>, events: &[RemoteEvent]) -> Result<()> {
        // Only links the tree knows, or that appear in a listed directory;
        // the rest is fetched fresh when its directory is first listed.
        let links: Vec<LinkId> = {
            let tree = self.tree.read();
            let mut seen = HashSet::new();
            events
                .iter()
                .filter(|event| {
                    tree.by_link(&event.link_id).is_some()
                        || event
                            .parent_link_id
                            .as_ref()
                            .and_then(|parent| tree.by_link(parent))
                            .is_some_and(|dir| tree.is_listed(dir))
                })
                .filter(|event| seen.insert(event.link_id.clone()))
                .map(|event| event.link_id.clone())
                .collect()
        };
        let mut nodes = Vec::with_capacity(links.len());
        let mut echoes = 0;
        for batch in links.chunks(NODE_BATCH) {
            let _turn = self.gate.background_patient(EVENT_PATIENCE).await;
            check_not_hot(&self.metrics, "get_nodes");
            nodes.extend(self.backend.get_nodes(batch).await?);
        }
        let dirs: Vec<Ino> = {
            let tree = self.tree.read();
            let mut dirs = Vec::new();
            // (destination, source) of each node that moved.
            let mut moves = Vec::new();
            for (link, node) in links.iter().zip(&nodes) {
                let local = tree.by_link(link);
                if let (Some(ino), Some(node)) = (local, node)
                    && tree.knows_remote(ino, node)
                {
                    echoes += 1;
                    continue;
                }
                if let (Some(known), Some(node)) = (local.and_then(|ino| tree.get(ino)), node) {
                    let known = known.remote.as_ref();
                    tracing::debug!(
                        link = %link.0,
                        known_rev = ?known.and_then(|k| k.rev.as_ref()),
                        remote_rev = ?node.active_revision,
                        known_parent = ?known.and_then(|k| k.parent_link.as_ref()),
                        remote_parent = ?node.parent_link_id,
                        same_name = known.is_some_and(|k| k.name == node.name),
                        trashed = node.trashed,
                        "remote change"
                    );
                }
                let destination = node
                    .as_ref()
                    .filter(|node| !node.trashed)
                    .and_then(|node| node.parent_link_id.as_ref())
                    .and_then(|parent| tree.by_link(parent))
                    .filter(|&dir| tree.is_listed(dir));
                dirs.extend(destination);
                for source in local
                    .map(|ino| tree.listed_parents(ino))
                    .unwrap_or_default()
                {
                    dirs.push(source);
                    if let Some(destination) = destination.filter(|&d| d != source) {
                        moves.push((destination, source));
                    }
                }
            }
            listing_order(dirs, &moves)
        };
        let count = |kind| events.iter().filter(|event| event.kind == kind).count();
        tracing::info!(
            events = events.len(),
            created = count(EventKind::Create),
            content = count(EventKind::UpdateContent),
            metadata = count(EventKind::UpdateMetadata),
            deleted = count(EventKind::Delete),
            resolved = links.len(),
            batches = links.len().div_ceil(NODE_BATCH),
            echoes,
            listings = dirs.len(),
            "event page applied"
        );
        for dir in dirs {
            let _turn = self.gate.background_patient(EVENT_PATIENCE).await;
            match self.list(dir).await {
                // Removed by an earlier listing of this page, or trashed on
                // the drive (the server no longer lists a trashed folder).
                // The pass below deals with what the folder held locally.
                Ok(()) | Err(CoreError::NotFound | CoreError::Backend(BackendError::NotFound)) => {}
                Err(error) => return Err(error),
            }
        }
        // Removed nodes with local changes. The listings above found those
        // in a listed folder; this also catches nodes moved locally out of
        // the removed node's remote parent (children of a trashed folder are
        // not reported trashed themselves, so no listing shows them gone).
        let gone: Vec<Ino> = {
            let tree = self.tree.read();
            let mut gone_links = HashSet::new();
            let mut inos = Vec::new();
            for (link, node) in links.iter().zip(&nodes) {
                if node.as_ref().is_some_and(|node| !node.trashed) {
                    continue;
                }
                gone_links.insert(link.clone());
                if let Some(ino) = tree.by_link(link)
                    && tree.subtree_has_pending(ino)
                {
                    inos.push(ino);
                }
            }
            if !gone_links.is_empty() {
                inos.extend(tree.pending_under_remote(&gone_links));
            }
            inos
        };
        if !gone.is_empty() {
            self.resolve_vanished(gone).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sources_are_listed_after_destinations() {
        assert_eq!(listing_order(vec![1, 2, 1, 3], &[(2, 1)]), [2, 1, 3]);
        assert_eq!(listing_order(vec![1, 2, 3], &[(3, 2), (2, 1)]), [3, 2, 1]);
        // A swap cannot be ordered; both are still listed once.
        assert_eq!(listing_order(vec![1, 2], &[(1, 2), (2, 1)]), [1, 2]);
    }
}
