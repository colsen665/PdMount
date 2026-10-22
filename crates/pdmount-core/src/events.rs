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
//! A `refresh` page, or a profile without a cursor, starts a refresh
//! (§9.4): a cursor is taken, events are followed from it at once, and a
//! background task lists every listed directory again (D025), paced. Only
//! when that is done is a cursor stored again; a crash before that finds
//! the old cursor, and the next mount refreshes again. A refresh asked for
//! while one runs makes it run once more, because listings done before the
//! new cursor may have missed changes.

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

/// Multiples of the poll interval after consecutive failures
/// (`cs/client/js/src/internal/events/eventManager.ts:4`).
const BACKOFF_STEPS: [u32; 7] = [1, 1, 2, 3, 5, 8, 13];
const JITTER: Duration = Duration::from_secs(1);
/// Links resolved per `get_nodes` call.
const NODE_BATCH: usize = 100;
/// Wait before listing again after a failed refresh.
const REFRESH_RETRY: Duration = Duration::from_secs(30);

/// Whether a refresh runs, and whether it must run once more.
#[derive(Debug, Default)]
pub(crate) struct RefreshState {
    running: bool,
    again: bool,
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

/// How long to wait after `failures` failed polls in a row (0 = the first).
fn backoff(interval: Duration, failures: usize, error: &CoreError) -> Duration {
    let step = BACKOFF_STEPS[failures.min(BACKOFF_STEPS.len() - 1)];
    let delay = interval * step + JITTER.mul_f64(rand::random::<f64>());
    match error {
        CoreError::Backend(BackendError::RateLimited {
            retry_after: Some(after),
        }) => delay.max(*after),
        _ => delay,
    }
}

impl Engine {
    /// The event task: follows the stream until the engine stops.
    pub(crate) async fn run_events(self: Arc<Self>) {
        let interval = self.config.sync.poll_interval.0;
        let mut failures = 0;
        loop {
            let delay = match self.sync_events().await {
                Ok(()) => {
                    failures = 0;
                    interval
                }
                Err(error) => {
                    let delay = backoff(interval, failures, &error);
                    failures += 1;
                    tracing::warn!(%error, ?delay, failures, "event sync failed");
                    delay
                }
            };
            tokio::time::sleep(delay).await;
        }
    }

    /// Applies the events waiting on the server, page by page, until none
    /// is left. The cursor advances only past applied pages; it is queued
    /// to the store after the rows a page changed, so a crash never stores
    /// it without them. While a refresh runs it is not stored at all.
    pub async fn sync_events(self: &Arc<Self>) -> Result<()> {
        let mut cursor = self.event_cursor.lock().await;
        let mut since = match cursor.clone() {
            Some(since) => since,
            None => {
                let fresh = self.start_refresh().await?;
                *cursor = Some(fresh.clone());
                fresh
            }
        };
        loop {
            let page = {
                let _turn = self.gate.background().await;
                check_not_hot(&self.metrics, "poll_events");
                self.backend.poll_events(&since).await?
            };
            if page.refresh {
                tracing::info!("the server asked for a refresh; listing every listed directory");
                since = self.start_refresh().await?;
                *cursor = Some(since.clone());
                continue;
            }
            if !page.events.is_empty() {
                self.apply_events(&page.events).await?;
            }
            if page.next != since {
                if !self.events_refreshing() {
                    self.store_cursor(&page.next);
                }
                since = page.next;
                *cursor = Some(since.clone());
            }
            if !page.more {
                return Ok(());
            }
        }
    }

    /// Whether a refresh is listing directories again.
    pub fn events_refreshing(&self) -> bool {
        self.refresh.lock().running
    }

    /// Takes a fresh cursor and starts (or repeats) the refresh task.
    async fn start_refresh(self: &Arc<Self>) -> Result<EventId> {
        let fresh = {
            let _turn = self.gate.background().await;
            check_not_hot(&self.metrics, "latest_event_id");
            self.backend.latest_event_id().await?
        };
        let start = {
            let mut state = self.refresh.lock();
            state.again = state.running;
            !std::mem::replace(&mut state.running, true)
        };
        if start {
            let this = Arc::clone(self);
            self.runtime().spawn(async move { this.refresh().await });
        }
        Ok(fresh)
    }

    /// Lists every listed directory again until a pass succeeds with no
    /// refresh asked for meanwhile, then stores the cursor reached.
    async fn refresh(self: Arc<Self>) {
        loop {
            if let Err(error) = self.revalidate().await {
                tracing::warn!(%error, "refresh incomplete; listing again later");
                tokio::time::sleep(REFRESH_RETRY).await;
                continue;
            }
            // Refreshes start only under the cursor lock (`sync_events`),
            // so none can be asked for between this check and the end.
            let cursor = self.event_cursor.lock().await;
            {
                let mut state = self.refresh.lock();
                if std::mem::take(&mut state.again) {
                    continue;
                }
                state.running = false;
            }
            if let Some(cursor) = cursor.as_ref() {
                self.store_cursor(cursor);
            }
            tracing::info!("refresh finished");
            return;
        }
    }

    fn store_cursor(&self, cursor: &EventId) {
        let cursor = cursor.as_str().to_owned();
        self.store.write(move |conn| {
            conn.execute(
                "INSERT OR REPLACE INTO event_cursor(volume, event_id, updated_at)
                 VALUES (?1, ?2, strftime('%s','now'))",
                (MY_FILES_VOLUME, &cursor),
            )
            .map(drop)
        });
    }

    /// Lists again the listed directories that a page of events changes.
    async fn apply_events(self: &Arc<Self>, events: &[RemoteEvent]) -> Result<()> {
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
            let _turn = self.gate.background().await;
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
            let _turn = self.gate.background().await;
            match self.list(dir).await {
                // Removed by an earlier listing of this page.
                Ok(()) | Err(CoreError::NotFound) => {}
                Err(error) => return Err(error),
            }
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

    #[test]
    fn backoff_follows_the_official_steps_and_honours_retry_after() {
        let interval = Duration::from_secs(30);
        let other = CoreError::Internal("x".into());
        let steps: Vec<u64> = (0..9)
            .map(|failures| backoff(interval, failures, &other).as_secs())
            .collect();
        assert_eq!(steps, [30, 30, 60, 90, 150, 240, 390, 390, 390]);
        let limited = CoreError::Backend(BackendError::RateLimited {
            retry_after: Some(Duration::from_secs(600)),
        });
        assert_eq!(backoff(interval, 0, &limited), Duration::from_secs(600));
    }
}
