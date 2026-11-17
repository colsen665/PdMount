//! The background stages of the Photos tree: the timeline read, the events
//! of the Photos volume, and the names of the months asked for (see the
//! module doc of [`super`]).

use super::model::{Month, Node, PhotoMeta, month_name, year_name};
use super::{NAME_BATCH, PhotosTree, rows};
use crate::errors::{CoreError, Result};
use crate::gate::Turn;
use crate::health::Connectivity;
use crate::metrics::check_not_hot;
use crate::poller::backoff;
use crate::vfs::{Invalidation, ROOT_INO};
use futures::FutureExt;
use pdmount_backend::photos::{TIMELINE_PAGE, TimelineItem};
use pdmount_backend::{
    BackendError, EventId, EventKind, LinkId, NodeKind, NodeLookup, RemoteEvent, RemoteNode,
};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

/// The wait before a failed names batch is tried again, at the first
/// failure; it grows as the pollers' does.
const NAMES_RETRY: Duration = Duration::from_secs(5);

/// How long a request a user waits on waits for a turn of the session's
/// gate before it goes ahead without one (D119): the first timeline page
/// while nothing is shown yet, and a month's later names batches a lookup
/// waits on. Well within the 20 s metadata deadline, so a sustained load in
/// another tree slows them, never fails them.
pub(super) const USER_PATIENCE: Duration = Duration::from_secs(2);

/// Anchors of the last full page tried, newest last, when the page after
/// the last one comes back empty (that link went meanwhile): each is one
/// request; after them the read starts again from the first page.
const ANCHORS_KEPT: usize = 8;

/// Starts from the first page within one read attempt before it fails, and
/// goes on later from the first page with what it has seen.
const MAX_RESTARTS: usize = 3;

/// The least time between two searches for new photos that read more than
/// the timeline's first page (D119): a photo whose capture time is old is
/// found then; the first page, newest captures, is read at every poll.
pub(super) const DEEP_SEARCH_INTERVAL: Duration = Duration::from_secs(300);

/// The wait before a background task that panicked runs again.
const RESTART_DELAY: Duration = if cfg!(test) {
    Duration::from_millis(50)
} else {
    Duration::from_secs(5)
};

/// A timeline read under way, kept across failures: the next attempt goes
/// on from the last page read, with the cursor taken at the start (D119).
pub(super) struct TimelineRead {
    cursor: EventId,
    seen: HashSet<LinkId>,
    /// The last links of the last full page, oldest last.
    anchors: Vec<LinkId>,
    after: Option<LinkId>,
    pages: usize,
    restarts: usize,
    started: Instant,
}

impl PhotosTree {
    /// Runs `task` until it returns; one that panics is logged, counted
    /// (`status`) and run again after [`RESTART_DELAY`], so a stage never
    /// stops for the session. Aborting the handle stops it.
    pub(super) fn supervised<F, Fut>(
        self: &Arc<Self>,
        name: &'static str,
        task: F,
    ) -> tokio::task::JoinHandle<()>
    where
        F: Fn(Arc<Self>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let this = Arc::clone(self);
        self.runtime.spawn(async move {
            loop {
                match AssertUnwindSafe(task(Arc::clone(&this)))
                    .catch_unwind()
                    .await
                {
                    Ok(()) => return,
                    Err(panic) => {
                        let message = panic
                            .downcast_ref::<&str>()
                            .map(|text| (*text).to_owned())
                            .or_else(|| panic.downcast_ref::<String>().cloned())
                            .unwrap_or_default();
                        this.restarts.fetch_add(1, Ordering::Relaxed);
                        tracing::error!(
                            task = name,
                            panic = message,
                            "photos: a background task panicked; it runs again"
                        );
                        tokio::time::sleep(RESTART_DELAY).await;
                    }
                }
            }
        })
    }

    /// The timeline read, until one finishes, then the event stream at
    /// `sync.poll_interval`, with the pollers' back-off (D107, D110).
    pub(super) async fn run_timeline_and_events(self: Arc<Self>) {
        let mut failures = 0;
        loop {
            self.health.wait_online().await;
            let interval = self.config.sync.poll_interval.0;
            let reading = !self.timeline_complete.load(Ordering::Acquire);
            let result = if reading {
                self.read_timeline().await
            } else {
                self.poll_events().await
            };
            let delay = match result {
                // The events from where the read began, at once.
                Ok(()) if reading => {
                    failures = 0;
                    continue;
                }
                Ok(()) => {
                    failures = 0;
                    // The blocks' access times, for the cache's LRU after a
                    // restart, as the engine's background task does.
                    self.cache.flush_access();
                    interval
                }
                Err(_) if self.health.state() != Connectivity::Online => {
                    failures = 0;
                    continue;
                }
                Err(error) => {
                    let delay = backoff(interval, failures, &error);
                    failures += 1;
                    tracing::warn!(%error, ?delay, failures, reading, "photos: sync failed");
                    delay
                }
            };
            tokio::time::sleep(delay).await;
        }
    }

    /// A background turn for the timeline read: while the tree shows
    /// nothing yet, every listing of the top folder waits on it, so it
    /// waits for the gate at most [`USER_PATIENCE`] (D119).
    async fn timeline_turn(&self) -> Option<Turn<'_>> {
        let empty = self.model.read().years().is_empty();
        if empty {
            self.gate.background_within(USER_PATIENCE).await
        } else {
            Some(self.gate.background().await)
        }
    }

    /// Reads the whole timeline, newest first, a page per background turn,
    /// keeping the names it has; photos it no longer lists go, once a read
    /// has finished cleanly. The event cursor is taken first, so changes
    /// during the read come as events. A read that fails goes on from its
    /// last page at the next call.
    pub(super) async fn read_timeline(&self) -> Result<()> {
        let taken = self.reading.lock().take();
        let mut read = match taken {
            Some(read) => {
                tracing::info!(pages = read.pages, "photos: reading the timeline on");
                read
            }
            None => {
                let cursor = {
                    let _turn = self.timeline_turn().await;
                    check_not_hot(&self.metrics, "latest_event_id");
                    self.backend.latest_event_id().await?
                };
                tracing::info!("photos: reading the timeline");
                TimelineRead {
                    cursor,
                    seen: HashSet::new(),
                    anchors: Vec::new(),
                    after: None,
                    pages: 0,
                    restarts: 0,
                    started: Instant::now(),
                }
            }
        };
        if let Err(error) = self.read_pages(&mut read).await {
            *self.reading.lock() = Some(read);
            return Err(error);
        }
        let gone: Vec<LinkId> = {
            let model = self.model.read();
            model
                .links()
                .filter(|link| !read.seen.contains(*link))
                .cloned()
                .collect()
        };
        self.remove_links(&gone);
        rows::cursor(&self.store, &read.cursor, true);
        *self.cursor.lock() = Some(read.cursor);
        self.timeline_complete.store(true, Ordering::Release);
        self.changed();
        tracing::info!(
            photos = read.seen.len(),
            pages = read.pages,
            gone = gone.len(),
            elapsed_ms = read.started.elapsed().as_millis() as u64,
            "photos: timeline read"
        );
        Ok(())
    }

    /// The pages of a read, from where it stands to the timeline's end: a
    /// page shorter than [`TIMELINE_PAGE`] that is not empty. An empty page
    /// after a link means that link went since its page was read (the
    /// server answers so), not that the timeline ends (D119): the page after
    /// the link before it is asked for, and after [`ANCHORS_KEPT`] of them
    /// the first page again.
    async fn read_pages(&self, read: &mut TimelineRead) -> Result<()> {
        let mut restarts = 0;
        loop {
            let page = {
                let _turn = self.timeline_turn().await;
                check_not_hot(&self.metrics, "timeline_page");
                self.photos
                    .timeline_page(&self.volume, read.after.as_ref())
                    .await?
            };
            if page.is_empty() && read.after.is_some() {
                read.anchors.pop();
                read.after = read.anchors.last().cloned();
                if read.after.is_none() {
                    restarts += 1;
                    read.restarts += 1;
                    tracing::info!(
                        restarts = read.restarts,
                        "photos: the timeline's anchors went during the read; from the first page again"
                    );
                    if restarts > MAX_RESTARTS {
                        return Err(CoreError::Backend(BackendError::Transient(
                            "the timeline kept changing under the read".into(),
                        )));
                    }
                }
                continue;
            }
            read.pages += 1;
            self.apply_items(&page, &mut read.seen);
            if page.len() < TIMELINE_PAGE {
                return Ok(());
            }
            let skip = page.len().saturating_sub(ANCHORS_KEPT);
            read.anchors = page[skip..].iter().map(|item| item.link.clone()).collect();
            read.after = read.anchors.last().cloned();
        }
    }

    /// Puts timeline items in the model and the store; `seen` collects
    /// their links.
    fn apply_items(&self, items: &[TimelineItem], seen: &mut HashSet<LinkId>) {
        if items.is_empty() {
            return;
        }
        let mut months = BTreeSet::new();
        let rows = {
            let mut model = self.model.write();
            let mut links = Vec::new();
            for item in items {
                months.extend(model.put_item(item));
                links.push(item.link.clone());
                links.extend(item.related.iter().map(|related| related.link.clone()));
            }
            seen.extend(links.iter().cloned());
            links
                .into_iter()
                .filter_map(|link| Some((link.clone(), model.photo(&link)?.clone())))
                .collect::<Vec<_>>()
        };
        rows::put(&self.store, rows);
        self.folders_changed(&months);
    }

    /// After the model changed in `months`: the kernel's views of the
    /// folders go, waiters look again, and months opened earlier get the
    /// names of their new photos.
    fn folders_changed(&self, months: &BTreeSet<Month>) {
        let mut opened = Vec::new();
        {
            let model = self.model.read();
            self.invalidate_dir(ROOT_INO);
            let years: BTreeSet<i32> = months.iter().map(|(year, _)| *year).collect();
            for year in years {
                if let Some(ino) = model.ino_of(&Node::Year(year)) {
                    self.invalidate_dir(ino);
                }
            }
            for month in months {
                if let Some(ino) = model.ino_of(&Node::Month(*month)) {
                    self.invalidate_dir(ino);
                }
                if model.opened(*month) && !model.complete(*month) {
                    opened.push(*month);
                }
            }
        }
        for month in opened {
            self.want(month);
        }
        self.changed();
    }

    /// Removes photos (and their related ones) from the model and the store.
    pub(super) fn remove_links(&self, links: &[LinkId]) {
        if links.is_empty() {
            return;
        }
        let mut months = BTreeSet::new();
        let mut removed_links = Vec::new();
        {
            let mut model = self.model.write();
            for link in links {
                let removed = model.remove(link);
                for (month, ino, name) in removed.shown {
                    if let Some(parent) = model.ino_of(&Node::Month(month)) {
                        let _ = self
                            .invalidations
                            .send(Invalidation::Entry { parent, name });
                    }
                    let _ = self.invalidations.send(Invalidation::Inode { ino });
                }
                months.extend(removed.months);
                removed_links.extend(removed.links);
            }
        }
        rows::remove(&self.store, removed_links);
        self.folders_changed(&months);
    }

    /// Reads the nodes of `links` (a request per [`NAME_BATCH`], one
    /// background turn each) and sets their names, sizes and revisions; a
    /// link the server no longer has, or has trashed, goes; one it cannot
    /// decrypt stays as it was ([`PhotosTree::apply_nodes`]).
    pub(super) async fn fetch_nodes(&self, links: &[LinkId]) -> Result<()> {
        for batch in links.chunks(NAME_BATCH) {
            let nodes = {
                let _turn = self.gate.background().await;
                check_not_hot(&self.metrics, "get_nodes");
                self.backend.get_nodes_checked(batch).await?
            };
            self.apply_nodes(batch, nodes);
        }
        Ok(())
    }

    /// What one `get_nodes_checked` of `batch` said: names, sizes and
    /// revisions set; absent or trashed photos removed; a photo whose node
    /// cannot be decrypted keeps its name, or, without one, is no longer
    /// waited for (D119).
    fn apply_nodes(&self, batch: &[LinkId], nodes: Vec<NodeLookup>) {
        let mut gone = Vec::new();
        let mut metas = Vec::new();
        let mut unreadable = Vec::new();
        for (link, node) in batch.iter().zip(nodes) {
            match node {
                NodeLookup::Found(node) => match meta_of(&node) {
                    Some(meta) => metas.push((link.clone(), meta)),
                    None => gone.push(link.clone()),
                },
                NodeLookup::Absent => gone.push(link.clone()),
                NodeLookup::Degraded => unreadable.push(link.clone()),
            }
        }
        if !unreadable.is_empty() {
            tracing::warn!(
                photos = unreadable.len(),
                "photos: nodes that cannot be decrypted; kept as they were"
            );
            let months: BTreeSet<Month> = self
                .model
                .write()
                .set_unreadable(&unreadable)
                .into_iter()
                .collect();
            self.folders_changed(&months);
        }
        self.set_metas(metas);
        self.remove_links(&gone);
    }

    /// Sets names, sizes and revisions; the names that appear drop any
    /// absence the kernel cached.
    fn set_metas(&self, metas: Vec<(LinkId, PhotoMeta)>) {
        if metas.is_empty() {
            return;
        }
        let mut months = BTreeSet::new();
        let rows = {
            let mut model = self.model.write();
            let mut rows = Vec::new();
            for (link, meta) in metas {
                // A renamed photo's old name goes from the kernel too.
                if let (Some(month), Some(old)) = (model.month_of(&link), model.shown_name(&link))
                    && old != meta.name
                    && let Some(parent) = model.ino_of(&Node::Month(month))
                {
                    let _ = self.invalidations.send(Invalidation::Entry {
                        parent,
                        name: old.to_owned(),
                    });
                }
                if let Some(month) = model.set_meta(&link, Some(meta)) {
                    months.insert(month);
                }
                if let Some(photo) = model.photo(&link) {
                    rows.push((link.clone(), photo.clone()));
                }
                if let (Some(month), Some(name)) = (model.month_of(&link), model.shown_name(&link))
                    && let Some(parent) = model.ino_of(&Node::Month(month))
                {
                    let _ = self.invalidations.send(Invalidation::Entry {
                        parent,
                        name: name.to_owned(),
                    });
                }
            }
            rows
        };
        rows::put(&self.store, rows);
        self.folders_changed(&months);
    }

    /// Reads the volume's events from the cursor to their end, then tries
    /// the links whose events wait again. The cursor always moves past a
    /// page: what of it cannot be applied now waits in `pending` (D119).
    pub(super) async fn poll_events(&self) -> Result<()> {
        let stored = self.cursor.lock().clone();
        let mut since = match stored {
            Some(cursor) => cursor,
            None => {
                // A whole timeline without a cursor: read it again.
                self.timeline_complete.store(false, Ordering::Release);
                return self.read_timeline().await;
            }
        };
        let mut waited = self.pending.lock().clone();
        loop {
            let page = {
                let _turn = self.gate.background().await;
                check_not_hot(&self.metrics, "poll_events");
                self.backend.poll_events(&since).await?
            };
            if page.refresh {
                tracing::info!(
                    "photos: the server asked for a refresh; reading the timeline again"
                );
                self.timeline_complete.store(false, Ordering::Release);
                rows::timeline_stale(&self.store);
                *self.reading.lock() = None;
                return self.read_timeline().await;
            }
            for event in &page.events {
                waited.remove(&event.link_id);
            }
            self.apply_events(&page.events).await;
            if page.next != since {
                rows::cursor(&self.store, &page.next, false);
                *self.cursor.lock() = Some(page.next.clone());
                since = page.next;
            }
            if !page.more {
                break;
            }
        }
        // Those that waited from an earlier poll and had no event now.
        let waited: Vec<LinkId> = {
            let mut pending = self.pending.lock();
            waited.retain(|link| pending.remove(link));
            waited.into_iter().collect()
        };
        if !waited.is_empty() {
            tracing::debug!(links = waited.len(), "photos: events tried again");
            self.apply_links(waited).await;
            self.save_pending();
        }
        *self.last_poll.lock() = Some(Instant::now());
        Ok(())
    }

    /// Applies one page of events: the last event of each link counts. A
    /// later event of a link set aside as a stranger looks at it again
    /// (finding its commit after a draft, D119). Never fails: links whose
    /// nodes or timeline place cannot be read now wait in `pending`, saved
    /// before the cursor moves.
    pub(super) async fn apply_events(&self, events: &[RemoteEvent]) {
        let mut last: HashMap<&LinkId, &RemoteEvent> = HashMap::new();
        for event in events {
            if event.link_id != self.root {
                last.insert(&event.link_id, event);
            }
        }
        if last.is_empty() {
            return;
        }
        let (mut gone, mut links) = (Vec::new(), Vec::new());
        {
            let mut pending = self.pending.lock();
            let mut strangers = self.strangers.lock();
            for (link, event) in last {
                pending.remove(link);
                strangers.remove(link);
                if event.kind == EventKind::Delete || event.trashed {
                    gone.push(link.clone());
                } else {
                    links.push(link.clone());
                }
            }
        }
        self.remove_links(&gone);
        self.apply_links(links).await;
        self.save_pending();
    }

    /// Brings links up to date: a known photo with a name, or one whose
    /// node could not be decrypted, has its node read again (one without is
    /// named when its month is opened); a link the tree does not know is
    /// looked for ([`PhotosTree::find_new`]). What cannot be read now goes
    /// to `pending`.
    async fn apply_links(&self, links: Vec<LinkId>) {
        let (mut changed, mut unknown) = (Vec::new(), Vec::new());
        {
            let model = self.model.read();
            let strangers = self.strangers.lock();
            for link in links {
                match model.photo(&link) {
                    Some(photo) if photo.meta.is_some() || model.is_unreadable(&link) => {
                        changed.push(link);
                    }
                    Some(_) => {}
                    None if strangers.contains(&link) => {}
                    None => unknown.push(link),
                }
            }
        }
        let mut retry = Vec::new();
        if let Err(error) = self.fetch_nodes(&changed).await {
            tracing::warn!(%error, photos = changed.len(), "photos: changed photos not read; tried again at the next poll");
            retry.extend(changed);
        }
        retry.extend(self.find_new(unknown).await);
        self.pending.lock().extend(retry);
    }

    /// Writes `pending` to the store.
    fn save_pending(&self) {
        let links: Vec<LinkId> = self.pending.lock().iter().cloned().collect();
        rows::pending(&self.store, links);
    }

    /// Links the tree does not know: their nodes (a photo is a file in the
    /// Photos root), then their capture times from the timeline, newest
    /// pages first. Each page read updates the photos on it. Only the first
    /// page is read, unless the last search beyond it is
    /// [`DEEP_SEARCH_INTERVAL`] old (D119). A link the whole timeline does
    /// not list is set aside as a stranger until its next event; the links
    /// whose search could not finish come back, to wait in `pending`.
    async fn find_new(&self, links: Vec<LinkId>) -> Vec<LinkId> {
        let mut retry = Vec::new();
        let mut candidates: HashMap<LinkId, PhotoMeta> = HashMap::new();
        for batch in links.chunks(NAME_BATCH) {
            let nodes = {
                let _turn = self.gate.background().await;
                check_not_hot(&self.metrics, "get_nodes");
                self.backend.get_nodes_checked(batch).await
            };
            let nodes = match nodes {
                Ok(nodes) => nodes,
                Err(error) => {
                    tracing::warn!(%error, links = batch.len(), "photos: new links not read; tried again at the next poll");
                    retry.extend_from_slice(batch);
                    continue;
                }
            };
            for (link, node) in batch.iter().zip(nodes) {
                let node = match node {
                    NodeLookup::Found(node) => node,
                    // Gone again before it was read.
                    NodeLookup::Absent => continue,
                    // Looked at again at its next event.
                    NodeLookup::Degraded => {
                        self.strangers.lock().insert(link.clone());
                        continue;
                    }
                };
                if node.parent_link_id.as_ref() != Some(&self.root) {
                    continue;
                }
                match meta_of(&node) {
                    Some(meta) => {
                        candidates.insert(link.clone(), meta);
                    }
                    // An album, or a trashed photo.
                    None => {
                        self.strangers.lock().insert(link.clone());
                    }
                }
            }
        }
        if candidates.is_empty() {
            return retry;
        }
        let deep = self
            .last_deep_search
            .lock()
            .is_none_or(|at| at.elapsed() >= DEEP_SEARCH_INTERVAL);
        let mut seen = HashSet::new();
        let mut after: Option<LinkId> = None;
        let mut pages = 0;
        let mut complete = false;
        loop {
            let page = {
                let _turn = self.gate.background().await;
                check_not_hot(&self.metrics, "timeline_page");
                self.photos
                    .timeline_page(&self.volume, after.as_ref())
                    .await
            };
            let page = match page {
                Ok(page) => page,
                Err(error) => {
                    tracing::warn!(%error, "photos: the timeline not read for new photos; tried again at the next poll");
                    break;
                }
            };
            pages += 1;
            // Empty after a link: that link went; the search stops here.
            if page.is_empty() && after.is_some() {
                break;
            }
            self.apply_items(&page, &mut seen);
            if candidates.keys().all(|link| seen.contains(link)) {
                break;
            }
            match page.last() {
                Some(last) if page.len() >= TIMELINE_PAGE => {
                    if !deep {
                        break;
                    }
                    after = Some(last.link.clone());
                }
                _ => {
                    complete = true;
                    break;
                }
            }
        }
        if pages > 1 {
            *self.last_deep_search.lock() = Some(Instant::now());
        }
        let (found, missing): (Vec<_>, Vec<_>) = candidates
            .into_iter()
            .partition(|(link, _)| seen.contains(link));
        if !missing.is_empty() {
            let links = missing.into_iter().map(|(link, _)| link);
            if complete {
                tracing::debug!(
                    links = links.len(),
                    "photos: files in the Photos root that the timeline does not list; left out until their next event"
                );
                self.strangers.lock().extend(links);
            } else {
                retry.extend(links);
            }
        }
        if !found.is_empty() {
            tracing::info!(photos = found.len(), "photos: new photos from the events");
        }
        self.set_metas(found);
        retry
    }

    /// Fetches the names of the months asked for, the one asked for last
    /// first, a batch per background turn.
    pub(super) async fn run_names(self: Arc<Self>) {
        let mut failures = 0;
        loop {
            if self.next_batch().is_none() {
                self.wake.notified().await;
                continue;
            }
            self.health.wait_online().await;
            let result = {
                let _turn = self.gate.background().await;
                let _flight = self.names_flight.lock().await;
                // Again: a request a user waits on may have fetched it.
                let Some((month, batch)) = self.next_batch() else {
                    continue;
                };
                check_not_hot(&self.metrics, "get_nodes");
                match self.backend.get_nodes_checked(&batch).await {
                    Ok(nodes) => {
                        self.apply_nodes(&batch, nodes);
                        Ok(month)
                    }
                    Err(error) => Err(error),
                }
            };
            match result {
                Ok(month) => {
                    failures = 0;
                    self.names_fetched(month);
                }
                Err(_) if self.health.state() != Connectivity::Online => failures = 0,
                Err(error) => {
                    let error = CoreError::Backend(error);
                    let delay = backoff(NAMES_RETRY, failures, &error);
                    failures += 1;
                    tracing::warn!(%error, ?delay, "photos: fetching names failed");
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }

    /// After a names batch of `month`: once it has them all, the thumbnail
    /// hook hears of it.
    fn names_fetched(&self, month: Month) {
        let model = self.model.read();
        if !model.complete(month) {
            return;
        }
        let named = model.entries(month).len();
        let views = Self::views(&model, month);
        drop(model);
        tracing::info!(
            month = format!("{}-{}", year_name(month.0), month_name(month.1)),
            photos = named,
            "photos: names of a month fetched"
        );
        if let Some(hook) = self.hook.read().clone() {
            hook.month_opened(month, views);
        }
    }

    /// The next names batch of `month` for a request a user waits on (an
    /// `ls` or a lookup in it, D119). The first batch of a month without
    /// names is the user's own work, outside the gate, as the engine lists a
    /// folder; a later one takes a turn of the gate, waited for at most
    /// [`USER_PATIENCE`]. Returns whether the month changed meanwhile.
    pub(super) async fn fetch_for_user(&self, month: Month) -> Result<bool> {
        let first = self.model.read().entries(month).is_empty();
        let _turn = if first {
            None
        } else {
            self.gate.background_within(USER_PATIENCE).await
        };
        let _wait = first.then(|| self.user_wait());
        let _flight = self.names_flight.lock().await;
        let batch: Vec<LinkId> = {
            let model = self.model.read();
            if first && !model.entries(month).is_empty() {
                // Its first batch came while this waited.
                return Ok(true);
            }
            model.unnamed(month).into_iter().take(NAME_BATCH).collect()
        };
        if batch.is_empty() {
            return Ok(true);
        }
        check_not_hot(&self.metrics, "get_nodes");
        let nodes = self.backend.get_nodes_checked(&batch).await?;
        self.apply_nodes(&batch, nodes);
        self.names_fetched(month);
        Ok(true)
    }

    /// The next batch of names to fetch: of the month asked for last that
    /// still lacks some. Months that have them all leave the list.
    pub(super) fn next_batch(&self) -> Option<(Month, Vec<LinkId>)> {
        let model = self.model.read();
        let mut wanted = self.wanted.lock();
        while let Some(month) = wanted.last().copied() {
            let unnamed = model.unnamed(month);
            if unnamed.is_empty() {
                wanted.pop();
                continue;
            }
            return Some((month, unnamed.into_iter().take(NAME_BATCH).collect()));
        }
        None
    }
}

/// A photo's name, size and revision from its node; `None` for anything
/// but a file that is not trashed.
fn meta_of(node: &RemoteNode) -> Option<PhotoMeta> {
    if node.kind != NodeKind::File || node.trashed {
        return None;
    }
    Some(PhotoMeta {
        name: node.name.clone(),
        size: node.size.unwrap_or(0),
        revision: node.active_revision.clone()?,
    })
}
