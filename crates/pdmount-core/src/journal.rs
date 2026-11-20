//! Journal executor: desired-state reconciliation (DESIGN.md §8.2, §7.3–§7.8).
//!
//! The journal is a set of inodes that may need remote work, one entry per
//! inode, not an operation log. For each ready entry the executor derives
//! the next step by comparing the node's local state with its last known
//! remote state: create the folder, upload the file (new or as a revision),
//! move or rename it, or trash it. Sequences coalesce naturally: a file
//! created, written, renamed and deleted before its first upload produces
//! nothing remotely.
//!
//! - **Dependencies** are by inode: a create or move waits for its parent's
//!   link; a node waits for the remote occupant of its target name to move
//!   away or be trashed first. Two nodes swapping names break the cycle
//!   through a temporary `.pdmount-swap-<ino>` name. A folder's trash
//!   waits for the entries of children deleted or moved out (D116).
//! - **Uploads** wait until the file has no writer and has been quiet for
//!   `upload.debounce`, or has been dirty for `upload.max_dirty_age`. They run
//!   concurrently (`upload.concurrency`); namespace steps run one at a time.
//! - **Errors:** retryable ones back off exponentially (1 s to 5 min, with
//!   jitter); permanent ones park the entry with a reason. Quota and size
//!   errors are also deferred to the next `flush`/`fsync` of the inode
//!   (§7.8).
//! - **Crash recovery:** an upload that reached the server before the daemon
//!   died is recognized by its SHA-1 and adopted instead of uploaded again.
//!
//! Parked state lives in RAM; parked entries are retried at the next mount.
//! The time an entry was queued is the exception: it is kept in the store
//! (`queued`, D105) so that `status` counts an entry's age across restarts.
//! A local change retries an automatic park but not one made with
//! `queue park` (D061).

use crate::cache::Priority;
use crate::config::{MetaRevisionPolicy, UploadConfig};
use crate::conflicts::{ConflictKind, ConflictNote};
use crate::engine::Engine;
use crate::errors::{CoreError, Result};
use crate::lock_order::{self, Rank};
use crate::metrics::{Metrics, check_not_hot};
use crate::staging::Base;
use crate::store::Store;
use crate::vfs::{
    Changes, Content, DEFAULT_FILE_MODE, Ino, Invalidation, ROOT_INO, RemoteState, Tree,
};
use pdmount_backend::{
    BackendError, FileMeta, LinkId, NameHash, NodeKind, RemoteNode, RevisionId, UploadSource,
};
use rusqlite::params;
use sha1::Digest;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

/// Coalescing window of metadata-only revisions (§8.5).
const META_DELAY: Duration = Duration::from_secs(30);
/// Fallback re-check of an entry blocked on another one.
const BLOCKED_RECHECK: Duration = Duration::from_secs(5);
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(300);
/// Attempts after which a non-retryable error parks an entry.
const PARK_AFTER: u32 = 5;
/// Steps in a row that end in `Replan` before the entry waits a second
/// between plans: the tree and the drive disagree in a way no step settles.
const REPLAN_BURST: u32 = 16;
const TRASH_BATCH: usize = 50;
/// The longest age taken from a stored queue time: a clock that jumped must
/// not give an absurd age (D105).
const MAX_CARRIED_AGE: Duration = Duration::from_secs(365 * 24 * 3600);
/// Base blocks materialized per round before a snapshot (256 MiB).
const MATERIALIZE_BATCH: usize = 64;
/// Largest file whose remote content is read to compare it with a local one
/// when the remote revision claims no SHA-1 (D040).
const IDENTICAL_PROBE_MAX: u64 = 16 << 20;
const MAX_WAKE: Duration = Duration::from_secs(1);
/// How often a `sync` of a path looks at its entries.
const SYNC_POLL: Duration = Duration::from_millis(50);
const USER_PARK_REASON: &str = "parked by user";
/// How far a `sync` follows what an entry waits for, and the planner a
/// cycle of nodes waiting for each other's places.
const MAX_DEPENDENCY_CHAIN: usize = 4096;
/// The start of the temporary name of a node that frees its remote place
/// for a node waiting for it (D117).
pub(crate) const SWAP_PREFIX: &str = ".pdmount-swap-";

#[derive(Debug, Clone)]
struct Entry {
    not_before: Instant,
    attempts: u32,
    parked: Option<String>,
    /// Parked with `queue park`: a local change does not resume it (D061).
    user_parked: bool,
    /// `queue drop` is taking the entry apart: no step may start.
    held: bool,
    in_flight: bool,
    /// Last local change, for the debounce.
    changed_at: Instant,
    /// When the inode became dirty in this process, for `max_dirty_age`
    /// (monotonic; the age shown adds [`Self::carried`]).
    dirty_since: Instant,
    /// How long the entry was queued before this process started: from the
    /// stored queue time, clamped (D105). Display only.
    carried: Duration,
    /// Upload as soon as possible (`sync`, unmount, `fsync` in remote mode).
    forced: bool,
    /// End of the back-off after the last failure; a shutdown flush does not
    /// start the entry before it (D075). Cleared by a change or a success.
    retry_at: Option<Instant>,
    blocked_on: Option<Ino>,
    /// The last failure, for `status` and diagnostics.
    last_error: Option<String>,
    /// Steps in a row that ended in `Replan`.
    replans: u32,
    /// Waits for another entry in a row ([`Journal::wait_on`]), for their
    /// back-off; not failed attempts (D121).
    waits: u32,
    /// Bumped by every change and force: a plan made from an older
    /// generation must not defer, block or remove the entry.
    generation: u64,
    /// [`Self::generation`] when the last step started: a wait found by
    /// that step keeps a change, retry or force made while it ran.
    step_generation: u64,
    /// Made only because another entry waits for it, and neither changed
    /// nor stepped since: when it leaves, nothing moved that the waiting
    /// entry could see, so it is not woken (D121).
    waited_into: bool,
    /// The tree's write access is withdrawn (D114): no step starts, and an
    /// entry that would take one is parked with this reason.
    hold: Option<String>,
}

impl Entry {
    fn new(now: Instant) -> Self {
        Self {
            not_before: now,
            attempts: 0,
            parked: None,
            user_parked: false,
            held: false,
            in_flight: false,
            changed_at: now,
            dirty_since: now,
            carried: Duration::ZERO,
            forced: false,
            retry_at: None,
            blocked_on: None,
            last_error: None,
            replans: 0,
            waits: 0,
            generation: 0,
            step_generation: 0,
            waited_into: false,
            hold: None,
        }
    }

    /// How long the entry has been queued, across restarts.
    fn age(&self, now: Instant) -> Duration {
        self.carried + now.saturating_duration_since(self.dirty_since)
    }

    /// A local change retries an automatic park, not a hold (D114).
    fn changed(&mut self) {
        if !self.user_parked {
            self.parked = self.hold.clone();
        }
        self.waits = 0;
        self.waited_into = false;
    }

    /// The executor parks the entry; the user's reason stays, and under a
    /// hold the hold's, so that lifting it retries the entry.
    fn park(&mut self, reason: String) {
        if !self.user_parked {
            self.parked = Some(self.hold.clone().unwrap_or(reason));
        }
    }

    /// Marks a step as running, unless the entry is held or parked. Under a
    /// hold (D114) it is parked with the hold's reason instead.
    fn start_step(&mut self) -> bool {
        if self.held || self.parked.is_some() {
            return false;
        }
        if self.hold.is_some() {
            self.parked = self.hold.clone();
            return false;
        }
        self.in_flight = true;
        self.blocked_on = None;
        self.step_generation = self.generation;
        self.waited_into = false;
        true
    }
}

/// An entry as `queue list` shows it.
#[derive(Debug, Clone)]
pub(crate) struct EntryView {
    pub ino: Ino,
    pub attempts: u32,
    pub parked: Option<String>,
    pub in_flight: bool,
    pub last_error: Option<String>,
    pub blocked_on: Option<Ino>,
    /// How long the inode has been dirty.
    pub age: Duration,
    /// Time left of the back-off after a failure.
    pub backoff: Option<Duration>,
}

/// One look at the entries a `sync` waits for.
#[derive(Debug, Default)]
pub(crate) struct SyncRound {
    /// Entries not parked.
    pub pending: usize,
    /// Of those, the entries that are not held up by a parked one.
    pub moving: usize,
    /// The parked entries among them and those holding them up.
    pub parked: Vec<Ino>,
}

/// The entries a shutdown flush waits for ([`Engine::flush_barrier`]).
#[derive(Debug, Clone, Default)]
pub struct FlushBarrier {
    inos: Vec<Ino>,
}

impl FlushBarrier {
    pub fn len(&self) -> usize {
        self.inos.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inos.is_empty()
    }
}

/// Why a shutdown flush stopped waiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlushEnd {
    /// None of the barrier's entries can finish any more: all uploaded, or
    /// the rest parked or held up by a parked entry.
    Reached,
    /// The grace ran out.
    Deadline,
    /// Offline or `needs_login`: no wait (D075).
    NotOnline(crate::health::Connectivity),
}

/// What a shutdown flush did ([`Engine::flush_to`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlushOutcome {
    /// Entries in the barrier.
    pub barrier: usize,
    /// Of those, the entries still queued (parked ones included).
    pub left: usize,
    pub elapsed: Duration,
    pub ended: FlushEnd,
}

impl FlushOutcome {
    /// Entries of the barrier that were uploaded.
    pub fn flushed(&self) -> usize {
        self.barrier - self.left
    }
}

/// Queue counts for `status`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QueueCounts {
    /// Entries not parked (including those in flight).
    pub pending: usize,
    pub parked: usize,
    pub in_flight: usize,
}

#[derive(Default)]
struct State {
    entries: BTreeMap<Ino, Entry>,
    /// Every entry is forced (draining before an unmount).
    draining: bool,
    /// Shutting down: no step starts any more (D062).
    stopping: bool,
    /// Tests only: no step starts until resumed, so that changes queue up.
    paused: bool,
    /// Ages of the entries of the last session that have none yet (D105);
    /// an entry made for one of these inodes takes it.
    restored: BTreeMap<Ino, Duration>,
    /// The reason of [`Journal::hold_all`] while it lasts: entries made
    /// meanwhile are held too.
    hold: Option<String>,
}

/// Directories to list again: those the executor has not taken yet, and
/// how many requests for each are not answered by a listing yet. The store
/// keeps the latter until they are, so a stop does not forget them (D069).
#[derive(Default)]
struct Listings {
    queued: Vec<Ino>,
    open: BTreeMap<Ino, u32>,
}

/// A running upload's place: one of its tree's and one of the session's.
pub(crate) struct UploadPermit {
    _tree: tokio::sync::OwnedSemaphorePermit,
    _session: tokio::sync::OwnedSemaphorePermit,
}

pub(crate) struct Journal {
    state: lock_order::Mutex<State>,
    wake: tokio::sync::Notify,
    /// Directories to list again (see `Node::clashed`).
    listings: parking_lot::Mutex<Listings>,
    /// Signalled whenever an entry finishes, fails or leaves.
    progress: tokio::sync::Notify,
    /// One permit per running upload of this tree (`upload.concurrency`),
    /// which tells when it is idle.
    uploads: Arc<tokio::sync::Semaphore>,
    upload_slots: usize,
    /// The session's: `upload.concurrency` over every tree (D058, D110).
    session_uploads: Arc<tokio::sync::Semaphore>,
    debounce: Duration,
    max_dirty_age: Duration,
    meta_revision: MetaRevisionPolicy,
    /// Where the queue times go (`queued`, D105); none in unit tests.
    store: Option<Arc<Store>>,
}

/// Unix milliseconds of `time` (0 before the epoch).
pub(crate) fn unix_ms(time: SystemTime) -> i64 {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_millis()).unwrap_or(i64::MAX)
        })
}

/// The age of an entry queued `elapsed_ms` before now: never negative, at
/// most [`MAX_CARRIED_AGE`].
fn carried_age(elapsed_ms: i64) -> Duration {
    Duration::from_millis(elapsed_ms.max(0).unsigned_abs()).min(MAX_CARRIED_AGE)
}

#[derive(Debug, Clone, Copy)]
struct Timing {
    changed_at: Instant,
    dirty_since: Instant,
    forced: bool,
    generation: u64,
}

impl Journal {
    /// A journal with limits of its own (tests).
    #[cfg(test)]
    pub(crate) fn new(config: &UploadConfig) -> Self {
        let slots = config.concurrency.max(1) as usize;
        Self::sharing(config, Arc::new(tokio::sync::Semaphore::new(slots)))
    }

    /// A journal whose uploads also take one of the session's
    /// `session_uploads` permits.
    fn sharing(config: &UploadConfig, session_uploads: Arc<tokio::sync::Semaphore>) -> Self {
        Self {
            state: lock_order::Mutex::new(Rank::Leaf, State::default()),
            wake: tokio::sync::Notify::new(),
            listings: parking_lot::Mutex::default(),
            progress: tokio::sync::Notify::new(),
            uploads: Arc::new(tokio::sync::Semaphore::new(
                config.concurrency.max(1) as usize
            )),
            upload_slots: config.concurrency.max(1) as usize,
            session_uploads,
            debounce: config.debounce.0,
            max_dirty_age: config.max_dirty_age.0,
            meta_revision: config.meta_revision,
            store: None,
        }
    }

    /// A journal that keeps the time each entry was queued in `store`, its
    /// uploads limited with the session's other trees by `session_uploads`.
    pub(crate) fn with_store(
        config: &UploadConfig,
        store: Arc<Store>,
        session_uploads: Arc<tokio::sync::Semaphore>,
    ) -> Self {
        Self {
            store: Some(store),
            ..Self::sharing(config, session_uploads)
        }
    }

    /// Takes the queue times stored by the last session: `rows` are
    /// `(inode, unix milliseconds)`, `now_ms` is now. Call before any entry
    /// exists; entries made afterwards for these inodes start with their age
    /// (D105). The age is never negative (a clock that went back) nor more
    /// than [`MAX_CARRIED_AGE`].
    pub(crate) fn restore(&self, rows: &[(Ino, i64)], now_ms: i64) {
        let mut state = self.state.lock();
        for &(ino, queued_ms) in rows {
            state
                .restored
                .insert(ino, carried_age(now_ms.saturating_sub(queued_ms)));
        }
    }

    /// Ends [`Self::restore`]: the stored times of inodes that got no entry
    /// (the queue was left empty, or the inode is gone) are deleted.
    pub(crate) fn finish_restore(&self) {
        let left: Vec<Ino> = {
            let mut state = self.state.lock();
            std::mem::take(&mut state.restored).into_keys().collect()
        };
        if left.is_empty() {
            return;
        }
        if let Some(store) = &self.store {
            store.write(move |conn| {
                let mut delete = conn.prepare_cached("DELETE FROM queued WHERE ino = ?1")?;
                for ino in left {
                    delete.execute(params![ino as i64])?;
                }
                Ok(())
            });
        }
    }

    fn entry<'a>(&self, state: &'a mut State, ino: Ino, now: Instant) -> &'a mut Entry {
        let draining = state.draining;
        let entry = self.entry_or_new(state, ino, now);
        entry.forced |= draining;
        entry
    }

    /// The entry of `ino`; a new one takes its age from the last session or
    /// records its queue time, and the tree's hold if there is one.
    fn entry_or_new<'a>(&self, state: &'a mut State, ino: Ino, now: Instant) -> &'a mut Entry {
        if !state.entries.contains_key(&ino) {
            let mut entry = Entry::new(now);
            entry.hold.clone_from(&state.hold);
            match state.restored.remove(&ino) {
                Some(age) => entry.carried = age,
                None => self.record_queued(ino),
            }
            state.entries.insert(ino, entry);
        }
        state.entries.get_mut(&ino).expect("just inserted")
    }

    /// Stores the time a new entry was queued. Called under the state lock,
    /// so the stored rows follow the entries' order.
    fn record_queued(&self, ino: Ino) {
        if let Some(store) = &self.store {
            let ms = unix_ms(SystemTime::now());
            store.write(move |conn| {
                conn.execute(
                    "INSERT OR REPLACE INTO queued(ino, queued_at_ms) VALUES (?1, ?2)",
                    params![ino as i64, ms],
                )
                .map(drop)
            });
        }
    }

    /// Deletes the stored queue time of an entry that left.
    fn record_left(&self, ino: Ino) {
        if let Some(store) = &self.store {
            store.write(move |conn| {
                conn.execute("DELETE FROM queued WHERE ino = ?1", params![ino as i64])
                    .map(drop)
            });
        }
    }

    /// Asks for `dirs` to be listed again; `run_journal` does it. `save`
    /// gets the directories still to list when they change, under the lock,
    /// so the saves keep their order.
    pub(crate) fn list_later(&self, dirs: Vec<Ino>, save: impl FnOnce(Vec<Ino>)) {
        if dirs.is_empty() {
            return;
        }
        let mut listings = self.listings.lock();
        let before = listings.open.len();
        for &dir in &dirs {
            *listings.open.entry(dir).or_default() += 1;
        }
        listings.queued.extend(dirs);
        if listings.open.len() != before {
            save(listings.open.keys().copied().collect());
        }
        drop(listings);
        self.wake.notify_one();
    }

    /// The listings asked for since the last call.
    fn take_listings(&self) -> Vec<Ino> {
        std::mem::take(&mut self.listings.lock().queued)
    }

    /// A listing taken by [`Self::take_listings`] succeeded.
    fn listed(&self, dir: Ino, save: impl FnOnce(Vec<Ino>)) {
        let mut listings = self.listings.lock();
        let std::collections::btree_map::Entry::Occupied(mut open) = listings.open.entry(dir)
        else {
            return;
        };
        *open.get_mut() -= 1;
        if *open.get() == 0 {
            open.remove();
            save(listings.open.keys().copied().collect());
        }
    }

    /// A namespace change: re-plan at once.
    pub(crate) fn touch(&self, ino: Ino) {
        let now = Instant::now();
        {
            let mut state = self.state.lock();
            let entry = self.entry(&mut state, ino, now);
            entry.changed();
            entry.attempts = 0;
            entry.retry_at = None;
            entry.not_before = now;
            entry.generation += 1;
        }
        self.wake.notify_one();
    }

    /// A content change: restarts the debounce.
    pub(crate) fn touch_content(&self, ino: Ino) {
        let now = Instant::now();
        {
            let mut state = self.state.lock();
            let entry = self.entry(&mut state, ino, now);
            entry.changed();
            entry.attempts = 0;
            entry.retry_at = None;
            entry.changed_at = now;
            entry.not_before = now;
            entry.generation += 1;
        }
        self.wake.notify_one();
    }

    /// A metadata change; same timing as content, coalesced by the planner.
    pub(crate) fn touch_meta(&self, ino: Ino) {
        self.touch_content(ino);
    }

    /// Inodes with an entry.
    pub(crate) fn inos(&self) -> Vec<Ino> {
        self.state.lock().entries.keys().copied().collect()
    }

    pub(crate) fn in_flight(&self, ino: Ino) -> bool {
        self.state
            .lock()
            .entries
            .get(&ino)
            .is_some_and(|e| e.in_flight)
    }

    /// `(parked, in flight)` of a queued inode; `None` when it is not queued.
    pub(crate) fn entry_state(&self, ino: Ino) -> Option<(bool, bool)> {
        let state = self.state.lock();
        let entry = state.entries.get(&ino)?;
        Some((entry.parked.is_some(), entry.in_flight))
    }

    pub(crate) fn counts(&self) -> QueueCounts {
        let state = self.state.lock();
        let mut counts = QueueCounts::default();
        for entry in state.entries.values() {
            if entry.parked.is_some() {
                counts.parked += 1;
            } else {
                counts.pending += 1;
            }
            if entry.in_flight {
                counts.in_flight += 1;
            }
        }
        counts
    }

    /// How long the oldest entry that is not parked has been dirty.
    pub(crate) fn oldest_age(&self) -> Option<Duration> {
        let now = Instant::now();
        let state = self.state.lock();
        state
            .entries
            .values()
            .filter(|entry| entry.parked.is_none())
            .map(|entry| entry.age(now))
            .max()
    }

    pub(crate) fn parked(&self) -> Vec<(Ino, String)> {
        let state = self.state.lock();
        state
            .entries
            .iter()
            .filter_map(|(ino, e)| e.parked.clone().map(|reason| (*ino, reason)))
            .collect()
    }

    /// Makes every entry eligible now and keeps doing so for new ones.
    pub(crate) fn force_all(&self, draining: bool) {
        let now = Instant::now();
        {
            let mut state = self.state.lock();
            state.draining = draining;
            for entry in state.entries.values_mut() {
                entry.forced = true;
                entry.not_before = entry.not_before.min(now);
                entry.generation += 1;
            }
        }
        self.wake.notify_one();
    }

    /// Forces one entry (`fsync` in remote mode).
    pub(crate) fn force(&self, ino: Ino) {
        let now = Instant::now();
        {
            let mut state = self.state.lock();
            let entry = self.entry(&mut state, ino, now);
            entry.forced = true;
            entry.not_before = now;
            entry.generation += 1;
        }
        self.wake.notify_one();
    }

    /// No step starts from now on; what is pending resumes at the next
    /// mount (D062).
    pub(crate) fn stop_starting(&self) {
        self.state.lock().stopping = true;
    }

    /// Tests: holds back every step (`true`) or lets them go on.
    #[cfg(test)]
    pub(crate) fn pause(&self, paused: bool) {
        self.state.lock().paused = paused;
        self.wake.notify_one();
    }

    /// Whether no upload and no inline step runs.
    pub(crate) fn idle(&self) -> bool {
        let state = self.state.lock();
        self.uploads.available_permits() == self.upload_slots
            && !state.entries.values().any(|entry| entry.in_flight)
    }

    fn ready(&self, now: Instant) -> (Vec<(Ino, Timing)>, Option<Instant>) {
        let state = self.state.lock();
        let mut ready = Vec::new();
        let mut next: Option<Instant> = None;
        if state.stopping || state.paused {
            return (ready, next);
        }
        for (&ino, entry) in &state.entries {
            if entry.in_flight || entry.held || entry.parked.is_some() {
                continue;
            }
            if entry.not_before <= now {
                ready.push((
                    ino,
                    Timing {
                        changed_at: entry.changed_at,
                        dirty_since: entry.dirty_since,
                        forced: entry.forced,
                        generation: entry.generation,
                    },
                ));
            } else {
                next = Some(next.map_or(entry.not_before, |n| n.min(entry.not_before)));
            }
        }
        (ready, next)
    }

    /// Nothing left to do as of `generation`; a later change keeps it.
    /// An entry made only by a wait wakes nothing when it leaves so: its
    /// node is as the waiting entry saw it, and a node with nothing to do
    /// now may have nothing to do ever (one in a local-only folder), so the
    /// wake would start a loop of waits (D121).
    fn done(&self, ino: Ino, generation: u64) {
        {
            let mut state = self.state.lock();
            let quiet = state.entries.get(&ino).is_some_and(|e| e.waited_into);
            if state
                .entries
                .get(&ino)
                .is_some_and(|e| !e.in_flight && e.generation == generation)
            {
                state.entries.remove(&ino);
                self.record_left(ino);
            }
            if !quiet {
                Self::unblock(&mut state, ino);
            }
        }
        self.progress.notify_waiters();
    }

    fn defer(&self, ino: Ino, until: Instant, generation: u64) {
        if let Some(entry) = self.state.lock().entries.get_mut(&ino)
            && entry.generation == generation
        {
            entry.blocked_on = None;
            entry.not_before = until;
        }
    }

    fn block(&self, ino: Ino, on: Ino, generation: u64) {
        let now = Instant::now();
        let mut state = self.state.lock();
        let mut new_dependency = false;
        if let Some(entry) = state.entries.get_mut(&ino)
            && entry.generation == generation
        {
            new_dependency = entry.blocked_on != Some(on);
            entry.blocked_on = Some(on);
            entry.not_before = now + BLOCKED_RECHECK;
        }
        // The dependency must be scheduled itself.
        let draining = state.draining;
        let new = !state.entries.contains_key(&on);
        let dep = self.entry_or_new(&mut state, on, now);
        dep.forced |= draining;
        dep.waited_into |= new;
        // A dependency that waits itself (a folder's trash, D116) looks
        // again at once: it may be waiting for this entry (D117).
        if new_dependency && dep.blocked_on.is_some() && !dep.in_flight {
            dep.not_before = dep.not_before.min(now);
        }
    }

    fn unblock(state: &mut State, ino: Ino) {
        let now = Instant::now();
        for entry in state.entries.values_mut() {
            if entry.blocked_on == Some(ino) {
                entry.blocked_on = None;
                entry.not_before = now;
            }
        }
    }

    /// Marks a step as running, unless the entry was parked or held since
    /// `ready` looked at it.
    pub(crate) fn start(&self, ino: Ino) -> bool {
        let mut state = self.state.lock();
        if state.stopping {
            return false;
        }
        match state.entries.get_mut(&ino) {
            Some(entry) => entry.start_step(),
            // Gone since the plan: `queue drop` took it.
            None => false,
        }
    }

    /// Starts an upload if fewer than `upload.concurrency` run, in this
    /// tree and in the session; it runs while the permit is held.
    pub(crate) fn try_start_upload(&self, ino: Ino) -> Option<UploadPermit> {
        let permit = UploadPermit {
            _tree: Arc::clone(&self.uploads).try_acquire_owned().ok()?,
            _session: Arc::clone(&self.session_uploads).try_acquire_owned().ok()?,
        };
        let mut state = self.state.lock();
        if state.stopping {
            return None;
        }
        let entry = state.entries.get_mut(&ino)?;
        entry.start_step().then_some(permit)
    }

    /// A step succeeded: re-plan the entry at once (it may need more).
    fn succeeded(&self, ino: Ino) {
        let now = Instant::now();
        {
            let mut state = self.state.lock();
            if let Some(entry) = state.entries.get_mut(&ino) {
                entry.in_flight = false;
                entry.attempts = 0;
                entry.replans = 0;
                entry.waits = 0;
                entry.retry_at = None;
                entry.not_before = now;
            }
            Self::unblock(&mut state, ino);
        }
        self.wake.notify_one();
        self.progress.notify_waiters();
    }

    /// A step ended in `Replan`: the entry is planned again at once, unless
    /// it keeps doing so without a step ever working; then it backs off like
    /// a failing step, so a stuck cycle costs few API calls.
    fn replanned(&self, ino: Ino) {
        let now = Instant::now();
        {
            let mut state = self.state.lock();
            if let Some(entry) = state.entries.get_mut(&ino) {
                entry.in_flight = false;
                entry.attempts = 0;
                entry.replans += 1;
                entry.not_before = if entry.replans > REPLAN_BURST {
                    tracing::warn!(ino, "planning the same step again and again");
                    now + backoff(entry.replans - REPLAN_BURST)
                } else {
                    now
                };
                entry.retry_at = (entry.not_before > now).then_some(entry.not_before);
            }
            Self::unblock(&mut state, ino);
        }
        self.wake.notify_one();
        self.progress.notify_waiters();
    }

    /// A step failed: back off, or park after too many attempts.
    fn failed(&self, ino: Ino, error: &BackendError) -> bool {
        let now = Instant::now();
        let parked = {
            let mut state = self.state.lock();
            let Some(entry) = state.entries.get_mut(&ino) else {
                return false;
            };
            entry.in_flight = false;
            // Without a connection or a session the entry is not at fault.
            let unreachable = error.is_unreachable();
            if !unreachable {
                entry.attempts += 1;
            }
            entry.last_error = Some(error.to_string());
            if error.is_permanent() && (entry.attempts >= PARK_AFTER || error.needs_the_user()) {
                entry.park(error.to_string());
                true
            } else {
                let mut delay = match error {
                    BackendError::RateLimited {
                        retry_after: Some(after),
                    } => *after,
                    _ if unreachable => BACKOFF_MIN,
                    _ => backoff(entry.attempts),
                };
                delay = delay.min(BACKOFF_MAX);
                entry.not_before = now + delay;
                entry.retry_at = Some(entry.not_before);
                false
            }
        };
        self.wake.notify_one();
        self.progress.notify_waiters();
        parked
    }

    /// A step found that `ino` must wait for the entry of `on` (a folder's
    /// trash for a child folder deleted, or a child moved out, inside it):
    /// the entry depends on `on`, which gets an entry if it has none, and is
    /// planned again as soon as `on`'s entry finishes a step or leaves.
    /// Each recheck asks the server, so without that wake it backs off by
    /// its waits in a row, which are not failed attempts (D121); `reason`
    /// is shown as its last error (D116). A change, retry or force made
    /// while the step ran wins: the entry is planned as that asked.
    fn wait_on(&self, ino: Ino, on: Ino, reason: &str) {
        let now = Instant::now();
        {
            let mut state = self.state.lock();
            let draining = state.draining;
            let Some(entry) = state.entries.get_mut(&ino) else {
                return;
            };
            entry.in_flight = false;
            if entry.generation == entry.step_generation {
                entry.waits += 1;
                entry.last_error = Some(reason.to_owned());
                entry.not_before = now + backoff(entry.waits);
                entry.retry_at = Some(entry.not_before);
                entry.blocked_on = Some(on);
                let new = !state.entries.contains_key(&on);
                let dep = self.entry_or_new(&mut state, on, now);
                dep.forced |= draining;
                dep.waited_into |= new;
            }
        }
        self.wake.notify_one();
        self.progress.notify_waiters();
    }

    fn park(&self, ino: Ino, reason: String) {
        let mut state = self.state.lock();
        if let Some(entry) = state.entries.get_mut(&ino) {
            entry.in_flight = false;
            entry.park(reason);
        }
        drop(state);
        self.progress.notify_waiters();
    }

    pub(crate) fn wake(&self) {
        self.wake.notify_one();
    }

    pub(crate) fn entries(&self) -> Vec<EntryView> {
        let now = Instant::now();
        let state = self.state.lock();
        state
            .entries
            .iter()
            .map(|(&ino, entry)| EntryView {
                ino,
                attempts: entry.attempts,
                parked: entry.parked.clone(),
                in_flight: entry.in_flight,
                last_error: entry.last_error.clone(),
                blocked_on: entry.blocked_on,
                age: entry.age(now),
                backoff: ((entry.attempts > 0 || entry.waits > 0) && entry.not_before > now)
                    .then(|| entry.not_before - now),
            })
            .collect()
    }

    /// `queue retry`: clears the park and the back-off, and forces.
    pub(crate) fn retry(&self, inos: &[Ino]) {
        let now = Instant::now();
        {
            let mut state = self.state.lock();
            for ino in inos {
                if let Some(entry) = state.entries.get_mut(ino) {
                    entry.parked = None;
                    entry.user_parked = false;
                    entry.attempts = 0;
                    entry.replans = 0;
                    entry.waits = 0;
                    entry.retry_at = None;
                    entry.forced = true;
                    entry.not_before = now;
                    entry.generation += 1;
                }
            }
        }
        self.wake.notify_one();
    }

    /// `queue park`: holds entries until `retry` or the next mount. A step
    /// that is running finishes.
    pub(crate) fn park_by_user(&self, inos: &[Ino]) {
        {
            let mut state = self.state.lock();
            for ino in inos {
                if let Some(entry) = state.entries.get_mut(ino) {
                    entry.parked = Some(USER_PARK_REASON.to_owned());
                    entry.user_parked = true;
                    entry.generation += 1;
                }
            }
        }
        self.progress.notify_waiters();
    }

    /// Holds every entry, and every entry made until
    /// [`Self::release_held`], with `reason` (the tree lost write access,
    /// D114): no step starts, and a local change does not lift it, as with
    /// `queue park`. An idle entry is parked with `reason` at once; one
    /// parked already keeps its reason, as does one the user parked; one
    /// whose step runs, or one made meanwhile, is planned as usual and
    /// parked with `reason` instead of a step (D121). Returns how many
    /// entries are held now, those the user parked left out.
    pub(crate) fn hold_all(&self, reason: &str) -> usize {
        let held = {
            let mut state = self.state.lock();
            state.hold = Some(reason.to_owned());
            for entry in state.entries.values_mut() {
                if entry.hold.is_some() {
                    continue;
                }
                entry.hold = Some(reason.to_owned());
                entry.generation += 1;
                if entry.parked.is_none() && !entry.in_flight {
                    entry.parked = Some(reason.to_owned());
                }
            }
            state
                .entries
                .values()
                .filter(|entry| entry.hold.is_some() && !entry.user_parked)
                .count()
        };
        self.progress.notify_waiters();
        held
    }

    /// Lifts the hold of [`Self::hold_all`] with `reason` and forces the
    /// entries it parked; returns how many. Entries parked for another
    /// reason stay parked (D121).
    pub(crate) fn release_held(&self, reason: &str) -> usize {
        let held: Vec<Ino> = {
            let mut state = self.state.lock();
            if state.hold.as_deref() == Some(reason) {
                state.hold = None;
            }
            let mut held = Vec::new();
            for (&ino, entry) in &mut state.entries {
                if entry.hold.as_deref() != Some(reason) {
                    continue;
                }
                entry.hold = None;
                if entry.parked.as_deref() == Some(reason) && !entry.user_parked {
                    held.push(ino);
                }
            }
            held
        };
        if !held.is_empty() {
            self.retry(&held);
        }
        self.wake.notify_one();
        held.len()
    }

    /// Keeps steps from starting for these entries, all of them or, when
    /// one is running (returned), none.
    pub(crate) fn hold(&self, inos: &[Ino]) -> std::result::Result<(), Ino> {
        let mut state = self.state.lock();
        if let Some(&running) = inos
            .iter()
            .find(|ino| state.entries.get(ino).is_some_and(|e| e.in_flight))
        {
            return Err(running);
        }
        for ino in inos {
            if let Some(entry) = state.entries.get_mut(ino) {
                entry.held = true;
                entry.generation += 1;
            }
        }
        Ok(())
    }

    /// Ends a [`Self::hold`] of the entries still there.
    pub(crate) fn release(&self, inos: &[Ino]) {
        {
            let mut state = self.state.lock();
            for ino in inos {
                if let Some(entry) = state.entries.get_mut(ino) {
                    entry.held = false;
                }
            }
        }
        self.wake.notify_one();
    }

    /// The local intent of the entry was discarded; a held entry leaves.
    pub(crate) fn forget(&self, ino: Ino) {
        {
            let mut state = self.state.lock();
            if state.entries.remove(&ino).is_some() {
                self.record_left(ino);
            }
            Self::unblock(&mut state, ino);
        }
        self.wake.notify_one();
        self.progress.notify_waiters();
    }

    /// Forces `selected` and what they wait for (once each: a forced entry
    /// keeps its back-off), and counts what can still finish.
    pub(crate) fn sync_round(&self, selected: &[Ino]) -> SyncRound {
        self.force_round(selected, true)
    }

    /// [`Self::sync_round`] for a shutdown flush (D075): skips the debounce,
    /// but an entry in its back-off after a failure (HTTP 429 included)
    /// waits for its end.
    pub(crate) fn flush_round(&self, selected: &[Ino]) -> SyncRound {
        self.force_round(selected, false)
    }

    /// Entries that are not parked.
    pub(crate) fn unparked(&self) -> Vec<Ino> {
        let state = self.state.lock();
        state
            .entries
            .iter()
            .filter(|(_, entry)| entry.parked.is_none())
            .map(|(&ino, _)| ino)
            .collect()
    }

    /// Of `inos`, how many still have an entry that is not parked, and how
    /// many a parked one.
    pub(crate) fn queued_among(&self, inos: &[Ino]) -> (usize, usize) {
        let state = self.state.lock();
        let (mut pending, mut parked) = (0, 0);
        for entry in inos.iter().filter_map(|ino| state.entries.get(ino)) {
            if entry.parked.is_some() {
                parked += 1;
            } else {
                pending += 1;
            }
        }
        (pending, parked)
    }

    /// How many of `inos` still have an entry.
    pub(crate) fn still_queued(&self, inos: &[Ino]) -> usize {
        let state = self.state.lock();
        inos.iter()
            .filter(|ino| state.entries.contains_key(ino))
            .count()
    }

    /// `hurry`: a forced entry starts now even in its back-off (`sync`).
    fn force_round(&self, selected: &[Ino], hurry: bool) -> SyncRound {
        let now = Instant::now();
        let mut round = SyncRound::default();
        let mut woke = false;
        {
            let mut state = self.state.lock();
            for &ino in selected {
                let Some(entry) = state.entries.get(&ino) else {
                    continue;
                };
                if entry.parked.is_none() {
                    round.pending += 1;
                }
                let mut current = ino;
                let mut steps = 0;
                let held_up_by = loop {
                    let Some(entry) = state.entries.get_mut(&current) else {
                        break None;
                    };
                    if entry.parked.is_some() {
                        break Some(current);
                    }
                    if !entry.forced {
                        entry.forced = true;
                        let start = match entry.retry_at {
                            Some(retry_at) if !hurry => retry_at.max(now),
                            _ => now,
                        };
                        entry.not_before = entry.not_before.min(start);
                        entry.generation += 1;
                        woke = true;
                    }
                    steps += 1;
                    match entry.blocked_on {
                        // A cycle is the planner's to break.
                        Some(next) if steps <= MAX_DEPENDENCY_CHAIN && !entry.in_flight => {
                            current = next;
                        }
                        _ => break None,
                    }
                };
                match held_up_by {
                    Some(parked) if !round.parked.contains(&parked) => round.parked.push(parked),
                    Some(_) => {}
                    None => round.moving += 1,
                }
            }
        }
        if woke {
            self.wake.notify_one();
        }
        round
    }

    /// The connection is back: every entry that is not parked retries now.
    pub(crate) fn wake_all(&self) {
        let now = Instant::now();
        {
            let mut state = self.state.lock();
            for entry in state.entries.values_mut().filter(|e| e.parked.is_none()) {
                entry.not_before = entry.not_before.min(now);
            }
        }
        self.wake.notify_one();
    }
}

pub(crate) fn backoff(attempts: u32) -> Duration {
    let base = BACKOFF_MIN.saturating_mul(1 << attempts.min(9).saturating_sub(1));
    let jitter = rand::random::<f64>() * 0.3 + 0.85;
    base.mul_f64(jitter).min(BACKOFF_MAX)
}

/// The temporary name that frees a node's remote place (D117).
pub(crate) fn swap_name(ino: Ino) -> String {
    format!("{SWAP_PREFIX}{ino}")
}

/// Whether `from`, followed through what each node waits for by the tree
/// (the folder it goes into while that has no link, the node that holds
/// its target name remotely), comes back to `ino`: a cycle of nodes
/// waiting for each other's places, such as two names swapped, three
/// rotated, or a folder created at the old name of a node moved into it
/// (D117). A tombstone ends the walk; its trash frees its name itself.
fn waits_for_me(tree: &Tree, from: Ino, ino: Ino) -> bool {
    let mut current = from;
    for _ in 0..MAX_DEPENDENCY_CHAIN {
        if current == ino {
            return true;
        }
        let Some(node) = tree.get(current) else {
            return false;
        };
        if !node.attached || current == ROOT_INO || (node.local_only && node.remote.is_none()) {
            return false;
        }
        current = match tree.link_of(node.parent) {
            None => node.parent,
            Some(parent) => match tree.remote_occupant(parent, &node.name) {
                Some(occupant) if occupant != current => occupant,
                _ => return false,
            },
        };
    }
    false
}

/// Whether the attached node `ino` is inside a local-only folder (§7.7):
/// the planner has nothing to do for it, ever, so a node with a remote
/// place keeps that place remotely.
fn in_local_only_folder(tree: &Tree, ino: Ino) -> bool {
    tree.get(ino)
        .filter(|node| node.attached)
        .and_then(|node| tree.get(node.parent))
        .is_some_and(|parent| parent.local_only && parent.remote.is_none())
}

/// The next remote step for an inode.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Plan {
    Done,
    Wait(Instant),
    Blocked(Ino),
    /// `upload.meta_revision` declines a metadata-only revision.
    SkipMeta,
    Work(Work),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Work {
    CreateFolder {
        parent: LinkId,
        name: String,
        mtime: SystemTime,
    },
    Upload(Target),
    Move {
        link: LinkId,
        new_parent: Option<LinkId>,
        name: String,
        expected: Option<NameHash>,
    },
    Trash {
        link: LinkId,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Target {
    New { parent: LinkId, name: String },
    Revision { link: LinkId },
}

/// What the trash pre-check of a tombstone found (§8.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrashCheck {
    /// Nothing the local delete did not know about.
    Trash,
    /// The remote node is kept and shown again.
    Kept,
    /// The entry of `on` comes first.
    Wait { on: Ino, reason: &'static str },
}

/// What a snapshot captured, for the commit.
struct Snapshot {
    path: PathBuf,
    content_gen: u64,
    meta_gen: u64,
    size: u64,
    sha1: [u8; 20],
    mtime: SystemTime,
    mode: u32,
    base_rev: Option<RevisionId>,
}

impl Engine {
    /// The executor loop; runs until the engine shuts down.
    pub(crate) async fn run_journal(self: Arc<Self>) {
        loop {
            self.health.wait_online().await;
            for dir in self.journal.take_listings() {
                let this = Arc::clone(&self);
                self.runtime().spawn(async move {
                    match this.list(dir).await {
                        Ok(()) => this.journal.listed(dir, |open| this.save_relist(open)),
                        // Kept in the store: the next start lists it (D069).
                        Err(error) => {
                            tracing::debug!(dir, %error, "listing after a freed name failed");
                        }
                    }
                });
            }
            let now = Instant::now();
            let (ready, next) = self.journal.ready(now);
            let mut trash: Vec<(Ino, LinkId)> = Vec::new();
            for (ino, timing) in ready {
                let plan = self.plan(ino, timing, now);
                tracing::trace!(ino, ?plan, forced = timing.forced, "journal plan");
                match plan {
                    Plan::Done => self.journal.done(ino, timing.generation),
                    Plan::Wait(until) => self.journal.defer(ino, until, timing.generation),
                    Plan::Blocked(on) => self.journal.block(ino, on, timing.generation),
                    Plan::SkipMeta => self.skip_meta(ino, timing.generation),
                    Plan::Work(Work::Trash { link }) => {
                        if self.journal.start(ino) {
                            trash.push((ino, link));
                        }
                    }
                    Plan::Work(Work::Upload(target)) => {
                        if let Some(permit) = self.journal.try_start_upload(ino) {
                            let this = Arc::clone(&self);
                            self.runtime().spawn(async move {
                                this.upload(ino, target, permit).await;
                            });
                        } else {
                            self.journal.defer(
                                ino,
                                now + Duration::from_millis(200),
                                timing.generation,
                            );
                        }
                    }
                    Plan::Work(work) => {
                        if self.journal.start(ino) {
                            self.execute(ino, work).await;
                        }
                    }
                }
            }
            for batch in trash.chunks(TRASH_BATCH) {
                self.trash(batch).await;
            }
            let sleep = next.map_or(MAX_WAKE, |at| at.saturating_duration_since(Instant::now()));
            tokio::select! {
                () = self.journal.wake.notified() => {}
                () = tokio::time::sleep(sleep.min(MAX_WAKE)) => {}
            }
        }
    }

    fn plan(&self, ino: Ino, timing: Timing, now: Instant) -> Plan {
        let tree = self.tree.read();
        let Some(node) = tree.get(ino) else {
            return Plan::Done;
        };
        if node.deleted {
            return match node.link() {
                Some(link) => Plan::Work(Work::Trash { link: link.clone() }),
                None => Plan::Done,
            };
        }
        if !node.attached || ino == ROOT_INO || (node.local_only && node.remote.is_none()) {
            return Plan::Done;
        }
        if in_local_only_folder(&tree, ino) {
            return Plan::Done;
        }
        // Waits for `on`; if that waits for this node in turn, a node that
        // has a remote place frees it under a temporary name in its folder
        // (D117).
        let blocked = |on: Ino| -> Plan {
            match &node.remote {
                Some(remote) if remote.name != swap_name(ino) && waits_for_me(&tree, on, ino) => {
                    Plan::Work(Work::Move {
                        link: remote.link.clone(),
                        new_parent: None,
                        name: swap_name(ino),
                        expected: remote.name_hash.clone(),
                    })
                }
                _ => Plan::Blocked(on),
            }
        };
        let Some(parent) = tree.link_of(node.parent).cloned() else {
            return blocked(node.parent);
        };
        // The target name must be free remotely, or held by this node.
        let reserve = |plan: Plan| -> Plan {
            match tree.remote_occupant(&parent, &node.name) {
                Some(occupant) if occupant != ino => blocked(occupant),
                _ => plan,
            }
        };
        let placement = tree.placement_pending(ino);
        match (node.kind, &node.remote) {
            (NodeKind::Folder, None) => reserve(Plan::Work(Work::CreateFolder {
                parent: parent.clone(),
                name: node.name.clone(),
                mtime: node.mtime,
            })),
            (_, Some(remote)) if placement => {
                let new_parent =
                    (remote.parent_link.as_ref() != Some(&parent)).then(|| parent.clone());
                reserve(Plan::Work(Work::Move {
                    link: remote.link.clone(),
                    new_parent,
                    name: node.name.clone(),
                    expected: remote.name_hash.clone(),
                }))
            }
            (NodeKind::Folder, Some(_)) => Plan::Done,
            (NodeKind::File, None) => {
                match self.upload_wait(node.open_writers, timing, now, false, false) {
                    Some(until) => Plan::Wait(until),
                    None => reserve(Plan::Work(Work::Upload(Target::New {
                        parent: parent.clone(),
                        name: node.name.clone(),
                    }))),
                }
            }
            (NodeKind::File, Some(remote)) => {
                let content = node.content_pending();
                if !content && !node.meta_pending() {
                    return Plan::Done;
                }
                if !content {
                    let declined = match self.journal.meta_revision {
                        MetaRevisionPolicy::Always => false,
                        MetaRevisionPolicy::Never => true,
                        MetaRevisionPolicy::MaxSize(max) => node.size > max.0,
                    };
                    if declined {
                        return Plan::SkipMeta;
                    }
                }
                match self.upload_wait(node.open_writers, timing, now, !content, node.remote_newer)
                {
                    Some(until) => Plan::Wait(until),
                    None => Plan::Work(Work::Upload(Target::Revision {
                        link: remote.link.clone(),
                    })),
                }
            }
        }
    }

    /// When an upload may start; `None` means now.
    fn upload_wait(
        &self,
        writers: u32,
        timing: Timing,
        now: Instant,
        meta_only: bool,
        remote_newer: bool,
    ) -> Option<Instant> {
        if timing.forced {
            return None;
        }
        if remote_newer && writers == 0 {
            // The conflict is known: settle it now instead of after the
            // debounce (§10.4).
            return None;
        }
        if now.saturating_duration_since(timing.dirty_since) >= self.journal.max_dirty_age {
            return None;
        }
        let quiet = if meta_only {
            META_DELAY
        } else {
            self.journal.debounce
        };
        let ready_at = timing.changed_at + quiet;
        if writers > 0 {
            // `release` touches the entry again.
            return Some(now.max(ready_at) + quiet);
        }
        (now < ready_at).then_some(ready_at)
    }

    fn skip_meta(&self, ino: Ino, generation: u64) {
        if let Some(node) = self.tree.write().get_mut(ino) {
            node.gens.uploaded_meta = node.gens.meta;
        }
        self.persist(&[ino]);
        self.journal.done(ino, generation);
    }

    async fn execute(self: &Arc<Self>, ino: Ino, work: Work) {
        let result = match work {
            Work::CreateFolder {
                parent,
                name,
                mtime,
            } => self.create_folder(ino, parent, name, mtime).await,
            Work::Move {
                link,
                new_parent,
                name,
                expected,
            } => self.move_node(ino, link, new_parent, name, expected).await,
            Work::Upload(_) | Work::Trash { .. } => unreachable!("handled by the loop"),
        };
        self.step_ended(ino, result);
    }

    /// Records how an inline step ended.
    fn step_ended(&self, ino: Ino, result: std::result::Result<(), UploadError>) {
        match result {
            // `Replan`: the tree changed under the step; look at the node again.
            Ok(()) => self.journal.succeeded(ino),
            Err(UploadError::Replan) => self.journal.replanned(ino),
            Err(UploadError::Backend(error)) => self.step_failed(ino, &error),
            Err(UploadError::Park(reason)) => {
                tracing::warn!(ino, reason, "step parked");
                self.journal.park(ino, reason);
            }
        }
    }

    fn step_failed(&self, ino: Ino, error: &BackendError) {
        tracing::warn!(ino, %error, "journal step failed");
        // A tree whose gate follows the server's answer (a share, D114):
        // every entry is held before this one is marked failed, which then
        // keeps the hold's reason.
        if *error == BackendError::PermissionDenied {
            self.write_refused();
        }
        let deferred = match error {
            BackendError::QuotaExceeded => Some(nix::libc::ENOSPC),
            BackendError::FileTooLarge => Some(nix::libc::EFBIG),
            _ => None,
        };
        if let Some(errno) = deferred {
            if let Some(node) = self.tree.write().get_mut(ino) {
                node.deferred_error = Some(errno);
            }
            self.persist(&[ino]);
        }
        if self.journal.failed(ino, error) {
            tracing::warn!(ino, %error, "journal entry parked");
        }
    }

    async fn create_folder(
        self: &Arc<Self>,
        ino: Ino,
        parent: LinkId,
        name: String,
        mtime: SystemTime,
    ) -> std::result::Result<(), UploadError> {
        check_not_hot(&self.metrics, "create_folder");
        match self.backend.create_folder(&parent, &name, mtime).await {
            Ok(link) => {
                self.bind_folder(
                    ino,
                    RemoteState {
                        link,
                        parent_link: Some(parent),
                        name,
                        rev: None,
                        size: 0,
                        mtime,
                        mode: None,
                        sha1: None,
                        name_hash: None,
                        unverified: None,
                    },
                );
                Ok(())
            }
            // A node of that name exists remotely: merge into a folder, give
            // way to anything else (§15.3 M7).
            Err(BackendError::AlreadyExists { existing }) => {
                Err(self.name_clash(ino, &parent, &name, existing, None).await)
            }
            Err(BackendError::NotFound) => Err(self.parent_vanished(&parent).await),
            Err(error) => Err(error.into()),
        }
    }

    /// Records the remote folder behind a folder created locally.
    fn bind_folder(&self, ino: Ino, remote: RemoteState) {
        {
            let stamp = self.stamp();
            let mut tree = self.tree.write();
            let link = remote.link.clone();
            tree.set_remote(ino, Some(remote));
            if let Some(node) = tree.get_mut(ino) {
                node.content = Content::Remote { link, rev: None };
                node.remote_stamp = stamp;
                if !node.attached {
                    node.deleted = true;
                }
            }
        }
        self.persist(&[ino]);
    }

    /// A rename or move, after a fresh look at the node (§8.3, §10.5): the
    /// remote may have moved it, or trashed it, since the plan was made.
    async fn move_node(
        self: &Arc<Self>,
        ino: Ino,
        link: LinkId,
        new_parent: Option<LinkId>,
        name: String,
        expected: Option<NameHash>,
    ) -> std::result::Result<(), UploadError> {
        check_not_hot(&self.metrics, "rename");
        let fresh = self.fresh_or_vanished(ino, &link).await?;
        let known = self
            .tree
            .read()
            .get(ino)
            .and_then(|node| node.remote.clone())
            .filter(|remote| remote.link == link)
            .ok_or_else(|| UploadError::retry("the remote node changed before the rename"))?;
        let target = new_parent.clone().or_else(|| known.parent_link.clone());
        let placement = (known.parent_link.clone(), known.name.clone());
        if (&fresh.parent_link_id, &fresh.name) != (&placement.0, &placement.1) {
            // The remote committed first, or an earlier step did.
            return self.moved_meanwhile(ino, &fresh, target.as_ref(), &name);
        }
        // The precondition is the hash read just now, never an older one.
        let hash = fresh.name_hash.clone().or(expected);
        if hash != known.name_hash {
            let stamp = self.stamp();
            let mut tree = self.tree.write();
            tree.set_remote_at(
                ino,
                Some(RemoteState {
                    name_hash: hash.clone(),
                    ..known
                }),
                stamp,
            );
            drop(tree);
            self.persist(&[ino]);
        }
        match self
            .backend
            .rename(&link, new_parent.as_ref(), &name, hash.as_ref())
            .await
        {
            Ok(()) => {}
            Err(BackendError::NameConflict) => {
                // The hash went stale between the look and the rename.
                let fresh = self.fresh_or_vanished(ino, &link).await?;
                if (&fresh.parent_link_id, &fresh.name) == (&placement.0, &placement.1) {
                    return Err(UploadError::Park(
                        "the server refused the name hash of an unchanged node".into(),
                    ));
                }
                return self.moved_meanwhile(ino, &fresh, target.as_ref(), &name);
            }
            Err(BackendError::InvalidMove) => {
                let fresh = self.fresh_or_vanished(ino, &link).await?;
                return Err(self.keep_server_placement(ino, &fresh));
            }
            Err(BackendError::AlreadyExists { .. }) => {
                let Some(parent) = target else {
                    return Err(UploadError::Park(
                        "the folder of the rename is unknown".into(),
                    ));
                };
                return Err(
                    match self
                        .yield_to_remote(ino, &parent, ConflictKind::NameTaken, Some(link), None)
                        .await
                    {
                        Ok(()) => UploadError::Replan,
                        Err(error) => error,
                    },
                );
            }
            Err(BackendError::NotFound) => {
                // The node, or the folder it moves into, is gone.
                return Err(match (&new_parent, self.fresh_live(&link).await?) {
                    (Some(parent), Some(_)) => self.parent_vanished(parent).await,
                    _ => self.vanished(ino).await,
                });
            }
            Err(error) => return Err(error.into()),
        }
        {
            let stamp = self.stamp();
            let mut tree = self.tree.write();
            if let Some(mut remote) = tree.get(ino).and_then(|n| n.remote.clone())
                && remote.link == link
            {
                if let Some(parent) = new_parent {
                    remote.parent_link = Some(parent);
                }
                remote.name = name;
                remote.name_hash = None;
                tree.set_remote(ino, Some(remote));
                if let Some(node) = tree.get_mut(ino) {
                    node.remote_stamp = stamp;
                }
            }
        }
        self.persist(&[ino]);
        Ok(())
    }

    /// The remote node `fresh` is not where the node was known to be. Found
    /// at its own temporary name in its known folder, it is this device's
    /// move aside (D117) whose reply was lost, or whose row a crash did not
    /// commit: that step took effect, and only the remote state follows, so
    /// that the plan goes on to the node's own name (D117 amended, V-7).
    /// The node never takes the temporary name locally. Anything else was
    /// moved by another device ([`Engine::remote_moved_first`]).
    fn moved_meanwhile(
        &self,
        ino: Ino,
        fresh: &RemoteNode,
        target: Option<&LinkId>,
        name: &str,
    ) -> std::result::Result<(), UploadError> {
        if !self.found_aside(ino, fresh) {
            return Err(self.remote_moved_first(ino, fresh, target, name));
        }
        tracing::debug!(ino, "the move aside took effect before; its reply was lost");
        if name == swap_name(ino) {
            Ok(())
        } else {
            Err(UploadError::Replan)
        }
    }

    /// Whether `fresh` is the node `ino` at its own temporary name in the
    /// folder it was known in; if so, that becomes its remote state, and
    /// its local placement stays.
    fn found_aside(&self, ino: Ino, fresh: &RemoteNode) -> bool {
        let stamp = self.stamp();
        {
            let mut tree = self.tree.write();
            let Some(known) = tree
                .get(ino)
                .and_then(|node| node.remote.clone())
                .filter(|remote| remote.link == fresh.link_id)
            else {
                return false;
            };
            if fresh.name != swap_name(ino) || fresh.parent_link_id != known.parent_link {
                return false;
            }
            tree.set_remote_at(ino, Some(known.with_placement_of(fresh)), stamp);
        }
        self.persist(&[ino]);
        true
    }

    /// The remote node `fresh` was moved or renamed before our rename ran.
    /// The one that committed first wins (§10.5): the inode follows the
    /// remote placement, unless that is impossible (an unlisted folder, a
    /// taken name), when the local placement is kept and pushed afterwards.
    /// Always `Replan`.
    fn remote_moved_first(
        &self,
        ino: Ino,
        fresh: &RemoteNode,
        target: Option<&LinkId>,
        name: &str,
    ) -> UploadError {
        let Some((orig_name, adopted)) = self.take_remote_placement(ino, fresh) else {
            return UploadError::Replan;
        };
        if fresh.parent_link_id.as_ref() == target && fresh.name == name {
            // The other device made the very same move.
            if let Some(changes) = adopted {
                self.apply_changes(changes);
            }
            return UploadError::Replan;
        }
        let detail = match adopted {
            Some(changes) => {
                self.apply_changes(changes);
                format!("kept {} from another device", fresh.name)
            }
            None => {
                tracing::warn!(ino, name = fresh.name, "local placement kept");
                format!(
                    "moved remotely to {} but the folder is not shown here; local placement kept",
                    fresh.name
                )
            }
        };
        self.record_conflict(ConflictNote {
            link: Some(fresh.link_id.clone()),
            remote_rev: fresh.active_revision.clone(),
            detail: Some(detail),
            ..ConflictNote::new(ConflictKind::RenameRename, ino, orig_name)
        });
        UploadError::Replan
    }

    /// The server refused a move into the node's own subtree (another device
    /// moved things meanwhile): its placement stays (§10.5).
    fn keep_server_placement(&self, ino: Ino, fresh: &RemoteNode) -> UploadError {
        let Some((orig_name, adopted)) = self.take_remote_placement(ino, fresh) else {
            return UploadError::Replan;
        };
        let Some(changes) = adopted else {
            return UploadError::Park(format!(
                "move cycle: the server keeps {} in place and its folder is not shown here",
                fresh.name
            ));
        };
        self.apply_changes(changes);
        self.record_conflict(ConflictNote {
            link: Some(fresh.link_id.clone()),
            detail: Some("the server refused a move into the folder's own subtree".to_owned()),
            ..ConflictNote::new(ConflictKind::MoveCycle, ino, orig_name)
        });
        UploadError::Replan
    }

    /// Records the remote placement of `fresh` as the node's and moves the
    /// inode there if it can. Returns its local name from before, and the
    /// changes (`None`: it cannot follow).
    fn take_remote_placement(
        &self,
        ino: Ino,
        fresh: &RemoteNode,
    ) -> Option<(String, Option<Changes>)> {
        let stamp = self.stamp();
        let (orig_name, adopted) = {
            let mut tree = self.tree.write();
            let node = tree.get(ino)?;
            let known = node
                .remote
                .clone()
                .filter(|remote| remote.link == fresh.link_id)?;
            let orig_name = node.name.clone();
            tree.set_remote_at(ino, Some(known.with_placement_of(fresh)), stamp);
            (orig_name, tree.adopt_remote_placement(ino))
        };
        self.persist(&[ino]);
        Some((orig_name, adopted))
    }

    /// Trashes remote nodes after a fresh look (§8.4, §10.5): what changed
    /// remotely since the local delete is not trashed but comes back.
    async fn trash(self: &Arc<Self>, batch: &[(Ino, LinkId)]) {
        check_not_hot(&self.metrics, "trash");
        let links: Vec<LinkId> = batch.iter().map(|(_, link)| link.clone()).collect();
        check_not_hot(&self.metrics, "get_nodes");
        let fresh = match self.backend.get_nodes(&links).await {
            Ok(fresh) => fresh,
            Err(error) => {
                for (ino, _) in batch {
                    self.step_failed(*ino, &error);
                }
                return;
            }
        };
        let mut send: Vec<(Ino, LinkId)> = Vec::new();
        for ((ino, link), node) in batch.iter().zip(fresh) {
            // Gone already (e.g. trashed with its folder) counts as done.
            let Some(node) = node.filter(|node| !node.trashed) else {
                self.trash_finished(*ino, link);
                continue;
            };
            // A wait is recorded before this batch's trash call, so a child
            // trashed by it wakes the folder at once.
            match self.trash_check(*ino, &node).await {
                Ok(TrashCheck::Trash) => send.push((*ino, link.clone())),
                Ok(TrashCheck::Kept) => {}
                // Something waits for the name meanwhile, maybe the very
                // child the trash waits for: free the name first (D117).
                Ok(TrashCheck::Wait { .. }) if self.name_wanted(*ino) => {
                    tracing::debug!(ino, "trash waits; its name goes to a temporary one");
                    let result = self.move_aside(*ino, &node).await;
                    self.step_ended(*ino, result);
                }
                Ok(TrashCheck::Wait { on, reason }) => {
                    tracing::debug!(ino, on, reason, "trash waits");
                    self.journal.wait_on(*ino, on, reason);
                }
                Err(error) => self.step_failed(*ino, &error),
            }
        }
        if send.is_empty() {
            return;
        }
        let links: Vec<LinkId> = send.iter().map(|(_, link)| link.clone()).collect();
        let results = match self.backend.trash(&links).await {
            Ok(results) => results,
            Err(error) => {
                for (ino, _) in &send {
                    self.step_failed(*ino, &error);
                }
                return;
            }
        };
        for ((ino, link), result) in send.iter().zip(results) {
            match result {
                Ok(()) | Err(BackendError::NotFound) => self.trash_finished(*ino, link),
                Err(error) => self.step_failed(*ino, &error),
            }
        }
    }

    /// Whether a local node other than the tombstone `ino` is placed where
    /// `ino` still is remotely, so that it waits for that name.
    fn name_wanted(&self, ino: Ino) -> bool {
        let tree = self.tree.read();
        let Some(remote) = tree.get(ino).and_then(|node| node.remote.as_ref()) else {
            return false;
        };
        if remote.name == swap_name(ino) {
            return false;
        }
        let Some(dir) = remote
            .parent_link
            .as_ref()
            .and_then(|link| tree.by_link(link))
        else {
            return false;
        };
        tree.child(dir, &remote.name)
            .ok()
            .flatten()
            .is_some_and(|holder| holder != ino)
    }

    /// Renames the tombstone `ino`, whose trash waits, to a temporary name
    /// in its folder: a node waits for its name, and may be the child the
    /// trash waits for, so neither would go on (D117). The trash follows
    /// under that name.
    async fn move_aside(
        self: &Arc<Self>,
        ino: Ino,
        fresh: &RemoteNode,
    ) -> std::result::Result<(), UploadError> {
        check_not_hot(&self.metrics, "rename");
        let known = self
            .tree
            .read()
            .get(ino)
            .and_then(|node| node.remote.clone())
            .filter(|remote| remote.link == fresh.link_id)
            .ok_or(UploadError::Replan)?;
        let name = swap_name(ino);
        let own_name = known.name.clone();
        let moved = (&fresh.parent_link_id, &fresh.name) != (&known.parent_link, &known.name);
        if !moved {
            let hash = fresh.name_hash.clone().or_else(|| known.name_hash.clone());
            match self
                .backend
                .rename(&fresh.link_id, None, &name, hash.as_ref())
                .await
            {
                Ok(()) => {}
                // Gone: the next look finishes the entry.
                Err(BackendError::NotFound) => return Err(UploadError::Replan),
                Err(error) => return Err(error.into()),
            }
        }
        {
            let stamp = self.stamp();
            let mut tree = self.tree.write();
            if tree.link_of(ino) == Some(&fresh.link_id) {
                // Moved by another device meanwhile: the name is free.
                let remote = if moved {
                    known.with_placement_of(fresh)
                } else {
                    RemoteState {
                        name,
                        name_hash: None,
                        ..known
                    }
                };
                tree.set_remote_at(ino, Some(remote), stamp);
                // The tombstone keeps the name it had remotely, which
                // `queue` and an export show (D121).
                if !moved
                    && let Some(node) = tree.get_mut(ino)
                    && !node.attached
                {
                    node.name = own_name;
                }
            }
        }
        self.persist(&[ino]);
        Ok(())
    }

    fn trash_finished(&self, ino: Ino, link: &LinkId) {
        {
            let stamp = self.stamp();
            let mut tree = self.tree.write();
            tree.note_trashed(link.clone(), stamp);
            if tree.link_of(ino) == Some(link) {
                tree.set_remote(ino, None);
            }
            if let Some(node) = tree.get_mut(ino) {
                node.deleted = false;
            }
        }
        self.persist(&[ino]);
        self.journal.succeeded(ino);
        self.drop_detached(ino);
    }

    /// The revision a node's content builds on, and whether a newer remote
    /// revision is known. `None` for an unknown node.
    fn base_of(&self, ino: Ino) -> Option<(Option<RevisionId>, bool)> {
        let staged_base = self
            .staged_entry(ino)
            .and_then(|staged| staged.lock().base_rev().cloned());
        let tree = self.tree.read();
        let node = tree.get(ino)?;
        let base = staged_base.or_else(|| node.remote.as_ref().and_then(|r| r.rev.clone()));
        Some((base, node.remote_newer))
    }

    /// Whether the tombstone `ino` may be trashed now. Kept when the remote
    /// node has something the local delete did not know about: the node is
    /// shown again.
    async fn trash_check(
        self: &Arc<Self>,
        ino: Ino,
        fresh: &RemoteNode,
    ) -> std::result::Result<TrashCheck, BackendError> {
        let Some((base, remote_newer)) = self.base_of(ino) else {
            return Ok(TrashCheck::Trash);
        };
        let (kind, detail, local_rev) = match fresh.kind {
            NodeKind::File => {
                if fresh.active_revision == base && !remote_newer {
                    return Ok(TrashCheck::Trash);
                }
                (ConflictKind::LocalDeleteRemoteEdit, None, base)
            }
            NodeKind::Folder => {
                check_not_hot(&self.metrics, "list_children");
                let (kids, unreadable) =
                    match self.backend.list_children_checked(&fresh.link_id).await {
                        Ok(listing) => (listing.children, listing.degraded.len()),
                        // The folder is gone; the trash call says so.
                        Err(BackendError::NotFound) => return Ok(TrashCheck::Trash),
                        Err(error) => return Err(error),
                    };
                // Kids the local tree knows: moved elsewhere (wait for the
                // move), or deleted too (their own entries trash them; wait
                // for deleted folders). A wait depends on the kid's entry,
                // so it ends when that entry does, not after a back-off:
                // a deep tree goes level by level (D116).
                let known: Vec<Option<Ino>> = {
                    let tree = self.tree.read();
                    kids.iter().map(|kid| tree.by_link(&kid.link_id)).collect()
                };
                // A kid that cannot be decrypted cannot be checked: it counts
                // as one the delete does not account for (D132).
                let mut foreign = unreadable;
                for (kid, held) in kids.iter().zip(known) {
                    let Some(held) = held else {
                        foreign += 1;
                        continue;
                    };
                    let (attached, deleted, stays) = {
                        let tree = self.tree.read();
                        let node = tree.get(held);
                        (
                            node.is_some_and(|n| n.attached),
                            node.is_some_and(|n| n.deleted),
                            in_local_only_folder(&tree, held),
                        )
                    };
                    if stays {
                        // Moved into a local-only folder: it never leaves
                        // this folder remotely, and its trash would take
                        // the file along. Kept like a child the delete
                        // does not account for (D121).
                        foreign += 1;
                        continue;
                    }
                    if attached {
                        return Ok(TrashCheck::Wait {
                            on: held,
                            reason: "waiting for moves out of the folder",
                        });
                    }
                    if deleted && kid.kind == NodeKind::Folder {
                        // Its own check lists it; until then something new
                        // inside it would go to the trash with this folder.
                        return Ok(TrashCheck::Wait {
                            on: held,
                            reason: "waiting for the folders deleted inside it",
                        });
                    }
                    let changed = match self.base_of(held) {
                        Some((base, newer)) if deleted => {
                            kid.kind == NodeKind::File && (kid.active_revision != base || newer)
                        }
                        _ => true,
                    };
                    if changed {
                        foreign += 1;
                    }
                }
                if foreign == 0 {
                    return Ok(TrashCheck::Trash);
                }
                (
                    ConflictKind::LocalDeleteRemoteChildren,
                    Some(foreign.to_string()),
                    None,
                )
            }
        };
        self.keep_remote(ino, fresh, kind, detail, local_rev).await;
        Ok(TrashCheck::Kept)
    }

    /// Does not trash the tombstone `ino`: forgets it and lists the remote
    /// parent, so that the remote node (with whatever is new in it) appears
    /// again, and records why.
    async fn keep_remote(
        self: &Arc<Self>,
        ino: Ino,
        fresh: &RemoteNode,
        kind: ConflictKind,
        detail: Option<String>,
        local_rev: Option<RevisionId>,
    ) {
        let orig_name = self
            .tree
            .read()
            .get(ino)
            .map(|node| node.name.clone())
            .unwrap_or_else(|| fresh.name.clone());
        let unbound = self.tree.write().unbind_tombstone(ino);
        if let Some(changes) = unbound {
            self.apply_changes(changes);
        }
        self.journal.succeeded(ino);
        let parent = fresh
            .parent_link_id
            .as_ref()
            .and_then(|link| self.tree.read().by_link(link));
        if let Some(dir) = parent
            && let Err(error) = self.list(dir).await
        {
            tracing::warn!(dir, %error, "listing a folder that kept a deleted node failed");
        }
        let other_ino = self.tree.read().by_link(&fresh.link_id);
        self.record_conflict(ConflictNote {
            other_ino,
            link: Some(fresh.link_id.clone()),
            local_rev,
            remote_rev: fresh.active_revision.clone(),
            detail,
            ..ConflictNote::new(kind, ino, orig_name)
        });
    }

    async fn upload(self: Arc<Self>, ino: Ino, target: Target, permit: UploadPermit) {
        let result = self.upload_now(ino, &target).await;
        // Before the outcome wakes the loop, so the next upload can start.
        drop(permit);
        match result {
            Ok(()) => self.journal.succeeded(ino),
            Err(UploadError::Backend(error)) => self.step_failed(ino, &error),
            Err(UploadError::Park(reason)) => {
                tracing::warn!(ino, reason, "upload parked");
                self.journal.park(ino, reason);
            }
            // The tree changed under the upload: look at the node again.
            Err(UploadError::Replan) => self.journal.replanned(ino),
        }
    }

    async fn upload_now(
        self: &Arc<Self>,
        ino: Ino,
        target: &Target,
    ) -> std::result::Result<(), UploadError> {
        if let Target::Revision { link } = target
            && let Some(fresh) = self.precheck_revision(ino, link).await?
        {
            // Only the content can tell whether the two versions differ.
            let snapshot = self.snapshot(ino).await?;
            let outcome = self.settle_with_snapshot(ino, &fresh, &snapshot).await;
            let _ = std::fs::remove_file(&snapshot.path);
            return Err(outcome);
        }
        let snapshot = self.snapshot(ino).await?;
        let result = self.upload_snapshot(ino, target, &snapshot).await;
        if result.is_err() {
            let _ = std::fs::remove_file(&snapshot.path);
        }
        result
    }

    /// The whole content of a staged file in a file of its own, as an upload
    /// takes it (§7.4), for an export (D114): its path in the staging
    /// directory, mode and mtime, or why it cannot be read.
    pub(crate) async fn snapshot_content(
        &self,
        ino: Ino,
    ) -> std::result::Result<(PathBuf, u32, SystemTime), String> {
        match self.snapshot(ino).await {
            Ok(snapshot) => Ok((snapshot.path, snapshot.mode, snapshot.mtime)),
            Err(UploadError::Backend(error)) => Err(error.to_string()),
            Err(UploadError::Park(reason)) => Err(reason),
            Err(UploadError::Replan) => Err("the file changed while it was read".into()),
        }
    }

    /// Materializes everything and snapshots the staging file (§7.4 1–3).
    async fn snapshot(&self, ino: Ino) -> std::result::Result<Snapshot, UploadError> {
        let staged = self.ensure_staged(ino).map_err(UploadError::core)?;
        // Batches until nothing is missing; a batch that makes no progress
        // (the base changed meanwhile) counts against eight tries.
        let mut stalls = 0;
        let mut previous = Vec::new();
        loop {
            let missing = staged.lock().missing_blocks(MATERIALIZE_BATCH);
            if missing.is_empty() {
                break;
            }
            if missing == previous {
                stalls += 1;
                if stalls == 8 {
                    break;
                }
            }
            match self.materialize(ino, &missing, Priority::Readahead).await {
                Ok(()) => {}
                // A resurrected file reads its old revision from the trash.
                Err(CoreError::Backend(BackendError::NotFound) | CoreError::NotFound) => {
                    return Err(UploadError::Park(
                        "content only in the emptied trash".into(),
                    ));
                }
                Err(error) => return Err(UploadError::core(error)),
            }
            previous = missing;
        }
        let (path, base_rev, gens, mtime, mode) = {
            let staged = staged.lock();
            if !staged.fully_materialized() {
                return Err(UploadError::retry("staging changed while materializing"));
            }
            let path = staged.snapshot().map_err(|e| UploadError::core(e.into()))?;
            let tree = self.tree.read();
            let node = tree
                .get(ino)
                .ok_or(UploadError::Backend(BackendError::NotFound))?;
            (
                path,
                staged.base_rev().cloned(),
                node.gens,
                node.mtime,
                node.mode,
            )
        };
        let hashed = path.clone();
        let (size, sha1) = tokio::task::spawn_blocking(move || hash_file(&hashed))
            .await
            .map_err(|e| UploadError::core(CoreError::Internal(e.to_string())))?
            .map_err(|e| UploadError::core(e.into()))?;
        Ok(Snapshot {
            path,
            content_gen: gens.content,
            meta_gen: gens.meta,
            size,
            sha1,
            mtime,
            mode,
            base_rev,
        })
    }

    async fn upload_snapshot(
        self: &Arc<Self>,
        ino: Ino,
        target: &Target,
        snapshot: &Snapshot,
    ) -> std::result::Result<(), UploadError> {
        let meta = FileMeta {
            mtime: snapshot.mtime,
            mode: Some(snapshot.mode),
            media_type: "application/octet-stream".into(),
            sha1: Some(snapshot.sha1),
        };
        let source = UploadSource::File(snapshot.path.clone());
        let remote = self.tree.read().get(ino).and_then(|n| n.remote.clone());
        let uploaded = match target {
            Target::New { parent, name } => {
                check_not_hot(&self.metrics, "upload_new");
                match self.backend.upload_new(parent, name, source, meta).await {
                    Ok((link, rev)) => Uploaded {
                        link,
                        rev,
                        parent: Some(parent.clone()),
                        name: name.clone(),
                        name_hash: None,
                    },
                    Err(BackendError::AlreadyExists { existing }) => {
                        return Err(self
                            .name_clash(ino, parent, name, existing, Some(snapshot))
                            .await);
                    }
                    Err(BackendError::NotFound) => return Err(self.parent_vanished(parent).await),
                    Err(error) => return Err(error.into()),
                }
            }
            Target::Revision { link } => {
                let remote = remote.filter(|r| &r.link == link).ok_or_else(|| {
                    UploadError::retry("the remote node changed before the upload")
                })?;
                let base = snapshot.base_rev.clone().or_else(|| remote.rev.clone());
                let unchanged = remote.sha1 == Some(snapshot.sha1)
                    && remote.size == snapshot.size
                    && remote.mtime == snapshot.mtime
                    && remote.mode.unwrap_or(DEFAULT_FILE_MODE) == snapshot.mode
                    && base == remote.rev;
                let rev = if unchanged {
                    // Nothing to upload: the remote already has it (§10.3).
                    remote.rev.clone()
                } else {
                    let base = base.ok_or(UploadError::Park(
                        "the remote file has no revision to build on".into(),
                    ))?;
                    check_not_hot(&self.metrics, "upload_revision");
                    match self
                        .backend
                        .upload_revision(link, &base, source, meta)
                        .await
                    {
                        Ok(rev) => Some(rev),
                        // Layer 3 of §10.4: the revision moved on after the
                        // pre-check, or another client's draft is open.
                        Err(BackendError::RevisionConflict { .. }) => {
                            let fresh = self.fresh_or_vanished(ino, link).await?;
                            if Some(&base) == fresh.active_revision.as_ref() {
                                return Err(UploadError::retry("another draft is open"));
                            }
                            if let Some(settled) = self.settle_cheap(ino, &fresh).await {
                                return Err(settled);
                            }
                            return Err(self.settle_with_snapshot(ino, &fresh, snapshot).await);
                        }
                        Err(BackendError::NotFound) => return Err(self.vanished(ino).await),
                        Err(error) => return Err(error.into()),
                    }
                };
                let Some(rev) = rev else {
                    return Err(UploadError::Park("the remote file has no revision".into()));
                };
                Uploaded {
                    link: link.clone(),
                    rev,
                    parent: remote.parent_link.clone(),
                    name: remote.name.clone(),
                    name_hash: remote.name_hash.clone(),
                }
            }
        };
        let revision = matches!(target, Target::Revision { .. });
        if self.commit_upload(ino, &uploaded, snapshot, revision).await {
            Ok(())
        } else {
            Err(UploadError::Replan)
        }
    }

    /// The name of a new file or folder is held remotely, by `existing` or
    /// by a node the server did not name (§10.6, D016). Always ends in
    /// `Replan` (the node is bound to the remote node, or has a new name),
    /// unless something failed. `snapshot` is the new file's content.
    async fn name_clash(
        self: &Arc<Self>,
        ino: Ino,
        parent: &LinkId,
        name: &str,
        existing: Option<LinkId>,
        snapshot: Option<&Snapshot>,
    ) -> UploadError {
        match self
            .settle_name_clash(ino, parent, name, existing, snapshot)
            .await
        {
            Ok(()) => UploadError::Replan,
            Err(error) => error,
        }
    }

    async fn settle_name_clash(
        self: &Arc<Self>,
        ino: Ino,
        parent: &LinkId,
        name: &str,
        existing: Option<LinkId>,
        snapshot: Option<&Snapshot>,
    ) -> std::result::Result<(), UploadError> {
        let retry = UploadError::retry;
        let existing = match existing {
            Some(link) => link,
            None => {
                check_not_hot(&self.metrics, "list_children");
                let children = match self.backend.list_children(parent).await {
                    Ok(children) => children,
                    Err(BackendError::NotFound) => return Err(self.parent_vanished(parent).await),
                    Err(error) => return Err(error.into()),
                };
                children
                    .into_iter()
                    .find(|node| node.name == name)
                    .ok_or_else(|| retry("the name is free again"))?
                    .link_id
            }
        };
        let fresh = self
            .fresh_live(&existing)
            .await?
            .filter(|node| node.kind == NodeKind::Folder || node.active_revision.is_some())
            .ok_or_else(|| retry("a draft holds the name"))?;

        // Another node knows the remote node already.
        let holder = self.tree.read().by_link(&existing).filter(|&z| z != ino);
        if let Some(z) = holder {
            let (attached, deleted) = {
                let tree = self.tree.read();
                let node = tree.get(z);
                (
                    node.is_some_and(|n| n.attached),
                    node.is_some_and(|n| n.deleted),
                )
            };
            if attached {
                // The next plan blocks on it.
                return Err(retry("the remote node is known under another name"));
            }
            let changes = {
                let mut tree = self.tree.write();
                if deleted {
                    tree.unbind_tombstone(z)
                } else {
                    tree.set_remote(z, None);
                    Some(Changes {
                        changed: vec![z],
                        ..Changes::default()
                    })
                }
            };
            if let Some(changes) = changes {
                self.apply_changes(changes);
            }
        }

        // Identical files and folders never conflict (D016).
        let local_kind = self.tree.read().get(ino).map(|node| node.kind);
        if local_kind == Some(fresh.kind) {
            match (fresh.kind, snapshot) {
                (NodeKind::Folder, _) => {
                    self.bind_folder(ino, RemoteState::from_remote(&fresh));
                    // Pick up children the existing folder may have.
                    let this = Arc::clone(self);
                    self.runtime().spawn(async move {
                        if let Err(error) = this.list(ino).await {
                            tracing::debug!(ino, %error, "listing a merged folder failed");
                        }
                    });
                    return Ok(());
                }
                (NodeKind::File, Some(snapshot))
                    if self.remote_matches(&fresh, snapshot).await? =>
                {
                    self.adopt_identical_file(ino, &fresh);
                    return Ok(());
                }
                _ => {}
            }
        }

        // Different things: the local node gives way (§10.6).
        self.yield_to_remote(
            ino,
            parent,
            ConflictKind::CreateCreate,
            Some(existing),
            fresh.active_revision.clone(),
        )
        .await
    }

    /// The local node `ino`, placed in the folder `parent`, loses its name
    /// to a remote node: it is renamed to its conflict name (open
    /// descriptors follow), the folder is listed so that the remote node
    /// appears at the original name, and the conflict is recorded (§10.6).
    /// A failed listing goes to the relist queue (and, after a restart,
    /// comes back through the open conflict, D055); the record is made
    /// either way.
    async fn yield_to_remote(
        self: &Arc<Self>,
        ino: Ino,
        parent: &LinkId,
        kind: ConflictKind,
        link: Option<LinkId>,
        remote_rev: Option<RevisionId>,
    ) -> std::result::Result<(), UploadError> {
        let retry = UploadError::retry;
        let dir = self
            .tree
            .read()
            .by_link(parent)
            .ok_or_else(|| retry("the parent folder is not known"))?;
        let now = self.clock()();
        let yielded = self.tree.write().yield_name(ino, now);
        let Some((changes, orig_name, conflict_name)) = yielded else {
            return Err(retry("the node is not in place"));
        };
        self.apply_changes(changes);
        // Recorded before the listing: a stop while it runs aborts this
        // step, and the open conflict is what lists the folder again at the
        // next start (D055).
        let id = self.record_conflict(ConflictNote {
            link,
            conflict_name: Some(conflict_name),
            remote_rev,
            ..ConflictNote::new(kind, ino, orig_name.clone())
        });
        // A listing that started after the name was freed (D054) shows the
        // remote node at its name.
        let listed = self.list(dir).await;
        if listed.is_err() {
            self.list_later(vec![dir]);
        }
        if let Some(other) = self.tree.read().child(dir, &orig_name).ok().flatten() {
            self.set_conflict_other(id, other);
        }
        listed.map_err(UploadError::core)
    }

    /// The parent folder of a new node is gone remotely: settle it (§10.5).
    async fn parent_vanished(self: &Arc<Self>, parent: &LinkId) -> UploadError {
        let dir = self.tree.read().by_link(parent);
        match dir {
            Some(dir) => self.vanished(dir).await,
            None => UploadError::Backend(BackendError::NotFound),
        }
    }

    /// A new file takes over an existing remote file with the same content:
    /// it builds on that revision and uploads nothing (unless the mode or
    /// mtime differ).
    fn adopt_identical_file(&self, ino: Ino, fresh: &RemoteNode) {
        let staged = self.staged_entry(ino);
        let mut staged_guard = staged.as_ref().map(|s| s.lock());
        if let Some(guard) = staged_guard.as_mut()
            && let Err(error) = guard.set_base_rev(fresh.active_revision.clone())
        {
            tracing::error!(ino, %error, "cannot record the base revision");
        }
        {
            let stamp = self.stamp();
            let mut tree = self.tree.write();
            tree.set_remote(ino, Some(RemoteState::from_remote(fresh)));
            if let Some(node) = tree.get_mut(ino) {
                node.remote_stamp = stamp;
                if !node.attached {
                    node.deleted = true;
                }
            }
        }
        drop(staged_guard);
        self.persist(&[ino]);
        tracing::info!(
            ino,
            "a remote file of that name has the same content; adopted"
        );
    }

    /// The live (not trashed) node behind `link`, fetched fresh.
    /// [`Engine::fresh_live`], or the node settled as vanished (§10.5).
    async fn fresh_or_vanished(
        self: &Arc<Self>,
        ino: Ino,
        link: &LinkId,
    ) -> std::result::Result<RemoteNode, UploadError> {
        match self.fresh_live(link).await? {
            Some(fresh) => Ok(fresh),
            None => Err(self.vanished(ino).await),
        }
    }

    async fn fresh_live(
        &self,
        link: &LinkId,
    ) -> std::result::Result<Option<RemoteNode>, UploadError> {
        check_not_hot(&self.metrics, "get_nodes");
        let nodes = self.backend.get_nodes(std::slice::from_ref(link)).await?;
        Ok(nodes
            .into_iter()
            .next()
            .flatten()
            .filter(|node| !node.trashed))
    }

    /// The remote file of `ino` is gone: settle the node (§10.5), then plan
    /// again.
    async fn vanished(self: &Arc<Self>, ino: Ino) -> UploadError {
        match self.resolve_vanished(vec![ino]).await {
            Ok(()) => UploadError::Replan,
            Err(error) => UploadError::core(error),
        }
    }

    /// Layer 2 of §10.4, before the snapshot: is the revision this upload
    /// builds on still the active one? `Ok(None)`: yes, go on. `Ok(Some)`:
    /// the file changed remotely and only the content can tell whether the
    /// two versions differ. `Err`: settled (`Replan`) or failed.
    async fn precheck_revision(
        self: &Arc<Self>,
        ino: Ino,
        link: &LinkId,
    ) -> std::result::Result<Option<RemoteNode>, UploadError> {
        let staged_base = self
            .staged_entry(ino)
            .and_then(|staged| staged.lock().base_rev().cloned());
        let known = self
            .tree
            .read()
            .get(ino)
            .and_then(|node| node.remote.clone())
            .filter(|remote| &remote.link == link)
            .ok_or_else(|| UploadError::retry("the remote node changed before the upload"))?;
        let base = staged_base.or_else(|| known.rev.clone());
        let fresh = self.fresh_or_vanished(ino, link).await?;
        if fresh.active_revision == base {
            // Refreshes the name hash for the rename pre-check.
            let refreshed = known.with_content_of(&fresh);
            if refreshed != known {
                let stamp = self.stamp();
                let mut tree = self.tree.write();
                if tree.get(ino).and_then(|node| node.link()) == Some(link) {
                    tree.set_remote_at(ino, Some(refreshed), stamp);
                    drop(tree);
                    self.persist(&[ino]);
                }
            }
            return Ok(None);
        }
        if let Some(settled) = self.settle_cheap(ino, &fresh).await {
            return Err(settled);
        }
        Ok(Some(fresh))
    }

    /// The remote file has a revision this upload is not based on: the cases
    /// that need no content comparison. `None` when the content decides.
    async fn settle_cheap(self: &Arc<Self>, ino: Ino, fresh: &RemoteNode) -> Option<UploadError> {
        let (metadata_only, local_size) = {
            let tree = self.tree.read();
            let node = tree.get(ino)?;
            (node.gens.content == node.gens.uploaded_content, node.size)
        };
        if metadata_only {
            // Not a conflict: another client's content wins, the local mode
            // and mtime stay pending (M7).
            self.readopt_remote_content(ino, fresh);
            return Some(UploadError::Replan);
        }
        if fresh.size.is_some_and(|size| size != local_size) {
            return Some(self.split_outcome(ino, fresh).await);
        }
        None
    }

    /// The remote file has a revision this upload is not based on, and the
    /// snapshot is the local content: identical content only moves the base
    /// (D016), different content is an edit/edit conflict. Always ends in
    /// `Replan`, unless something failed.
    async fn settle_with_snapshot(
        self: &Arc<Self>,
        ino: Ino,
        fresh: &RemoteNode,
        snapshot: &Snapshot,
    ) -> UploadError {
        match self.remote_matches(fresh, snapshot).await {
            Ok(true) => {
                self.adopt_base(ino, fresh);
                UploadError::Replan
            }
            Ok(false) => self.split_outcome(ino, fresh).await,
            Err(error) => error,
        }
    }

    async fn split_outcome(self: &Arc<Self>, ino: Ino, fresh: &RemoteNode) -> UploadError {
        match self.split_edit_conflict(ino, fresh).await {
            Ok(()) => UploadError::Replan,
            Err(error) => error,
        }
    }

    /// Whether the active revision of `fresh` has the snapshot's content:
    /// by the SHA-1 it claims or, when it claims none (official apps), by
    /// reading it, up to [`IDENTICAL_PROBE_MAX`] (D040).
    async fn remote_matches(
        &self,
        fresh: &RemoteNode,
        snapshot: &Snapshot,
    ) -> std::result::Result<bool, UploadError> {
        if let Some(sha1) = fresh.sha1 {
            return Ok(sha1 == snapshot.sha1);
        }
        let (Some(size), Some(rev)) = (fresh.size, fresh.active_revision.clone()) else {
            return Ok(false);
        };
        if size != snapshot.size || size > IDENTICAL_PROBE_MAX {
            return Ok(false);
        }
        if size == 0 {
            return Ok(true);
        }
        let base = Base {
            link: fresh.link_id.clone(),
            rev,
            rev_size: size,
            size,
        };
        let data = self
            .read_revision(&base, 0, size, Priority::Readahead)
            .await
            .map_err(UploadError::core)?;
        let sha1: [u8; 20] = sha1::Sha1::digest(&data[..]).into();
        Ok(sha1 == snapshot.sha1)
    }

    /// Another client's new content replaces the node's remote content: the
    /// node has no content change of its own (§10.4). The staging, if any,
    /// goes; the kernel drops what it cached.
    fn readopt_remote_content(&self, ino: Ino, fresh: &RemoteNode) {
        let staged = self.staged_entry(ino);
        let staged_guard = staged.as_ref().map(|s| s.lock());
        {
            let mut tree = self.tree.write();
            let Some(node) = tree.get(ino) else {
                return;
            };
            // A write may have landed since the caller looked.
            if node.link() != Some(&fresh.link_id)
                || node.gens.content != node.gens.uploaded_content
            {
                return;
            }
            let known = node.remote.clone().expect("linked");
            let remote = known.with_content_of(fresh);
            let node = tree.get_mut(ino).expect("checked");
            node.content = Content::Remote {
                link: fresh.link_id.clone(),
                rev: fresh.active_revision.clone(),
            };
            node.size = remote.size;
            node.content_epoch += 1;
            node.remote_newer = false;
            tree.set_remote_at(ino, Some(remote), self.stamp());
        }
        let unstaged = staged.as_ref().and_then(|staged| {
            let mut map = self.staged.lock();
            map.get(&ino)
                .is_some_and(|entry| Arc::ptr_eq(entry, staged))
                .then(|| map.remove(&ino))
                .flatten()
        });
        drop(staged_guard);
        drop(unstaged);
        self.apply_changes(Changes {
            changed: vec![ino],
            invalidations: vec![Invalidation::Inode { ino }],
            ..Changes::default()
        });
        self.remove_staging_after_commit(ino);
        tracing::info!(
            ino,
            "adopted a newer remote revision of a file with only metadata changes"
        );
    }

    /// The remote revision has the local content: the upload builds on it
    /// from now on (nothing is conflicting).
    fn adopt_base(&self, ino: Ino, fresh: &RemoteNode) {
        let staged = self.staged_entry(ino);
        let mut staged_guard = staged.as_ref().map(|s| s.lock());
        if let Some(guard) = staged_guard.as_mut()
            && let Err(error) = guard.set_base_rev(fresh.active_revision.clone())
        {
            tracing::error!(ino, %error, "cannot record the new base revision");
        }
        {
            let stamp = self.stamp();
            let mut tree = self.tree.write();
            let Some(node) = tree.get_mut(ino) else {
                return;
            };
            node.remote_newer = false;
            if let Some(known) = node.remote.clone()
                && known.link == fresh.link_id
            {
                tree.set_remote_at(ino, Some(known.with_content_of(fresh)), stamp);
            }
        }
        drop(staged_guard);
        self.persist(&[ino]);
        tracing::info!(
            ino,
            "the remote revision has the local content; building on it"
        );
    }

    /// Edit/edit (§10.5): the local file becomes the conflict copy and the
    /// server's version takes its name.
    async fn split_edit_conflict(
        self: &Arc<Self>,
        ino: Ino,
        fresh: &RemoteNode,
    ) -> std::result::Result<(), UploadError> {
        let new_ino = self
            .allocate_inos(1)
            .await
            .map_err(UploadError::core)?
            .pop()
            .ok_or_else(|| UploadError::retry("no inode number to spare"))?;
        let staged = self.staged_entry(ino);
        let mut staged_guard = staged.as_ref().map(|s| s.lock());
        let split = self.tree.write().split_conflict(
            ino,
            RemoteState::from_remote(fresh),
            new_ino,
            self.clock()(),
        );
        let Some((changes, note)) = split else {
            return Ok(());
        };
        if let Some(guard) = staged_guard.as_mut()
            && let Err(error) = guard.set_base_rev(None)
        {
            tracing::error!(ino, %error, "cannot clear the base revision");
        }
        drop(staged_guard);
        self.apply_changes(changes);
        self.record_conflict(note);
        Ok(())
    }

    /// The synchronous part of [`Self::commit_upload`], under the staging
    /// and tree locks: whether the node is clean, or `Err` with what
    /// `commit_upload` returns at once.
    fn record_upload(
        &self,
        ino: Ino,
        uploaded: &Uploaded,
        snapshot: &Snapshot,
        revision: bool,
    ) -> std::result::Result<bool, bool> {
        let staged = self.staged_entry(ino);
        let mut staged_guard = staged.as_ref().map(|s| s.lock());
        let stamp = self.stamp();
        let clean = {
            let mut tree = self.tree.write();
            if revision
                && tree
                    .get(ino)
                    .is_some_and(|node| node.link() != Some(&uploaded.link))
            {
                tracing::info!(
                    ino,
                    link = %uploaded.link,
                    "the node left the remote file during its upload; not recorded"
                );
                drop(tree);
                drop(staged_guard);
                let _ = std::fs::remove_file(&snapshot.path);
                return Err(false);
            }
            tree.set_remote(
                ino,
                Some(RemoteState {
                    link: uploaded.link.clone(),
                    parent_link: uploaded.parent.clone(),
                    name: uploaded.name.clone(),
                    rev: Some(uploaded.rev.clone()),
                    size: snapshot.size,
                    mtime: snapshot.mtime,
                    mode: Some(snapshot.mode),
                    sha1: Some(snapshot.sha1),
                    name_hash: uploaded.name_hash.clone(),
                    unverified: None,
                }),
            );
            let Some(node) = tree.get_mut(ino) else {
                return Err(true);
            };
            node.remote_stamp = stamp;
            node.remote_newer = false;
            node.gens.uploaded_content = node.gens.uploaded_content.max(snapshot.content_gen);
            node.gens.uploaded_meta = node.gens.uploaded_meta.max(snapshot.meta_gen);
            if !node.attached {
                // Unlinked while uploading: trash what was uploaded.
                node.deleted = true;
            }
            let clean = node.gens.content == snapshot.content_gen;
            if clean {
                node.content = Content::Remote {
                    link: uploaded.link.clone(),
                    rev: Some(uploaded.rev.clone()),
                };
            }
            clean
        };
        // Under the staging lock, so no write lands in between (see
        // `Engine::with_staged`). A staging left behind by a crash builds on
        // the uploaded revision.
        if let Some(guard) = staged_guard.as_mut()
            && let Err(error) = guard.set_base_rev(Some(uploaded.rev.clone()))
        {
            tracing::error!(ino, %error, "cannot record the new base revision");
        }
        let unstaged = if clean && let Some(staged) = &staged {
            let mut map = self.staged.lock();
            if map
                .get(&ino)
                .is_some_and(|entry| Arc::ptr_eq(entry, staged))
            {
                map.remove(&ino)
            } else {
                None
            }
        } else {
            None
        };
        drop(staged_guard);
        // Closed outside the map lock (see `Engine::unstage`).
        drop(unstaged);
        Ok(clean)
    }

    /// Records an upload (§7.4 7–8): the node is clean only if nothing
    /// changed since the snapshot. A clean node's staging goes once the row
    /// naming the uploaded revision is durable.
    ///
    /// `false` when the node is bound to another remote node by now (a split
    /// or a resurrection while the revision went up): nothing is recorded,
    /// and the caller plans again.
    async fn commit_upload(
        &self,
        ino: Ino,
        uploaded: &Uploaded,
        snapshot: &Snapshot,
        revision: bool,
    ) -> bool {
        let clean = match self.record_upload(ino, uploaded, snapshot, revision) {
            Ok(clean) => clean,
            Err(done) => return done,
        };
        if let Err(error) = self
            .cache
            .seed(
                &uploaded.link,
                &uploaded.rev,
                &snapshot.path,
                self.backend.upload_block_size(),
            )
            .await
        {
            tracing::debug!(%error, "could not seed the cache from the snapshot");
        }
        let _ = std::fs::remove_file(&snapshot.path);
        let (link, rev) = (uploaded.link.0.clone(), uploaded.rev.0.clone());
        self.store.write(move |conn| {
            conn.execute(
                "INSERT OR IGNORE INTO own_revisions(link_id, rev, created_at) VALUES (?1, ?2, strftime('%s','now'))",
                (&link, &rev),
            )
            .map(drop)
        });
        self.persist(&[ino]);
        Metrics::inc(&self.metrics.uploads);
        if clean {
            self.remove_staging_after_commit(ino);
        }
        self.pins_changed();
        true
    }

    /// Pending entries with what the planner says about them (tests).
    #[cfg(test)]
    pub(crate) fn describe_pending(&self) -> Vec<String> {
        let now = Instant::now();
        let entries: Vec<(Ino, Entry)> = {
            let state = self.journal.state.lock();
            state.entries.iter().map(|(i, e)| (*i, e.clone())).collect()
        };
        entries
            .into_iter()
            .map(|(ino, entry)| {
                let timing = Timing {
                    changed_at: entry.changed_at,
                    dirty_since: entry.dirty_since,
                    forced: true,
                    generation: entry.generation,
                };
                let plan = self.plan(ino, timing, now);
                let tree = self.tree.read();
                let node = tree.get(ino).map(|n| {
                    format!(
                        "attached {} deleted {} local_only {} remote {:?} parent {} name {:?} content_pending {} meta_pending {}",
                        n.attached,
                        n.deleted,
                        n.local_only,
                        n.remote.as_ref().map(|r| (&r.link, &r.parent_link, &r.name)),
                        n.parent,
                        n.name,
                        n.content_pending(),
                        n.meta_pending()
                    )
                });
                format!(
                    "ino {ino} {:?}: plan {plan:?}; in_flight {} blocked_on {:?} parked {:?} attempts {} last_error {:?}; node {node:?}",
                    tree.path(ino),
                    entry.in_flight,
                    entry.blocked_on,
                    entry.parked,
                    entry.attempts,
                    entry.last_error
                )
            })
            .collect()
    }

    /// Queue counts for `status`.
    pub fn queue_counts(&self) -> QueueCounts {
        self.journal.counts()
    }

    /// Gauges for `stats` (§5.8), by name.
    pub fn gauges(&self) -> Vec<(&'static str, u64)> {
        let counts = self.journal.counts();
        let oldest = self.journal.oldest_age().unwrap_or_default();
        vec![
            ("queue_pending", counts.pending as u64),
            ("queue_parked", counts.parked as u64),
            ("queue_oldest_secs", oldest.as_secs()),
            ("staged_open_files", self.staging.open_files() as u64),
        ]
    }

    /// How long the oldest entry that is not parked has been dirty (`status`).
    pub fn queue_oldest(&self) -> Option<Duration> {
        self.journal.oldest_age()
    }

    /// Parked entries with their paths and reasons.
    pub fn parked(&self) -> Vec<(String, String)> {
        let tree = self.tree.read();
        self.journal
            .parked()
            .into_iter()
            .map(|(ino, reason)| {
                (
                    tree.path(ino).unwrap_or_else(|| format!("(inode {ino})")),
                    reason,
                )
            })
            .collect()
    }

    /// Uploads everything now and waits until nothing but parked entries
    /// is left, or `timeout` passes (`unmount --wait`, `sync`).
    pub async fn drain(&self, timeout: Duration) -> QueueCounts {
        let deadline = Instant::now() + timeout;
        self.journal.force_all(true);
        let counts = loop {
            let progress = self.journal.progress.notified();
            let counts = self.journal.counts();
            if counts.pending == 0 || Instant::now() >= deadline {
                break counts;
            }
            tokio::select! {
                () = progress => {}
                () = tokio::time::sleep(Duration::from_millis(50)) => {}
            }
        };
        self.journal.force_all(false);
        counts
    }

    /// What keeps `ino` from its next step (`queue show`).
    pub(crate) fn pending_dependency(&self, ino: Ino) -> Option<Ino> {
        let now = Instant::now();
        let timing = Timing {
            changed_at: now,
            dirty_since: now,
            forced: true,
            generation: 0,
        };
        match self.plan(ino, timing, now) {
            Plan::Blocked(on) => Some(on),
            _ => None,
        }
    }

    /// Forces the entries `selected` gives each round and waits until none
    /// can finish any more, or `timeout` passes (`sync`). Entries that come
    /// up meanwhile count too.
    pub(crate) async fn sync_entries(
        &self,
        selected: impl Fn() -> Vec<Ino>,
        timeout: Duration,
    ) -> SyncRound {
        let deadline = Instant::now() + timeout;
        loop {
            let round = self.journal.sync_round(&selected());
            if round.moving == 0 || Instant::now() >= deadline {
                return round;
            }
            tokio::time::sleep(SYNC_POLL).await;
        }
    }

    /// The entries a shutdown flush waits for: those queued now and not
    /// parked (D075). Entries queued later do not extend the wait.
    pub fn flush_barrier(&self) -> FlushBarrier {
        FlushBarrier {
            inos: self.journal.unparked(),
        }
    }

    /// Of the barrier's entries, how many are still queued and not parked,
    /// and how many are parked now (M5.2: the start's watch).
    pub fn barrier_queued(&self, barrier: &FlushBarrier) -> (usize, usize) {
        self.journal.queued_among(&barrier.inos)
    }

    /// Uploads the barrier's entries without the debounce and waits until
    /// none of them can finish any more, the connection is lost, or
    /// `deadline` passes (D075, D091). New writes are still accepted and
    /// uploaded as usual; back-offs and rate limits are kept.
    pub async fn flush_to(&self, barrier: &FlushBarrier, deadline: Instant) -> FlushOutcome {
        let start = Instant::now();
        let ended = loop {
            let state = self.health.state();
            if state != crate::health::Connectivity::Online {
                break FlushEnd::NotOnline(state);
            }
            let progress = self.journal.progress.notified();
            let round = self.journal.flush_round(&barrier.inos);
            if round.moving == 0 {
                break FlushEnd::Reached;
            }
            let now = Instant::now();
            if now >= deadline {
                break FlushEnd::Deadline;
            }
            tokio::select! {
                () = progress => {}
                () = tokio::time::sleep(SYNC_POLL.min(deadline - now)) => {}
            }
        };
        FlushOutcome {
            barrier: barrier.inos.len(),
            left: self.journal.still_queued(&barrier.inos),
            elapsed: start.elapsed(),
            ended,
        }
    }

    /// `fsync` in `remote` mode: waits until the content at call time is
    /// uploaded.
    pub async fn sync_remote(&self, ino: Ino, timeout: Duration) -> Result<()> {
        let target = self
            .tree
            .read()
            .get(ino)
            .map(|n| n.gens)
            .ok_or(CoreError::NotFound)?;
        self.journal.force(ino);
        let deadline = Instant::now() + timeout;
        loop {
            let (gens, parked) = {
                let tree = self.tree.read();
                let gens = tree.get(ino).map(|n| n.gens);
                (
                    gens,
                    self.journal.parked().into_iter().any(|(i, _)| i == ino),
                )
            };
            let Some(gens) = gens else {
                return Ok(());
            };
            if gens.uploaded_content >= target.content && gens.uploaded_meta >= target.meta {
                return Ok(());
            }
            if parked {
                return Err(CoreError::Errno(nix::libc::EIO));
            }
            if Instant::now() >= deadline {
                return Err(CoreError::Timeout);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            self.journal.wake();
        }
    }
}

struct Uploaded {
    link: LinkId,
    rev: RevisionId,
    parent: Option<LinkId>,
    name: String,
    name_hash: Option<NameHash>,
}

enum UploadError {
    Backend(BackendError),
    Park(String),
    /// The node changed under the upload (a conflict was settled, the
    /// remote file moved on): nothing failed, look at the node again.
    Replan,
}

impl UploadError {
    /// A temporary failure: the step is planned again after a backoff.
    fn retry(why: &str) -> Self {
        Self::Backend(BackendError::Transient(why.to_owned()))
    }

    fn core(error: CoreError) -> Self {
        match error {
            CoreError::Backend(error) => Self::Backend(error),
            other => Self::Backend(BackendError::Transient(other.to_string())),
        }
    }
}

impl From<BackendError> for UploadError {
    fn from(error: BackendError) -> Self {
        Self::Backend(error)
    }
}

fn hash_file(path: &std::path::Path) -> std::io::Result<(u64, [u8; 20])> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = sha1::Sha1::new();
    // On the stack: a heap buffer per upload is churn that glibc keeps (D049).
    let mut buffer = [0u8; 64 << 10];
    let mut size = 0u64;
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
        size += n as u64;
    }
    Ok((size, hasher.finalize().into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The session's upload permits of a journal alone.
    fn uploads() -> Arc<tokio::sync::Semaphore> {
        Arc::new(tokio::sync::Semaphore::new(
            UploadConfig::default().concurrency as usize,
        ))
    }

    fn not_before(journal: &Journal, ino: Ino) -> Instant {
        journal.state.lock().entries[&ino].not_before
    }

    fn stored_times(store: &Store) -> Vec<(Ino, i64)> {
        store
            .read(|conn| {
                let mut select =
                    conn.prepare("SELECT ino, queued_at_ms FROM queued ORDER BY ino")?;
                select
                    .query_map([], |row| Ok((row.get::<_, i64>(0)? as Ino, row.get(1)?)))?
                    .collect()
            })
            .expect("read")
    }

    #[test]
    fn a_carried_age_is_never_negative_nor_absurd() {
        assert_eq!(carried_age(300_000), Duration::from_secs(300));
        assert_eq!(carried_age(0), Duration::ZERO);
        assert_eq!(carried_age(-5_000), Duration::ZERO, "the clock went back");
        assert_eq!(carried_age(i64::MAX), MAX_CARRIED_AGE, "the clock jumped");
        assert_eq!(carried_age(i64::MIN), Duration::ZERO);
    }

    /// An entry restored from the last session counts from when it was
    /// queued; a new one counts from now (D105).
    #[tokio::test]
    async fn a_restored_entry_keeps_its_age() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Arc::new(Store::open(&dir.path().join("state.db")).expect("store"));
        let now_ms = unix_ms(SystemTime::now());
        let journal = Journal::with_store(&UploadConfig::default(), Arc::clone(&store), uploads());
        journal.restore(&[(1, now_ms - 300_000), (3, now_ms + 60_000)], now_ms);
        journal.touch(1);
        journal.touch(2);
        journal.touch(3);
        let views: BTreeMap<Ino, Duration> = journal
            .entries()
            .into_iter()
            .map(|view| (view.ino, view.age))
            .collect();
        assert!(views[&1] >= Duration::from_secs(300), "{:?}", views[&1]);
        assert!(views[&1] < Duration::from_secs(310));
        assert!(views[&2] < Duration::from_secs(5), "queued now");
        assert!(views[&3] < Duration::from_secs(5), "a time in the future");
        let oldest = journal.oldest_age().expect("oldest");
        assert!(oldest >= Duration::from_secs(300), "{oldest:?}");
    }

    /// Entries write their queue time once, at creation, and delete it when
    /// they leave; restored ones keep the stored time; unused rows go.
    #[tokio::test]
    async fn queue_times_follow_the_entries() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Arc::new(Store::open(&dir.path().join("state.db")).expect("store"));
        let journal = Journal::with_store(&UploadConfig::default(), Arc::clone(&store), uploads());
        journal.touch(7);
        journal.touch_content(7);
        store.flush().await.expect("flush");
        let rows = stored_times(&store);
        assert_eq!(rows.len(), 1, "one row per entry");
        assert_eq!(rows[0].0, 7);
        journal.touch_content(7);
        store.flush().await.expect("flush");
        assert_eq!(stored_times(&store), rows, "a later change keeps it");
        let generation = journal.state.lock().entries[&7].generation;
        journal.done(7, generation);
        store.flush().await.expect("flush");
        assert!(stored_times(&store).is_empty(), "the entry left");

        journal.touch(8);
        journal.touch(9);
        journal.forget(9);
        store.flush().await.expect("flush");
        assert_eq!(stored_times(&store).len(), 1, "forgotten: no row");

        // A restart: 8 gets an entry again, 5 has none.
        store.write(|conn| {
            conn.execute("INSERT INTO queued VALUES (5, 1000)", [])
                .map(drop)
        });
        store.flush().await.expect("flush");
        let before = stored_times(&store);
        let again = Journal::with_store(&UploadConfig::default(), Arc::clone(&store), uploads());
        again.restore(&before, unix_ms(SystemTime::now()));
        again.touch(8);
        again.finish_restore();
        store.flush().await.expect("flush");
        let rows = stored_times(&store);
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].0, 8);
        assert_eq!(rows[0].1, before.iter().find(|r| r.0 == 8).expect("8").1);
    }

    /// The shutdown flush (D075) starts an entry that waits for its
    /// debounce at once, but one in its back-off (HTTP 429 here) only when
    /// the back-off ends; `sync` hurries both.
    #[test]
    fn a_flush_round_skips_the_debounce_but_keeps_a_back_off() {
        for hurry in [false, true] {
            let journal = Journal::new(&UploadConfig::default());
            let now = Instant::now();
            journal.touch_content(1);
            journal.defer(1, now + Duration::from_secs(60), 1);
            journal.touch_content(2);
            assert!(journal.start(2));
            journal.failed(
                2,
                &BackendError::RateLimited {
                    retry_after: Some(Duration::from_secs(30)),
                },
            );
            let round = if hurry {
                journal.sync_round(&[1, 2])
            } else {
                journal.flush_round(&[1, 2])
            };
            assert_eq!((round.pending, round.moving), (2, 2));
            assert!(not_before(&journal, 1) <= Instant::now(), "the debounce");
            let ready: Vec<Ino> = journal
                .ready(Instant::now())
                .0
                .iter()
                .map(|r| r.0)
                .collect();
            if hurry {
                assert_eq!(ready, [1, 2]);
            } else {
                assert_eq!(ready, [1]);
                assert!(not_before(&journal, 2) >= now + Duration::from_secs(29));
            }
        }
    }

    /// A change after the failure ends its back-off for the flush too.
    #[test]
    fn a_change_ends_the_back_off_for_a_flush() {
        let journal = Journal::new(&UploadConfig::default());
        journal.touch_content(1);
        assert!(journal.start(1));
        journal.failed(
            1,
            &BackendError::RateLimited {
                retry_after: Some(Duration::from_secs(30)),
            },
        );
        journal.touch_content(1);
        journal.defer(1, Instant::now() + Duration::from_secs(60), 2);
        journal.flush_round(&[1]);
        assert!(not_before(&journal, 1) <= Instant::now());
    }

    /// A folder's trash that waits for a child's entry (D116) is planned
    /// again as soon as that entry finishes a step or leaves, however often
    /// it waited; the child gets an entry if it had none. (One made by the
    /// wait alone that leaves at once wakes nothing:
    /// `a_dependency_that_leaves_at_once_does_not_wake_its_waiter`.)
    #[test]
    fn a_wait_ends_with_the_entry_it_waits_for() {
        for finish in ["succeeded", "done", "forget"] {
            let journal = Journal::new(&UploadConfig::default());
            journal.touch(1);
            if finish == "done" {
                // The child's own change, which it then had done.
                journal.touch(2);
            }
            for _ in 0..10 {
                assert!(journal.start(1));
                journal.wait_on(1, 2, "waiting for the folders deleted inside it");
            }
            assert!(not_before(&journal, 1) > Instant::now() + Duration::from_secs(60));
            let view = journal.entries().into_iter().find(|e| e.ino == 1);
            let view = view.expect("entry");
            assert_eq!(view.blocked_on, Some(2));
            assert_eq!(
                view.last_error.as_deref(),
                Some("waiting for the folders deleted inside it")
            );
            assert!(journal.inos().contains(&2), "the child is scheduled");
            match finish {
                "succeeded" => {
                    assert!(journal.start(2));
                    journal.succeeded(2);
                }
                "done" => journal.done(2, 1),
                _ => journal.forget(2),
            }
            assert!(not_before(&journal, 1) <= Instant::now(), "{finish}");
        }
    }

    /// A dependency that exists only because something waits for it, and
    /// leaves at its first look without a step (a node in a local-only
    /// folder: nothing to do, ever), does not wake what waits for it. Each
    /// such wake made a folder's trash ask the server again, about once a
    /// second, forever (D121).
    #[test]
    fn a_dependency_that_leaves_at_once_does_not_wake_its_waiter() {
        // A folder's trash waits (D116).
        let journal = Journal::new(&UploadConfig::default());
        journal.touch(1);
        assert!(journal.start(1));
        journal.wait_on(1, 2, "waiting for moves out of the folder");
        let after_wait = not_before(&journal, 1);
        assert!(after_wait > Instant::now());
        journal.done(2, 0);
        assert!(!journal.inos().contains(&2), "the dependency left");
        assert_eq!(not_before(&journal, 1), after_wait, "not woken");
        // A node blocked on the remote occupant of its name.
        let journal = Journal::new(&UploadConfig::default());
        journal.touch(1);
        journal.block(1, 2, 1);
        let after_block = not_before(&journal, 1);
        assert!(after_block > Instant::now());
        journal.done(2, 0);
        assert_eq!(not_before(&journal, 1), after_block, "not woken");
    }

    /// Waits are not failed attempts (D121): after many waits the first
    /// permanent error does not park the entry at once, and its first
    /// transient one backs off from the start.
    #[test]
    fn waits_are_not_failed_attempts() {
        let journal = Journal::new(&UploadConfig::default());
        journal.touch(1);
        for _ in 0..=PARK_AFTER {
            assert!(journal.start(1));
            journal.wait_on(1, 2, "waiting for moves out of the folder");
        }
        assert!(journal.start(1));
        assert!(
            !journal.failed(1, &BackendError::Other("refused".into())),
            "the first permanent error does not park"
        );
        assert!(journal.parked().is_empty());
        assert!(journal.start(1));
        journal.failed(1, &BackendError::Transient("timeout".into()));
        assert!(
            not_before(&journal, 1) < Instant::now() + Duration::from_secs(5),
            "the second failure backs off about 2 s"
        );
    }

    /// A change, `retry` or force made while the check ran is not undone by
    /// the wait's back-off (D121).
    #[test]
    fn a_wait_keeps_a_change_made_while_the_check_ran() {
        for meanwhile in ["change", "retry", "force"] {
            let journal = Journal::new(&UploadConfig::default());
            journal.touch(1);
            assert!(journal.start(1));
            match meanwhile {
                "change" => journal.touch(1),
                "retry" => journal.retry(&[1]),
                _ => journal.force(1),
            }
            journal.wait_on(1, 2, "waiting for moves out of the folder");
            assert!(not_before(&journal, 1) <= Instant::now(), "{meanwhile}");
        }
    }

    const HOLD: &str = "no write access";

    /// Entries made after a tree's hold (D114) are held too: a write that
    /// passed the gate just before it closed, and a dependency its plan
    /// finds. A step that ran at the hold and found a wait starts no other
    /// step either. Nothing starts until the hold is lifted (D121).
    #[test]
    fn entries_made_after_a_hold_are_held() {
        let journal = Journal::new(&UploadConfig::default());
        journal.touch(1);
        assert!(journal.start(1));
        journal.hold_all(HOLD);
        journal.wait_on(1, 2, "waiting for the folders deleted inside it");
        journal.touch(3);
        journal.block(3, 4, 1);
        for ino in [1, 2, 3, 4] {
            assert!(!journal.start(ino), "{ino} is held");
        }
        let held: Vec<Ino> = journal
            .parked()
            .into_iter()
            .filter(|(_, reason)| reason == HOLD)
            .map(|(ino, _)| ino)
            .collect();
        assert_eq!(held, [1, 3, 4]);
        assert_eq!(journal.release_held(HOLD), 3);
        for ino in [1, 3, 4] {
            assert!(journal.start(ino), "{ino} is released");
        }
    }

    /// A hold keeps the reason of an entry parked for something else, and
    /// lifting it does not retry that entry (D121).
    #[test]
    fn a_hold_keeps_other_park_reasons() {
        let journal = Journal::new(&UploadConfig::default());
        journal.touch(1);
        assert!(journal.start(1));
        assert!(journal.failed(1, &BackendError::QuotaExceeded));
        journal.touch(2);
        assert_eq!(journal.hold_all(HOLD), 2);
        let quota = BackendError::QuotaExceeded.to_string();
        assert_eq!(journal.parked(), [(1, quota.clone()), (2, HOLD.to_owned())]);
        // A local change lifts the automatic park, not the hold.
        journal.touch_content(1);
        assert_eq!(
            journal.parked(),
            [(1, HOLD.to_owned()), (2, HOLD.to_owned())]
        );
        assert_eq!(journal.release_held(HOLD), 2);
        assert!(journal.parked().is_empty());

        // Without a change in between, the quota park outlives the hold.
        let journal = Journal::new(&UploadConfig::default());
        journal.touch(1);
        assert!(journal.start(1));
        assert!(journal.failed(1, &BackendError::QuotaExceeded));
        journal.hold_all(HOLD);
        assert_eq!(journal.release_held(HOLD), 0);
        assert_eq!(journal.parked(), [(1, quota)]);
    }

    /// An entry whose step was running at the hold is not shown as held
    /// once that step succeeded: it is planned again, and held only if it
    /// needs another step (D121).
    #[test]
    fn an_entry_running_at_a_hold_is_not_held_after_its_step_succeeded() {
        let journal = Journal::new(&UploadConfig::default());
        journal.touch(1);
        assert!(journal.start(1));
        journal.hold_all(HOLD);
        journal.succeeded(1);
        assert!(journal.parked().is_empty(), "{:?}", journal.parked());
        let ready: Vec<Ino> = journal
            .ready(Instant::now())
            .0
            .iter()
            .map(|r| r.0)
            .collect();
        assert_eq!(ready, [1], "planned again");
        assert!(!journal.start(1), "another step waits for the hold");
        assert_eq!(journal.parked(), [(1, HOLD.to_owned())]);
    }

    /// Parked entries are not part of a barrier.
    #[test]
    fn parked_entries_are_left_out_of_a_barrier() {
        let journal = Journal::new(&UploadConfig::default());
        journal.touch(1);
        journal.touch(2);
        journal.park_by_user(&[2]);
        assert_eq!(journal.unparked(), [1]);
        assert_eq!(journal.still_queued(&[1, 2, 3]), 2);
    }
}
