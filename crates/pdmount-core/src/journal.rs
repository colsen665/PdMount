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
//!   through a temporary `.pdmount-swap-<ino>` name.
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

use crate::cache::Priority;
use crate::config::{MetaRevisionPolicy, UploadConfig};
use crate::conflicts::{ConflictKind, ConflictNote};
use crate::engine::Engine;
use crate::errors::{CoreError, Result};
use crate::lock_order::{self, Rank};
use crate::metrics::{Metrics, check_not_hot};
use crate::staging::Base;
use crate::vfs::{Changes, Content, DEFAULT_FILE_MODE, Ino, Invalidation, ROOT_INO, RemoteState};
use pdmount_backend::{
    BackendError, FileMeta, LinkId, NameHash, NodeKind, RemoteNode, RevisionId, UploadSource,
};
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
/// Largest file whose remote content is read to compare it with a local one
/// when the remote revision claims no SHA-1 (D040).
const IDENTICAL_PROBE_MAX: u64 = 16 << 20;
const MAX_WAKE: Duration = Duration::from_secs(1);

#[derive(Debug, Clone)]
struct Entry {
    not_before: Instant,
    attempts: u32,
    parked: Option<String>,
    in_flight: bool,
    /// Last local change, for the debounce.
    changed_at: Instant,
    /// When the inode became dirty, for `max_dirty_age`.
    dirty_since: Instant,
    /// Upload as soon as possible (`sync`, unmount, `fsync` in remote mode).
    forced: bool,
    blocked_on: Option<Ino>,
    /// The last failure, for `status` and diagnostics.
    last_error: Option<String>,
    /// Steps in a row that ended in `Replan`.
    replans: u32,
    /// Bumped by every change and force: a plan made from an older
    /// generation must not defer, block or remove the entry.
    generation: u64,
}

impl Entry {
    fn new(now: Instant) -> Self {
        Self {
            not_before: now,
            attempts: 0,
            parked: None,
            in_flight: false,
            changed_at: now,
            dirty_since: now,
            forced: false,
            blocked_on: None,
            last_error: None,
            replans: 0,
            generation: 0,
        }
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
}

pub(crate) struct Journal {
    state: lock_order::Mutex<State>,
    wake: tokio::sync::Notify,
    /// Directories to list again (see `Node::clashed`).
    listings: parking_lot::Mutex<Vec<Ino>>,
    /// Signalled whenever an entry finishes, fails or leaves.
    progress: tokio::sync::Notify,
    /// One permit per running upload (`upload.concurrency`).
    uploads: Arc<tokio::sync::Semaphore>,
    debounce: Duration,
    max_dirty_age: Duration,
    meta_revision: MetaRevisionPolicy,
}

#[derive(Debug, Clone, Copy)]
struct Timing {
    changed_at: Instant,
    dirty_since: Instant,
    forced: bool,
    generation: u64,
}

impl Journal {
    pub(crate) fn new(config: &UploadConfig) -> Self {
        Self {
            state: lock_order::Mutex::new(Rank::Leaf, State::default()),
            wake: tokio::sync::Notify::new(),
            listings: parking_lot::Mutex::default(),
            progress: tokio::sync::Notify::new(),
            uploads: Arc::new(tokio::sync::Semaphore::new(
                config.concurrency.max(1) as usize
            )),
            debounce: config.debounce.0,
            max_dirty_age: config.max_dirty_age.0,
            meta_revision: config.meta_revision,
        }
    }

    fn entry(state: &mut State, ino: Ino, now: Instant) -> &mut Entry {
        let draining = state.draining;
        let entry = state.entries.entry(ino).or_insert_with(|| Entry::new(now));
        entry.forced |= draining;
        entry
    }

    /// Asks for `dirs` to be listed again; `run_journal` does it.
    pub(crate) fn list_later(&self, dirs: Vec<Ino>) {
        self.listings.lock().extend(dirs);
        self.wake.notify_one();
    }

    /// A namespace change: re-plan at once.
    pub(crate) fn touch(&self, ino: Ino) {
        let now = Instant::now();
        {
            let mut state = self.state.lock();
            let entry = Self::entry(&mut state, ino, now);
            entry.parked = None;
            entry.attempts = 0;
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
            let entry = Self::entry(&mut state, ino, now);
            entry.parked = None;
            entry.attempts = 0;
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
            let entry = Self::entry(&mut state, ino, now);
            entry.forced = true;
            entry.not_before = now;
            entry.generation += 1;
        }
        self.wake.notify_one();
    }

    fn ready(&self, now: Instant) -> (Vec<(Ino, Timing)>, Option<Instant>) {
        let state = self.state.lock();
        let mut ready = Vec::new();
        let mut next: Option<Instant> = None;
        for (&ino, entry) in &state.entries {
            if entry.in_flight || entry.parked.is_some() {
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
    fn done(&self, ino: Ino, generation: u64) {
        {
            let mut state = self.state.lock();
            if state
                .entries
                .get(&ino)
                .is_some_and(|e| !e.in_flight && e.generation == generation)
            {
                state.entries.remove(&ino);
            }
            Self::unblock(&mut state, ino);
        }
        self.progress.notify_waiters();
    }

    fn defer(&self, ino: Ino, until: Instant, generation: u64) {
        if let Some(entry) = self.state.lock().entries.get_mut(&ino)
            && entry.generation == generation
        {
            entry.not_before = until;
        }
    }

    fn block(&self, ino: Ino, on: Ino, generation: u64) {
        let now = Instant::now();
        let mut state = self.state.lock();
        if let Some(entry) = state.entries.get_mut(&ino)
            && entry.generation == generation
        {
            entry.blocked_on = Some(on);
            entry.not_before = now + BLOCKED_RECHECK;
        }
        // The dependency must be scheduled itself.
        let draining = state.draining;
        let dep = state.entries.entry(on).or_insert_with(|| Entry::new(now));
        dep.forced |= draining;
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

    fn start(&self, ino: Ino) {
        if let Some(entry) = self.state.lock().entries.get_mut(&ino) {
            entry.in_flight = true;
        }
    }

    /// Starts an upload if fewer than `upload.concurrency` run; it runs
    /// while the permit is held.
    fn try_start_upload(&self, ino: Ino) -> Option<tokio::sync::OwnedSemaphorePermit> {
        let permit = Arc::clone(&self.uploads).try_acquire_owned().ok()?;
        let mut state = self.state.lock();
        state.entries.get_mut(&ino)?.in_flight = true;
        Some(permit)
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
                entry.parked = Some(error.to_string());
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
                false
            }
        };
        self.wake.notify_one();
        self.progress.notify_waiters();
        parked
    }

    fn park(&self, ino: Ino, reason: String) {
        let mut state = self.state.lock();
        if let Some(entry) = state.entries.get_mut(&ino) {
            entry.in_flight = false;
            entry.parked = Some(reason);
        }
        drop(state);
        self.progress.notify_waiters();
    }

    pub(crate) fn wake(&self) {
        self.wake.notify_one();
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

fn backoff(attempts: u32) -> Duration {
    let base = BACKOFF_MIN.saturating_mul(1 << attempts.min(9).saturating_sub(1));
    let jitter = rand::random::<f64>() * 0.3 + 0.85;
    base.mul_f64(jitter).min(BACKOFF_MAX)
}

/// Errors retrying cannot fix soon. `None`: keep retrying.
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
            let relist = std::mem::take(&mut *self.journal.listings.lock());
            for dir in relist {
                let this = Arc::clone(&self);
                self.runtime().spawn(async move {
                    if let Err(error) = this.list(dir).await {
                        tracing::debug!(dir, %error, "listing after a freed name failed");
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
                        self.journal.start(ino);
                        trash.push((ino, link));
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
                        self.journal.start(ino);
                        self.execute(ino, work).await;
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
        if tree
            .get(node.parent)
            .is_some_and(|parent| parent.local_only && parent.remote.is_none())
        {
            // Inside a local-only folder (§7.7).
            return Plan::Done;
        }
        let Some(parent) = tree.link_of(node.parent).cloned() else {
            return Plan::Blocked(node.parent);
        };
        // The target name must be free remotely, or held by this node.
        let reserve = |plan: Plan| -> Plan {
            match tree.remote_occupant(&parent, &node.name) {
                Some(occupant) if occupant != ino => {
                    let swap = node.remote.is_some()
                        && tree.get(occupant).is_some_and(|other| {
                            other.attached
                                && tree.link_of(other.parent).is_some_and(|other_parent| {
                                    node.remote.as_ref().and_then(|r| r.parent_link.as_ref())
                                        == Some(other_parent)
                                        && node.remote.as_ref().map(|r| r.name.as_str())
                                            == Some(other.name.as_str())
                                })
                        });
                    match (&node.remote, swap) {
                        (Some(remote), true) => Plan::Work(Work::Move {
                            link: remote.link.clone(),
                            new_parent: None,
                            name: format!(".pdmount-swap-{ino}"),
                            expected: remote.name_hash.clone(),
                        }),
                        _ => Plan::Blocked(occupant),
                    }
                }
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
            // The remote committed first.
            return Err(self.remote_moved_first(ino, &fresh, target.as_ref(), &name));
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
                return Err(self.remote_moved_first(ino, &fresh, target.as_ref(), &name));
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
            match self.trash_check(*ino, &node).await {
                Ok(true) => send.push((*ino, link.clone())),
                Ok(false) => {}
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

    /// Whether the tombstone `ino` may be trashed: `false` when the remote
    /// node has something the local delete did not know about, in which case
    /// the node is kept and shown again.
    async fn trash_check(
        self: &Arc<Self>,
        ino: Ino,
        fresh: &RemoteNode,
    ) -> std::result::Result<bool, BackendError> {
        let Some((base, remote_newer)) = self.base_of(ino) else {
            return Ok(true);
        };
        let (kind, detail, local_rev) = match fresh.kind {
            NodeKind::File => {
                if fresh.active_revision == base && !remote_newer {
                    return Ok(true);
                }
                (ConflictKind::LocalDeleteRemoteEdit, None, base)
            }
            NodeKind::Folder => {
                check_not_hot(&self.metrics, "list_children");
                let kids = match self.backend.list_children(&fresh.link_id).await {
                    Ok(kids) => kids,
                    // The folder is gone; the trash call says so.
                    Err(BackendError::NotFound) => return Ok(true),
                    Err(error) => return Err(error),
                };
                // Kids the local tree knows: moved elsewhere (wait for the
                // move), or deleted too (their own entries trash them; wait
                // for deleted folders).
                let known: Vec<Option<Ino>> = {
                    let tree = self.tree.read();
                    kids.iter().map(|kid| tree.by_link(&kid.link_id)).collect()
                };
                let mut foreign = 0usize;
                for (kid, held) in kids.iter().zip(known) {
                    let Some(held) = held else {
                        foreign += 1;
                        continue;
                    };
                    let (attached, deleted) = {
                        let tree = self.tree.read();
                        let node = tree.get(held);
                        (
                            node.is_some_and(|n| n.attached),
                            node.is_some_and(|n| n.deleted),
                        )
                    };
                    if attached {
                        return Err(BackendError::Transient(
                            "waiting for moves out of the folder".into(),
                        ));
                    }
                    if deleted && kid.kind == NodeKind::Folder {
                        // Its own check lists it; until then something new
                        // inside it would go to the trash with this folder.
                        return Err(BackendError::Transient(
                            "waiting for the folders deleted inside it".into(),
                        ));
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
                    return Ok(true);
                }
                (
                    ConflictKind::LocalDeleteRemoteChildren,
                    Some(foreign.to_string()),
                    None,
                )
            }
        };
        self.keep_remote(ino, fresh, kind, detail, local_rev).await;
        Ok(false)
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

    async fn upload(
        self: Arc<Self>,
        ino: Ino,
        target: Target,
        permit: tokio::sync::OwnedSemaphorePermit,
    ) {
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

    /// Materializes everything and snapshots the staging file (§7.4 1–3).
    async fn snapshot(&self, ino: Ino) -> std::result::Result<Snapshot, UploadError> {
        let staged = self.ensure_staged(ino).map_err(UploadError::core)?;
        for _ in 0..8 {
            let missing = staged.lock().missing_blocks();
            if missing.is_empty() {
                break;
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
            self.journal.list_later(vec![dir]);
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
