//! One event poller per volume (DESIGN.md §9.2, §14.5; D080, D110, D112).
//!
//! A tree (My Files, a device, a share) has its own engine, but trees can
//! share a volume: a device's tree lives on the user's own volume, and two
//! shares of one owner on the owner's. Two engines polling one volume would
//! double the traffic (`cs/README.md`), so a [`Poller`] reads each volume's
//! stream once and hands every page to all the trees on it. Each tree still
//! keeps its own cursor in its own store, written after the rows a page
//! changed (§9.1): a pass locks the cursors of all its trees (in tree order,
//! before any gate turn), polls from the first tree's cursor, and a tree whose
//! cursor differs (a tree added later, a crash between two stores) or that
//! failed to apply a page refreshes from the common cursor (§9.4) instead of
//! holding the others back. Only a page every tree failed is retried, and
//! not for ever: after [`STUCK_PAGE_LIMIT`] failures of the same page with a
//! non-transient error ([`gives_up_page`]), the pass lists every tree again
//! from a fresh cursor, as after a server's `refresh` (M6.0.3 V-3: one node
//! that cannot be decrypted must not stop a tree's remote changes for good).
//!
//! Intervals (D112, as the official `EventScheduler`): the user's own volume
//! every `sync.poll_interval`; other users' volumes every
//! [`OTHER_VOLUME_INTERVAL`], except the one whose tree was used last
//! through a mount ([`Cadence::touch`]), which polls like the own volume
//! until [`QUIET_PERIOD`] passes without a touch (the official
//! `setBackground`). Every poller polls once at its start, and a poller
//! whose volume becomes the one in use, or is touched again after the quiet
//! period, recomputes its wait at once. Every wait adds up to [`JITTER`] at
//! random ([`pass_wait`]), as the official `EventScheduler`'s does, so the
//! pollers of a daemon do not stay in step. After failures the interval is
//! multiplied by 1, 1, 2, 3, 5, 8, 13, as the official event manager does
//! (`cs/client/js/src/internal/events/eventManager.ts:4`), plus the jitter.
//!
//! Share membership, roles and revocations arrive on the core event stream
//! (M5.9), not on a volume's. While a Shared mount listens
//! ([`Pollers::watch_core`], M5.12, D112) one task polls that stream every
//! `sync.poll_interval` (the official client polls it every 30 s) and calls
//! [`Pollers::share_refresh`] on a `DriveShareRefresh` event or a `refresh`.

use crate::engine::Engine;
use crate::errors::{CoreError, Result};
use crate::health::{Connectivity, Health};
use crate::metrics::check_not_hot;
use pdmount_backend::shared::SharedWithMe;
use pdmount_backend::{BackendError, EventId};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Multiples of the poll interval after consecutive failures
/// (`cs/client/js/src/internal/events/eventManager.ts:4`).
const BACKOFF_STEPS: [u32; 7] = [1, 1, 2, 3, 5, 8, 13];
/// The most a wait is lengthened at random (the official `EventScheduler`'s
/// `JITTER_SECONDS`).
pub const JITTER: Duration = Duration::from_secs(1);
/// Other users' volumes, unless in use (D112; the official client's
/// `EventScheduler`).
pub const OTHER_VOLUME_INTERVAL: Duration = Duration::from_secs(600);
/// How long the volume in use stays so without a touch before it goes back
/// to [`OTHER_VOLUME_INTERVAL`] (D112, parity E2), as the official client's
/// `setBackground` when a share's view is left; a mount sees only requests.
/// Five minutes keep the short interval through a pause in work (reading,
/// editing between saves) and cost at most ten polls of a share no longer
/// used; a touch after it polls at once anyway.
pub const QUIET_PERIOD: Duration = Duration::from_secs(300);
/// Failures of one event page, with a non-transient error, after which a
/// pass gives the page up and refreshes from a fresh cursor (§9.2).
pub(crate) const STUCK_PAGE_LIMIT: u32 = 3;

/// Whether a failure to read or apply a page says something about the page:
/// an error the backend gives again whatever the wait. Transport failures,
/// 429 and 5xx (`Transient`, `RateLimited`), no connection and no session
/// (`Offline`, `Unauthorized`) pass on their own, and errors that are not
/// the backend's (the local store, a deadline) are not the page's.
fn is_page_error(error: &CoreError) -> bool {
    matches!(error, CoreError::Backend(error) if error.is_permanent())
}

/// Counts a failure of the page at `since` on the lead tree; `true` when
/// the page has now failed [`STUCK_PAGE_LIMIT`] times with a page error,
/// and the count starts again. A transient failure neither counts nor
/// resets: the page is still in the way once it passes.
pub(crate) fn gives_up_page(lead: &Engine, since: &EventId, error: &CoreError) -> bool {
    if !is_page_error(error) {
        return false;
    }
    let mut stuck = lead.stuck_page.lock();
    let failures = match &mut *stuck {
        Some((page, failures)) if page == since => {
            *failures += 1;
            *failures
        }
        _ => {
            *stuck = Some((since.clone(), 1));
            1
        }
    };
    if failures < STUCK_PAGE_LIMIT {
        return false;
    }
    *stuck = None;
    true
}

/// The random part of a wait of `interval`, for a `fraction` in 0..1: up to
/// [`JITTER`], as the official `EventScheduler` adds to every wait; under an
/// interval of 30 s (tests only, the configuration's floor is 30 s) up to a
/// thirtieth of it, so a short test interval does not gain a second a pass.
pub(crate) fn jitter(interval: Duration, fraction: f64) -> Duration {
    JITTER.min(interval / 30).mul_f64(fraction.clamp(0.0, 1.0))
}

/// The wait after a successful pass: `interval` plus up to [`jitter`], so
/// the pollers of a daemon do not stay in step (parity E1). `fraction` is
/// drawn once a pass, so a recomputed wait keeps it.
pub(crate) fn pass_wait(interval: Duration, fraction: f64) -> Duration {
    interval + jitter(interval, fraction)
}

/// How long to wait after `failures` failed polls in a row (0 = the first).
pub(crate) fn backoff(interval: Duration, failures: usize, error: &CoreError) -> Duration {
    let step = BACKOFF_STEPS[failures.min(BACKOFF_STEPS.len() - 1)];
    let delay = interval * step + jitter(interval, rand::random::<f64>());
    match error {
        CoreError::Backend(BackendError::RateLimited {
            retry_after: Some(after),
        }) => delay.max(*after),
        _ => delay,
    }
}

/// The volume used last through a mount, shared by a daemon's pollers
/// (D112: one volume of another user at a time polls like the own one,
/// until [`QUIET_PERIOD`] passes without a touch).
#[derive(Debug)]
pub struct Cadence {
    foreground: tokio::sync::watch::Sender<Option<Arc<str>>>,
    /// The last touch, in milliseconds since `epoch`.
    touched: AtomicU64,
    epoch: Instant,
    quiet: Duration,
}

impl Default for Cadence {
    fn default() -> Self {
        Self::with_quiet_period(QUIET_PERIOD)
    }
}

/// A touch within this of the last one written is not written again (the
/// FUSE layer touches on every lookup, open and listing), and within a
/// hundredth of a shorter quiet period.
const TOUCH_GRAIN_MS: u64 = 1000;

impl Cadence {
    /// A cadence whose volume in use goes back after `quiet` without a touch
    /// ([`QUIET_PERIOD`] by default; tests shorten it).
    pub fn with_quiet_period(quiet: Duration) -> Self {
        Self {
            foreground: tokio::sync::watch::channel(None).0,
            touched: AtomicU64::new(0),
            epoch: Instant::now(),
            quiet,
        }
    }

    fn now_ms(&self) -> u64 {
        u64::try_from(self.epoch.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    fn quiet_ms(&self) -> u64 {
        u64::try_from(self.quiet.as_millis()).unwrap_or(u64::MAX)
    }

    /// Whether the last touch is the quiet period or more before `now_ms`.
    fn quiet_at(&self, now_ms: u64) -> bool {
        now_ms.saturating_sub(self.touched.load(Ordering::Acquire)) >= self.quiet_ms()
    }

    /// Marks `volume` as the one in use (a lookup, an open or a listing in
    /// one of its trees). Cheap when it is already: the FUSE layer calls it
    /// on those requests. A touch that makes a volume the one in use, a
    /// different one or the same after the quiet period, wakes the pollers.
    pub fn touch(&self, volume: &Arc<str>) {
        let now = self.now_ms();
        let same = self.foreground.borrow().as_ref() == Some(volume);
        if same && !self.quiet_at(now) {
            let grain = TOUCH_GRAIN_MS.min(self.quiet_ms() / 100);
            if now.saturating_sub(self.touched.load(Ordering::Relaxed)) >= grain {
                self.touched.fetch_max(now, Ordering::AcqRel);
            }
            return;
        }
        // Written before the wake-up, so a woken poller sees it in use.
        self.touched.fetch_max(now, Ordering::AcqRel);
        self.foreground.send_modify(|current| {
            *current = Some(Arc::clone(volume));
        });
    }

    /// The volume in use, if any (none once the quiet period has passed).
    pub fn foreground(&self) -> Option<Arc<str>> {
        if self.quiet_at(self.now_ms()) {
            return None;
        }
        self.foreground.borrow().clone()
    }

    fn is_foreground(&self, volume: &str) -> bool {
        self.foreground.borrow().as_deref() == Some(volume) && !self.quiet_at(self.now_ms())
    }

    fn subscribe(&self) -> tokio::sync::watch::Receiver<Option<Arc<str>>> {
        self.foreground.subscribe()
    }
}

/// What a mount of a further tree tells the pollers about its use: the
/// tree's volume and the daemon's [`Cadence`].
#[derive(Debug, Clone)]
pub struct VolumeUse {
    cadence: Arc<Cadence>,
    volume: Arc<str>,
}

impl VolumeUse {
    pub fn new(cadence: Arc<Cadence>, volume: &str) -> Self {
        Self {
            cadence,
            volume: Arc::from(volume),
        }
    }

    /// The tree was used through its mount.
    pub fn touch(&self) {
        self.cadence.touch(&self.volume);
    }
}

/// The poller of one volume: the trees on it, and how often it polls.
pub struct Poller {
    volume: String,
    /// The user's own volume (My Files', and devices'): never the long
    /// interval of other users' volumes.
    own: bool,
    trees: parking_lot::Mutex<Vec<Arc<Engine>>>,
    cadence: Arc<Cadence>,
    /// A tree was added: poll now, so it catches up.
    added: tokio::sync::Notify,
}

impl Poller {
    pub fn new(volume: &str, own: bool, cadence: Arc<Cadence>) -> Arc<Self> {
        Arc::new(Self {
            volume: volume.to_owned(),
            own,
            trees: parking_lot::Mutex::default(),
            cadence,
            added: tokio::sync::Notify::new(),
        })
    }

    pub fn volume(&self) -> &str {
        &self.volume
    }

    /// Adds a tree; the next pass starts at once.
    pub fn add(&self, engine: Arc<Engine>) {
        self.trees.lock().push(engine);
        self.added.notify_one();
    }

    /// Removes a tree (its mount is going); `true` when none is left.
    pub fn remove(&self, engine: &Arc<Engine>) -> bool {
        let mut trees = self.trees.lock();
        trees.retain(|tree| !Arc::ptr_eq(tree, engine));
        trees.is_empty()
    }

    pub fn trees(&self) -> Vec<Arc<Engine>> {
        self.trees.lock().clone()
    }

    /// The wait between two passes now (D112), from the first tree's
    /// `sync.poll_interval`.
    pub fn interval(&self, poll_interval: Duration) -> Duration {
        if self.own || self.cadence.is_foreground(&self.volume) {
            poll_interval
        } else {
            OTHER_VOLUME_INTERVAL.max(poll_interval)
        }
    }

    /// Polls until the task is aborted. Errors never end the loop.
    pub async fn run(self: Arc<Self>) {
        let mut foreground = self.cadence.subscribe();
        let mut failures = 0;
        loop {
            let trees = self.trees();
            let Some(lead) = trees.first().cloned() else {
                self.added.notified().await;
                continue;
            };
            let poll_interval = lead.config().sync.poll_interval.0;
            // After a lost connection the first poll comes at once.
            lead.health().wait_online().await;
            let delay = match sync_trees(&self.volume, &trees).await {
                Ok(()) => {
                    failures = 0;
                    None
                }
                Err(_) if lead.health().state() != Connectivity::Online => {
                    failures = 0;
                    continue;
                }
                Err(error) => {
                    let delay = backoff(self.interval(poll_interval), failures, &error);
                    failures += 1;
                    tracing::warn!(volume = self.volume, %error, ?delay, failures, "event sync failed");
                    Some(delay)
                }
            };
            let started = Instant::now();
            let fraction = rand::random::<f64>();
            loop {
                // Recomputed after each change of the volume in use, and when
                // the wait ends: the volume may have gone quiet meanwhile
                // (QUIET_PERIOD), and then waits the long interval.
                let wait =
                    delay.unwrap_or_else(|| pass_wait(self.interval(poll_interval), fraction));
                let left = (started + wait).saturating_duration_since(Instant::now());
                if left.is_zero() {
                    break;
                }
                tokio::select! {
                    () = tokio::time::sleep(left) => {}
                    () = self.added.notified() => break,
                    changed = foreground.changed() => {
                        if changed.is_err() {
                            // No cadence any more: keep the current wait.
                            tokio::time::sleep(left).await;
                            break;
                        }
                    }
                }
            }
        }
    }
}

/// Reads the volume's stream to its end once and hands every page to all
/// `trees` (all on that volume, named `volume` in the log; the first
/// leads). See the module doc.
pub(crate) async fn sync_trees(volume: &str, trees: &[Arc<Engine>]) -> Result<()> {
    // Lock order: the cursors in tree order, then gate turns.
    let mut locked = Vec::with_capacity(trees.len());
    for tree in trees {
        locked.push(tree.event_cursor.lock().await);
    }
    // A tree retired before its cursor was taken (a gone share whose
    // removal waits for this pass, D114) is left out: nothing may change in
    // its state any more.
    let (trees, mut cursors): (Vec<&Arc<Engine>>, Vec<_>) = trees
        .iter()
        .zip(locked)
        .filter(|(tree, _)| !tree.is_retired())
        .unzip();
    let Some(&lead) = trees.first() else {
        return Ok(());
    };
    let mut since = match cursors[0].clone() {
        Some(since) => since,
        None => {
            let fresh = lead.fresh_cursor().await?;
            lead.begin_refresh();
            *cursors[0] = Some(fresh.clone());
            fresh
        }
    };
    for (tree, cursor) in trees.iter().zip(cursors.iter_mut()).skip(1) {
        if cursor.as_ref() != Some(&since) {
            tracing::info!("a tree's event cursor differs from its volume's; listing it again");
            tree.begin_refresh();
            **cursor = Some(since.clone());
        }
    }
    loop {
        let page = {
            // Patient: a steady load must not stop the sync (D120).
            let _turn = lead
                .gate
                .background_patient(crate::events::EVENT_PATIENCE)
                .await;
            check_not_hot(&lead.metrics, "poll_events");
            lead.backend.poll_events(&since).await
        };
        let page = match page {
            Ok(page) => page,
            Err(error) => {
                let error = CoreError::from(error);
                if !gives_up_page(lead, &since, &error) {
                    return Err(error);
                }
                since = give_up_page(volume, lead, &trees, &mut cursors, &error).await?;
                continue;
            }
        };
        if page.refresh {
            tracing::info!("the server asked for a refresh; listing every listed directory");
            let fresh = lead.fresh_cursor().await?;
            for (tree, cursor) in trees.iter().zip(cursors.iter_mut()) {
                tree.begin_refresh();
                **cursor = Some(fresh.clone());
            }
            since = fresh;
            continue;
        }
        if !page.events.is_empty() {
            let mut failed = Vec::new();
            let mut last_error = None;
            for (index, tree) in trees.iter().enumerate() {
                if let Err(error) = tree.apply_events(&page.events).await {
                    failed.push(index);
                    last_error = Some(error);
                }
            }
            if let Some(error) = last_error {
                // Every tree failed: nothing advances, the page comes again,
                // unless it failed too often (see the module doc).
                if failed.len() == trees.len() {
                    if !gives_up_page(lead, &since, &error) {
                        return Err(error);
                    }
                    since = give_up_page(volume, lead, &trees, &mut cursors, &error).await?;
                    continue;
                }
                // Some did: those that failed list again from the new cursor
                // and the others go on (one tree failing does not stop another).
                tracing::warn!(%error, trees = failed.len(), "a tree could not apply an event page; listing it again");
                for index in failed {
                    trees[index].begin_refresh();
                }
            }
        }
        if page.next != since {
            for (tree, cursor) in trees.iter().zip(cursors.iter_mut()) {
                if !tree.events_refreshing() {
                    tree.store_cursor(&page.next);
                }
                **cursor = Some(page.next.clone());
            }
            since = page.next;
        }
        if !page.more {
            for tree in &trees {
                tree.events_caught_up();
            }
            return Ok(());
        }
    }
}

/// The page at the trees' cursor failed [`STUCK_PAGE_LIMIT`] times: every
/// tree lists again from a fresh cursor, which the pass goes on from. The
/// cursor is stored once each refresh is done (§9.4).
async fn give_up_page(
    volume: &str,
    lead: &Arc<Engine>,
    trees: &[&Arc<Engine>],
    cursors: &mut [tokio::sync::MutexGuard<'_, Option<EventId>>],
    error: &CoreError,
) -> Result<EventId> {
    tracing::warn!(
        volume,
        %error,
        failures = STUCK_PAGE_LIMIT,
        "an event page keeps failing; skipping it and listing every tree again from a fresh cursor"
    );
    let fresh = lead.fresh_cursor().await?;
    for (tree, cursor) in trees.iter().zip(cursors.iter_mut()) {
        tree.begin_refresh();
        **cursor = Some(fresh.clone());
    }
    Ok(fresh)
}

/// Told when the core event stream says that shares changed
/// (`DriveShareRefresh`, M5.9): a Shared mount reads its listing and the
/// share roots again (M5.12).
#[async_trait::async_trait]
pub trait ShareListener: Send + Sync {
    async fn shares_changed(&self);

    /// The last listing failed: read it again at the next successful poll
    /// of the core stream, signal or not.
    fn stale(&self) -> bool {
        false
    }
}

/// Core event pages read in one poll, at most.
const MAX_CORE_PAGES: usize = 16;

type Listeners = Arc<parking_lot::Mutex<Vec<Arc<dyn ShareListener>>>>;

/// A volume's poller and its task.
type Running = (Arc<Poller>, tokio::task::JoinHandle<()>);

/// The pollers of a daemon, one per volume, and the [`Cadence`] they share.
pub struct Pollers {
    cadence: Arc<Cadence>,
    runtime: tokio::runtime::Handle,
    volumes: parking_lot::Mutex<HashMap<String, Running>>,
    listeners: Listeners,
    /// The core stream's task while a listener is registered.
    core: parking_lot::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

/// One volume's poller, for `status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeView {
    pub volume: String,
    pub trees: usize,
    pub interval: Duration,
}

impl Pollers {
    /// Must be called inside a tokio runtime.
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self {
            cadence: Arc::new(Cadence::default()),
            runtime: tokio::runtime::Handle::current(),
            volumes: parking_lot::Mutex::default(),
            listeners: Listeners::default(),
            core: parking_lot::Mutex::default(),
        }
    }

    pub fn cadence(&self) -> &Arc<Cadence> {
        &self.cadence
    }

    /// Adds a tree on `volume` (`own`: the user's own volume), starting the
    /// volume's poller if it has none. The engine must have been started
    /// with [`Engine::start_background_polled`].
    pub fn add(&self, volume: &str, own: bool, engine: Arc<Engine>) {
        let mut volumes = self.volumes.lock();
        let (poller, _) = volumes.entry(volume.to_owned()).or_insert_with(|| {
            let poller = Poller::new(volume, own, Arc::clone(&self.cadence));
            let task = self.runtime.spawn(Arc::clone(&poller).run());
            (poller, task)
        });
        poller.add(engine);
    }

    /// Removes a tree; a poller with no tree left stops.
    pub fn remove(&self, engine: &Arc<Engine>) {
        let mut volumes = self.volumes.lock();
        volumes.retain(|_, (poller, task)| {
            let empty = poller.remove(engine);
            if empty {
                task.abort();
            }
            !empty
        });
    }

    /// The pollers and their current intervals, by volume.
    pub fn view(&self) -> Vec<VolumeView> {
        let volumes = self.volumes.lock();
        let mut views: Vec<VolumeView> = volumes
            .values()
            .map(|(poller, _)| {
                let trees = poller.trees();
                let poll_interval = trees.first().map_or(OTHER_VOLUME_INTERVAL, |lead| {
                    lead.config().sync.poll_interval.0
                });
                VolumeView {
                    volume: poller.volume.clone(),
                    trees: trees.len(),
                    interval: poller.interval(poll_interval),
                }
            })
            .collect();
        views.sort_by(|a, b| a.volume.cmp(&b.volume));
        views
    }

    /// Registers a listener for [`Pollers::share_refresh`].
    pub fn on_share_refresh(&self, listener: Arc<dyn ShareListener>) {
        self.listeners.lock().push(listener);
    }

    /// The volume interval of each volume, for `status`.
    pub fn intervals(&self) -> HashMap<String, Duration> {
        self.view()
            .into_iter()
            .map(|view| (view.volume, view.interval))
            .collect()
    }

    /// Registers `listener` and, unless it runs, starts the core stream's
    /// task: every `interval` while `health` is online, it polls the core
    /// stream of `source` from `since` (the latest event when `None`) and
    /// calls the listeners on a `DriveShareRefresh` or a `refresh`
    /// ([`Pollers::share_refresh`]). One core stream per daemon (M5.12).
    pub fn watch_core(
        &self,
        listener: Arc<dyn ShareListener>,
        source: Arc<dyn SharedWithMe>,
        health: Arc<Health>,
        interval: Duration,
        since: Option<EventId>,
    ) {
        self.on_share_refresh(listener);
        let mut core = self.core.lock();
        if core.as_ref().is_some_and(|task| !task.is_finished()) {
            return;
        }
        let listeners = Arc::clone(&self.listeners);
        *core = Some(
            self.runtime
                .spawn(run_core(source, health, interval, since, listeners)),
        );
    }

    /// Removes `listener`; the core stream's task stops with the last one.
    pub fn unwatch_core(&self, listener: &Arc<dyn ShareListener>) {
        let mut listeners = self.listeners.lock();
        listeners.retain(|other| !Arc::ptr_eq(other, listener));
        if listeners.is_empty()
            && let Some(task) = self.core.lock().take()
        {
            task.abort();
        }
    }

    /// The hook of the core stream's `DriveShareRefresh` (M5.9, M5.12).
    pub async fn share_refresh(&self) {
        notify_listeners(&self.listeners, true).await;
    }

    /// Stops every poller and the core stream's task.
    pub fn stop(&self) {
        for (_, (_, task)) in self.volumes.lock().drain() {
            task.abort();
        }
        if let Some(task) = self.core.lock().take() {
            task.abort();
        }
    }
}

/// Calls the listeners: all on a signal, else those whose last listing
/// failed.
async fn notify_listeners(listeners: &Listeners, signalled: bool) {
    let listeners = listeners.lock().clone();
    for listener in listeners {
        if signalled || listener.stale() {
            listener.shares_changed().await;
        }
    }
}

/// Reads the core stream from `cursor` to its end (at most
/// [`MAX_CORE_PAGES`] pages); whether shares changed. Without a cursor it
/// takes the latest event and reports nothing.
pub(crate) async fn poll_core(
    source: &dyn SharedWithMe,
    cursor: &mut Option<EventId>,
) -> Result<bool> {
    let Some(mut since) = cursor.clone() else {
        *cursor = Some(source.latest_core_event_id().await?);
        return Ok(false);
    };
    let mut signalled = false;
    for _ in 0..MAX_CORE_PAGES {
        let page = source.poll_core_events(&since).await?;
        signalled |= page.shares_changed || page.refresh;
        since = page.next;
        *cursor = Some(since.clone());
        if !page.more {
            break;
        }
    }
    Ok(signalled)
}

/// The core stream's task (see [`Pollers::watch_core`]).
async fn run_core(
    source: Arc<dyn SharedWithMe>,
    health: Arc<Health>,
    interval: Duration,
    since: Option<EventId>,
    listeners: Listeners,
) {
    let mut cursor = since;
    let mut failures = 0;
    loop {
        // After a lost connection the first poll comes at once.
        health.wait_online().await;
        let delay = match poll_core(&*source, &mut cursor).await {
            Ok(signalled) => {
                failures = 0;
                if signalled {
                    tracing::info!("the core event stream says shares changed");
                }
                notify_listeners(&listeners, signalled).await;
                interval + JITTER.mul_f64(rand::random::<f64>())
            }
            Err(_) if health.state() != Connectivity::Online => {
                failures = 0;
                continue;
            }
            Err(error) => {
                let delay = backoff(interval, failures, &error);
                failures += 1;
                tracing::warn!(%error, ?delay, failures, "core event poll failed");
                delay
            }
        };
        tokio::time::sleep(delay).await;
    }
}

impl Drop for Pollers {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Config;
    use crate::conflicts::system_clock;
    use crate::device::{DeviceIdentity, Uuid};
    use crate::engine::EngineOptions;
    use crate::metrics::Op;
    use crate::units::HumanDuration;
    use bytes::Bytes;
    use pdmount_backend::fake::{FakeBackend, FakeOp, FakeRole};
    use pdmount_backend::{Backend, FileMeta, LinkId, UploadSource};
    use std::time::UNIX_EPOCH;

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

    /// Parity E1: every wait after a successful pass is the interval plus
    /// 0–1 s at random, as the official `EventScheduler`; a test interval
    /// under 30 s gains at most a thirtieth of it.
    #[test]
    fn every_wait_has_up_to_a_second_of_jitter() {
        for interval in [Duration::from_secs(30), OTHER_VOLUME_INTERVAL] {
            assert_eq!(pass_wait(interval, 0.0), interval);
            assert_eq!(pass_wait(interval, 0.5), interval + JITTER / 2);
            assert!(pass_wait(interval, 1.0) <= interval + JITTER);
            let waits: Vec<Duration> = (0..1000)
                .map(|_| pass_wait(interval, rand::random::<f64>()))
                .collect();
            for wait in &waits {
                assert!(*wait >= interval && *wait < interval + JITTER, "{wait:?}");
            }
            // Spread over the second, not a constant.
            let spread = *waits.iter().max().expect("waits") - *waits.iter().min().expect("waits");
            assert!(spread > JITTER / 2, "{spread:?}");
        }
        let short = Duration::from_millis(300);
        for _ in 0..1000 {
            let wait = pass_wait(short, rand::random::<f64>());
            assert!(wait >= short && wait < short + short / 30, "{wait:?}");
        }
        // The back-off keeps the jitter.
        let other = CoreError::Internal("x".into());
        let interval = Duration::from_secs(30);
        let delay = backoff(interval, 0, &other);
        assert!(delay >= interval && delay < interval + JITTER, "{delay:?}");
    }

    #[tokio::test]
    async fn other_volumes_poll_every_ten_minutes_unless_in_use() {
        let cadence = Arc::new(Cadence::default());
        let own = Poller::new("mine", true, Arc::clone(&cadence));
        let other = Poller::new("theirs", false, Arc::clone(&cadence));
        let third = Poller::new("third", false, Arc::clone(&cadence));
        let base = Duration::from_secs(30);
        assert_eq!(own.interval(base), base);
        assert_eq!(other.interval(base), OTHER_VOLUME_INTERVAL);
        // The volume used last polls like the own one; one at a time.
        VolumeUse::new(Arc::clone(&cadence), "theirs").touch();
        assert_eq!(other.interval(base), base);
        assert_eq!(third.interval(base), OTHER_VOLUME_INTERVAL);
        VolumeUse::new(Arc::clone(&cadence), "third").touch();
        assert_eq!(other.interval(base), OTHER_VOLUME_INTERVAL);
        assert_eq!(third.interval(base), base);
        // Using the own volume changes nothing for the others.
        assert_eq!(own.interval(base), base);
        assert_eq!(cadence.foreground().as_deref(), Some("third"));
    }

    /// D112, parity E2: the volume in use goes back to the long interval
    /// once the quiet period passes without a touch, as the official
    /// `setBackground`; touches within it keep it, and a touch after it
    /// brings it forward again.
    #[tokio::test]
    async fn the_volume_in_use_goes_back_after_the_quiet_period() {
        let quiet = Duration::from_millis(400);
        let cadence = Arc::new(Cadence::with_quiet_period(quiet));
        let other = Poller::new("theirs", false, Arc::clone(&cadence));
        let own = Poller::new("mine", true, Arc::clone(&cadence));
        let base = Duration::from_secs(30);
        let theirs = VolumeUse::new(Arc::clone(&cadence), "theirs");
        assert_eq!(QUIET_PERIOD, Duration::from_secs(300));
        assert_eq!(other.interval(base), OTHER_VOLUME_INTERVAL);
        theirs.touch();
        assert_eq!(other.interval(base), base);
        // Touched more often than the quiet period: it stays in use.
        for _ in 0..4 {
            tokio::time::sleep(quiet / 4).await;
            theirs.touch();
        }
        assert_eq!(other.interval(base), base);
        assert_eq!(cadence.foreground().as_deref(), Some("theirs"));
        // Quiet: back to the long interval, none in use.
        tokio::time::sleep(quiet + Duration::from_millis(100)).await;
        assert_eq!(other.interval(base), OTHER_VOLUME_INTERVAL);
        assert_eq!(cadence.foreground(), None);
        assert_eq!(own.interval(base), base, "the own volume never goes back");
        // A touch brings it forward again.
        let mut woken = cadence.subscribe();
        woken.mark_unchanged();
        theirs.touch();
        assert!(
            woken.has_changed().expect("cadence"),
            "the pollers are woken"
        );
        assert_eq!(other.interval(base), base);
        assert_eq!(cadence.foreground().as_deref(), Some("theirs"));
    }

    /// The same through a running poller on the fake: a volume not in use
    /// waits the long interval; a touch polls it at once and then at the
    /// short one while touched; after the quiet period it stops polling, and
    /// a touch polls it at once again.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_running_poller_follows_the_volume_in_use() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fake = FakeBackend::new();
        let share = fake
            .add_root("Team", Some("owner@example.test"), Some(FakeRole::Editor))
            .expect("share");
        let mut config = Config::default();
        config.sync.poll_interval = HumanDuration(Duration::from_millis(100));
        let team = open_with(
            fake.tree(&share.volume, share.root.clone()),
            dir.path(),
            config,
        )
        .await;
        // No pacing of background turns: the polls come at the interval.
        team.gate.set_interval(Duration::ZERO);
        let quiet = Duration::from_millis(600);
        let cadence = Arc::new(Cadence::with_quiet_period(quiet));
        let poller = Poller::new(&share.volume, false, Arc::clone(&cadence));
        poller.add(Arc::clone(&team));
        let task = tokio::spawn(Arc::clone(&poller).run());
        let in_use = VolumeUse::new(Arc::clone(&cadence), &share.volume);

        // The pass at the start, then the long interval.
        let started = wait_for_polls(&team, 1).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(polls(&team), started, "not in use: the long interval");
        // A touch polls at once, then every 100 ms while touched.
        in_use.touch();
        let forward = wait_for_polls(&team, started + 1).await;
        for _ in 0..12 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            in_use.touch();
        }
        assert!(
            polls(&team) >= forward + 4,
            "in use: the short interval ({} polls)",
            polls(&team) - forward
        );
        // Quiet: once a pass in flight is done, no more polls.
        tokio::time::sleep(quiet + Duration::from_millis(300)).await;
        assert_eq!(cadence.foreground(), None);
        let quiet_count = polls(&team);
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(polls(&team), quiet_count, "back to the long interval");
        // A touch brings it forward again, polled at once.
        in_use.touch();
        wait_for_polls(&team, quiet_count + 1).await;
        task.abort();
    }

    /// Waits (at most 5 s, far below the long interval) until `engine` has
    /// polled `count` times; the count then.
    async fn wait_for_polls(engine: &Engine, count: u64) -> u64 {
        for _ in 0..500 {
            let now = polls(engine);
            if now >= count {
                return now;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("{count} polls expected, {} made", polls(engine));
    }

    fn meta() -> FileMeta {
        FileMeta {
            mtime: UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            mode: Some(0o600),
            media_type: "application/octet-stream".into(),
            sha1: None,
        }
    }

    async fn open(backend: FakeBackend, dir: &std::path::Path) -> Arc<Engine> {
        open_with(backend, dir, Config::default()).await
    }

    async fn open_with(backend: FakeBackend, dir: &std::path::Path, config: Config) -> Arc<Engine> {
        let options = EngineOptions::in_dir(
            dir,
            config,
            DeviceIdentity {
                id: Uuid::from_u128(7),
                name: "poller-test".into(),
            },
            system_clock(),
        );
        let (engine, _invalidations) = Engine::open(Arc::new(backend), options)
            .await
            .expect("engine");
        engine
    }

    fn polls(engine: &Engine) -> u64 {
        engine
            .metrics()
            .snapshot_latency()
            .into_iter()
            .find(|(op, _)| *op == Op::BackendPollEvents.as_str())
            .map_or(0, |(_, summary)| summary.count)
    }

    /// The names in a tree's root.
    fn names(engine: &Arc<Engine>) -> Vec<String> {
        let (handle, _) = engine.opendir(crate::vfs::ROOT_INO).expect("opendir");
        let entries = engine.dir_snapshot(handle).expect("entries");
        engine.releasedir(handle);
        entries
            .iter()
            .map(|entry| entry.name.clone())
            .filter(|name| name != "." && name != "..")
            .collect()
    }

    async fn upload(backend: &FakeBackend, parent: &LinkId, name: &str) {
        backend
            .upload_new(
                parent,
                name,
                UploadSource::Bytes(Bytes::from_static(b"data")),
                meta(),
            )
            .await
            .expect("upload");
    }

    /// Two trees on one volume (My Files and a device's root) get every
    /// page of one poll; a tree on another volume has its own stream.
    #[tokio::test(flavor = "multi_thread")]
    async fn one_poll_per_volume_reaches_every_tree_on_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fake = FakeBackend::new();
        let device = fake.add_root("Laptop", None, None).expect("device");
        let share = fake
            .add_root("Team", Some("owner@example.test"), Some(FakeRole::Editor))
            .expect("share");
        let my_files = open(fake.clone(), &dir.path().join("a")).await;
        let laptop = open(
            fake.tree(&device.volume, device.root.clone()),
            &dir.path().join("b"),
        )
        .await;
        let team = open(
            fake.tree(&share.volume, share.root.clone()),
            &dir.path().join("c"),
        )
        .await;
        for engine in [&my_files, &laptop, &team] {
            engine
                .ensure_listed(crate::vfs::ROOT_INO)
                .await
                .expect("list");
        }
        let main_root = fake.root().await.expect("root").link_id;
        upload(&fake, &main_root, "in-my-files").await;
        upload(&fake, &device.root, "on-the-laptop").await;
        upload(&fake, &share.root, "shared").await;

        let before = polls(&my_files) + polls(&laptop);
        sync_trees("volume", &[Arc::clone(&my_files), Arc::clone(&laptop)])
            .await
            .expect("sync");
        // One poll for the volume, made by the first tree.
        assert_eq!(polls(&my_files) + polls(&laptop) - before, 1);
        assert_eq!(polls(&laptop), 0);
        assert_eq!(names(&my_files), ["in-my-files"]);
        assert_eq!(names(&laptop), ["on-the-laptop"]);
        // The share's volume has not been polled yet.
        assert!(names(&team).is_empty());
        sync_trees("team", std::slice::from_ref(&team))
            .await
            .expect("sync");
        assert_eq!(names(&team), ["shared"]);
        // Both trees of the volume keep the same cursor in their stores.
        let cursor =
            |engine: &Arc<Engine>| crate::events::load_cursor(&engine.store).expect("cursor");
        assert_eq!(cursor(&my_files), cursor(&laptop));
    }

    /// A retired tree (a gone share whose removal waits for the pass, D114)
    /// gets nothing from a pass that took it before its removal: its cursor
    /// and listing stay, the other trees go on. The removal's wait for the
    /// pass ends once a pass holding the tree has.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_pass_leaves_a_retired_tree_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fake = FakeBackend::new();
        let device = fake.add_root("Laptop", None, None).expect("device");
        let my_files = open(fake.clone(), &dir.path().join("a")).await;
        let laptop = open(
            fake.tree(&device.volume, device.root.clone()),
            &dir.path().join("b"),
        )
        .await;
        for engine in [&my_files, &laptop] {
            engine
                .ensure_listed(crate::vfs::ROOT_INO)
                .await
                .expect("list");
        }
        upload(&fake, &device.root, "after-the-removal").await;
        let main_root = fake.root().await.expect("root").link_id;
        upload(&fake, &main_root, "in-my-files").await;
        let before = laptop.event_cursor.lock().await.clone();
        // A pass that holds the laptop's cursor: the removal waits for it.
        let held = laptop.event_cursor.lock().await;
        laptop.retire();
        let waited = tokio::spawn({
            let laptop = Arc::clone(&laptop);
            async move { laptop.wait_for_event_pass().await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!waited.is_finished(), "a pass still holds the tree");
        drop(held);
        waited.await.expect("the wait ends with the pass");

        sync_trees("volume", &[Arc::clone(&my_files), Arc::clone(&laptop)])
            .await
            .expect("sync");
        assert_eq!(names(&my_files), ["in-my-files"]);
        assert!(names(&laptop).is_empty(), "the retired tree changed");
        assert_eq!(*laptop.event_cursor.lock().await, before);
        assert!(!laptop.events_refreshing());
        // A volume with only retired trees is not polled.
        let polled = polls(&laptop);
        sync_trees("volume", std::slice::from_ref(&laptop))
            .await
            .expect("sync");
        assert_eq!(polls(&laptop), polled);
    }

    /// A tree whose cursor differs from its volume's (added later, or a
    /// crash between two stores) refreshes from the common one.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_tree_with_another_cursor_lists_again() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fake = FakeBackend::new();
        let device = fake.add_root("Laptop", None, None).expect("device");
        let my_files = open(fake.clone(), &dir.path().join("a")).await;
        let main_root = fake.root().await.expect("root").link_id;
        upload(&fake, &main_root, "first").await;
        // Opened after that upload: its cursor is a newer one.
        let laptop = open(
            fake.tree(&device.volume, device.root.clone()),
            &dir.path().join("b"),
        )
        .await;
        upload(&fake, &main_root, "second").await;
        let differ = *my_files.event_cursor.lock().await != *laptop.event_cursor.lock().await;
        assert!(differ, "the cursors differ");
        sync_trees("volume", &[Arc::clone(&my_files), Arc::clone(&laptop)])
            .await
            .expect("sync");
        assert_eq!(
            *my_files.event_cursor.lock().await,
            *laptop.event_cursor.lock().await
        );
        // The refresh of the device's tree runs in the background.
        for _ in 0..200 {
            if !laptop.events_refreshing() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(!laptop.events_refreshing());
        assert!(!my_files.events_refreshing());
        my_files
            .ensure_listed(crate::vfs::ROOT_INO)
            .await
            .expect("list");
        assert_eq!(names(&my_files), ["first", "second"]);
    }

    #[test]
    fn only_permanent_backend_errors_count_against_a_page() {
        for passing in [
            BackendError::Transient("HTTP 502".into()),
            BackendError::RateLimited { retry_after: None },
            BackendError::Offline("connect".into()),
            BackendError::Unauthorized,
        ] {
            assert!(
                !is_page_error(&CoreError::Backend(passing.clone())),
                "{passing}"
            );
        }
        for page in [
            BackendError::Other("a node does not decrypt".into()),
            BackendError::NotFound,
            BackendError::Integrity("signature".into()),
            BackendError::PermissionDenied,
        ] {
            assert!(is_page_error(&CoreError::Backend(page.clone())), "{page}");
        }
        assert!(!is_page_error(&CoreError::Store("busy".into())));
        assert!(!is_page_error(&CoreError::Timeout));
    }

    /// A tree with a file uploaded behind its cursor, in its listed root.
    async fn tree_with_a_page(
        dir: &std::path::Path,
    ) -> (FakeBackend, Arc<Engine>, Option<EventId>) {
        let fake = FakeBackend::new();
        let engine = open(fake.clone(), dir).await;
        engine
            .ensure_listed(crate::vfs::ROOT_INO)
            .await
            .expect("list");
        let root = fake.root().await.expect("root").link_id;
        upload(&fake, &root, "behind-the-page").await;
        let before = engine.event_cursor.lock().await.clone();
        assert!(before.is_some());
        (fake, engine, before)
    }

    /// Fails the next sync of `engine` with `error` from `get_nodes`, and
    /// checks that the page stays where it was.
    async fn fails_at(
        fake: &FakeBackend,
        engine: &Arc<Engine>,
        before: &Option<EventId>,
        error: BackendError,
    ) {
        fake.inject_failure(FakeOp::GetNodes, error.clone());
        let failed = sync_trees("volume", std::slice::from_ref(engine))
            .await
            .expect_err("the page fails");
        assert_eq!(failed.to_string(), CoreError::Backend(error).to_string());
        assert_eq!(*engine.event_cursor.lock().await, *before);
        assert!(!engine.events_refreshing());
    }

    async fn wait_for_refresh(engine: &Arc<Engine>) {
        for _ in 0..500 {
            if !engine.events_refreshing() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the refresh did not finish");
    }

    /// M6.0.3 V-3: a page that fails for every tree with an error that is
    /// not transient (a node that cannot be decrypted, say) was replayed at
    /// every poll for ever. The third failure gives it up: the trees list
    /// again from a fresh cursor, which is stored once that is done.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_page_that_keeps_failing_is_given_up_for_a_refresh() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (fake, engine, before) = tree_with_a_page(dir.path()).await;
        let broken = || BackendError::Other("a node does not decrypt".into());
        for _ in 1..STUCK_PAGE_LIMIT {
            fails_at(&fake, &engine, &before, broken()).await;
        }
        assert!(names(&engine).is_empty());

        fake.inject_failure(FakeOp::GetNodes, broken());
        sync_trees("volume", std::slice::from_ref(&engine))
            .await
            .expect("the page is given up");
        let after = engine.event_cursor.lock().await.clone();
        assert_ne!(after, before, "the cursor moves on");
        wait_for_refresh(&engine).await;
        assert_eq!(names(&engine), ["behind-the-page"]);
        engine.store.flush().await.expect("flush");
        assert_eq!(
            crate::events::load_cursor(&engine.store).expect("cursor"),
            after
        );
        assert!(engine.stuck_page.lock().is_none());
    }

    /// D132 (fork P26): the refresh after a page is given up lists the
    /// trees again; a child that does not decrypt comes back degraded in
    /// that listing instead of failing it, so the refresh finishes, and a
    /// known child that became degraded stays.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_refresh_with_a_degraded_child_finishes_and_keeps_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fake = FakeBackend::new();
        let root = fake.root().await.expect("root").link_id;
        upload(&fake, &root, "known").await;
        let engine = open(fake.clone(), dir.path()).await;
        engine
            .ensure_listed(crate::vfs::ROOT_INO)
            .await
            .expect("list");
        upload(&fake, &root, "behind-the-page").await;
        let before = engine.event_cursor.lock().await.clone();
        let known = fake
            .list_children(&root)
            .await
            .expect("list")
            .into_iter()
            .find(|node| node.name == "known")
            .expect("known")
            .link_id;
        fake.set_degraded(&known, true);

        let broken = || BackendError::Other("a node does not decrypt".into());
        for _ in 1..STUCK_PAGE_LIMIT {
            fails_at(&fake, &engine, &before, broken()).await;
        }
        fake.inject_failure(FakeOp::GetNodes, broken());
        sync_trees("volume", std::slice::from_ref(&engine))
            .await
            .expect("the page is given up");
        wait_for_refresh(&engine).await;
        assert_eq!(names(&engine), ["behind-the-page", "known"]);
        assert_ne!(*engine.event_cursor.lock().await, before);
    }

    /// Transient failures (transport, 429, 5xx) never give a page up, and
    /// neither count nor reset the failures of a page that is broken.
    #[tokio::test(flavor = "multi_thread")]
    async fn transient_failures_do_not_give_a_page_up() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (fake, engine, before) = tree_with_a_page(dir.path()).await;
        let transient = [
            BackendError::Transient("HTTP 502".into()),
            BackendError::RateLimited { retry_after: None },
            BackendError::Transient("connection reset".into()),
            BackendError::RateLimited { retry_after: None },
        ];
        for error in transient.clone() {
            fails_at(&fake, &engine, &before, error).await;
        }
        assert!(engine.stuck_page.lock().is_none());

        let broken = || BackendError::Other("a node does not decrypt".into());
        fails_at(&fake, &engine, &before, broken()).await;
        for error in transient {
            fails_at(&fake, &engine, &before, error).await;
        }
        fails_at(&fake, &engine, &before, broken()).await;
        // The third page error, after any number of transient ones.
        fake.inject_failure(FakeOp::GetNodes, broken());
        sync_trees("volume", std::slice::from_ref(&engine))
            .await
            .expect("the page is given up");
        assert_ne!(*engine.event_cursor.lock().await, before);
        wait_for_refresh(&engine).await;
        assert_eq!(names(&engine), ["behind-the-page"]);
    }

    /// Without injected failures the same page applies at once, through
    /// the events: the tests above fail because of the injections only.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_page_applies_without_failures() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (_fake, engine, before) = tree_with_a_page(dir.path()).await;
        sync_trees("volume", std::slice::from_ref(&engine))
            .await
            .expect("sync");
        assert_ne!(*engine.event_cursor.lock().await, before);
        assert!(!engine.events_refreshing());
        assert_eq!(names(&engine), ["behind-the-page"]);
    }
}
