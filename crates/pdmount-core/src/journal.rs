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
use crate::engine::Engine;
use crate::errors::{CoreError, Result};
use crate::metrics::{Metrics, check_not_hot};
use crate::vfs::{Content, DEFAULT_FILE_MODE, Ino, ROOT_INO, RemoteState};
use pdmount_backend::{
    BackendError, FileMeta, LinkId, NameHash, NodeKind, RevisionId, UploadSource,
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
const TRASH_BATCH: usize = 50;
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
    uploads_running: usize,
    /// Every entry is forced (draining before an unmount).
    draining: bool,
}

pub(crate) struct Journal {
    state: parking_lot::Mutex<State>,
    wake: tokio::sync::Notify,
    /// Signalled whenever an entry finishes, fails or leaves.
    progress: tokio::sync::Notify,
    debounce: Duration,
    max_dirty_age: Duration,
    concurrency: usize,
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
            state: parking_lot::Mutex::default(),
            wake: tokio::sync::Notify::new(),
            progress: tokio::sync::Notify::new(),
            debounce: config.debounce.0,
            max_dirty_age: config.max_dirty_age.0,
            concurrency: config.concurrency.max(1) as usize,
            meta_revision: config.meta_revision,
        }
    }

    fn entry(state: &mut State, ino: Ino, now: Instant) -> &mut Entry {
        let draining = state.draining;
        let entry = state.entries.entry(ino).or_insert_with(|| Entry::new(now));
        entry.forced |= draining;
        entry
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

    fn try_start_upload(&self, ino: Ino) -> bool {
        let mut state = self.state.lock();
        if state.uploads_running >= self.concurrency {
            return false;
        }
        let Some(entry) = state.entries.get_mut(&ino) else {
            return false;
        };
        entry.in_flight = true;
        state.uploads_running += 1;
        true
    }

    /// A step succeeded: re-plan the entry at once (it may need more).
    fn succeeded(&self, ino: Ino, upload: bool) {
        let now = Instant::now();
        {
            let mut state = self.state.lock();
            if upload {
                state.uploads_running = state.uploads_running.saturating_sub(1);
            }
            if let Some(entry) = state.entries.get_mut(&ino) {
                entry.in_flight = false;
                entry.attempts = 0;
                entry.not_before = now;
            }
            Self::unblock(&mut state, ino);
        }
        self.wake.notify_one();
        self.progress.notify_waiters();
    }

    /// A step failed: back off, or park after too many attempts.
    fn failed(&self, ino: Ino, upload: bool, error: &BackendError) -> bool {
        let now = Instant::now();
        let parked = {
            let mut state = self.state.lock();
            if upload {
                state.uploads_running = state.uploads_running.saturating_sub(1);
            }
            let Some(entry) = state.entries.get_mut(&ino) else {
                return false;
            };
            entry.in_flight = false;
            entry.attempts += 1;
            entry.last_error = Some(error.to_string());
            let permanent = permanent_reason(error);
            if let Some(reason) = permanent.filter(|_| !error.is_retryable())
                && (entry.attempts >= PARK_AFTER || parks_at_once(error))
            {
                entry.parked = Some(reason);
                true
            } else {
                let mut delay = match error {
                    BackendError::RateLimited {
                        retry_after: Some(after),
                    } => *after,
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

    fn park(&self, ino: Ino, upload: bool, reason: String) {
        let mut state = self.state.lock();
        if upload {
            state.uploads_running = state.uploads_running.saturating_sub(1);
        }
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
}

fn backoff(attempts: u32) -> Duration {
    let base = BACKOFF_MIN.saturating_mul(1 << attempts.min(9).saturating_sub(1));
    let jitter = rand::random::<f64>() * 0.3 + 0.85;
    base.mul_f64(jitter).min(BACKOFF_MAX)
}

/// Errors retrying cannot fix soon. `None`: keep retrying.
fn permanent_reason(error: &BackendError) -> Option<String> {
    match error {
        BackendError::RateLimited { .. }
        | BackendError::Transient(_)
        | BackendError::Offline(_)
        | BackendError::Unauthorized => None,
        other => Some(other.to_string()),
    }
}

fn parks_at_once(error: &BackendError) -> bool {
    matches!(
        error,
        BackendError::QuotaExceeded
            | BackendError::FileTooLarge
            | BackendError::Unsupported(_)
            | BackendError::PermissionDenied
            | BackendError::TooManyChildren
            | BackendError::NestingTooDeep
    )
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
                        if self.journal.try_start_upload(ino) {
                            let this = Arc::clone(&self);
                            self.runtime().spawn(async move {
                                this.upload(ino, target).await;
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
            (NodeKind::File, None) => match self.upload_wait(node.open_writers, timing, now, false)
            {
                Some(until) => Plan::Wait(until),
                None => reserve(Plan::Work(Work::Upload(Target::New {
                    parent: parent.clone(),
                    name: node.name.clone(),
                }))),
            },
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
                match self.upload_wait(node.open_writers, timing, now, !content) {
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
    ) -> Option<Instant> {
        if timing.forced {
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
            Ok(()) => self.journal.succeeded(ino, false),
            Err(error) => self.step_failed(ino, false, &error),
        }
    }

    fn step_failed(&self, ino: Ino, upload: bool, error: &BackendError) {
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
        if self.journal.failed(ino, upload, error) {
            tracing::warn!(ino, %error, "journal entry parked");
        }
    }

    async fn create_folder(
        self: &Arc<Self>,
        ino: Ino,
        parent: LinkId,
        name: String,
        mtime: SystemTime,
    ) -> std::result::Result<(), BackendError> {
        check_not_hot(&self.metrics, "create_folder");
        let mut adopted = false;
        let remote = match self.backend.create_folder(&parent, &name, mtime).await {
            Ok(link) => RemoteState {
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
            // A folder of that name exists remotely and nothing local knows
            // it: merge into it (§15.3 M7).
            Err(BackendError::AlreadyExists {
                existing: Some(existing),
            }) if self.tree.read().by_link(&existing).is_none() => {
                let node = self
                    .backend
                    .get_nodes(std::slice::from_ref(&existing))
                    .await?;
                match node.into_iter().next().flatten() {
                    Some(node) if node.kind == NodeKind::Folder => {
                        adopted = true;
                        RemoteState::from_remote(&node)
                    }
                    _ => {
                        return Err(BackendError::AlreadyExists {
                            existing: Some(existing),
                        });
                    }
                }
            }
            Err(error) => return Err(error),
        };
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
        if adopted {
            // Pick up children the existing folder may have.
            let this = Arc::clone(self);
            self.runtime().spawn(async move {
                if let Err(error) = this.list(ino).await {
                    tracing::debug!(ino, %error, "listing a merged folder failed");
                }
            });
        }
        Ok(())
    }

    async fn move_node(
        &self,
        ino: Ino,
        link: LinkId,
        new_parent: Option<LinkId>,
        name: String,
        expected: Option<NameHash>,
    ) -> std::result::Result<(), BackendError> {
        check_not_hot(&self.metrics, "rename");
        match self
            .backend
            .rename(&link, new_parent.as_ref(), &name, expected.as_ref())
            .await
        {
            Ok(()) => {}
            Err(BackendError::NameConflict) => {
                // The name changed remotely since it was read: refresh the
                // precondition and try again.
                let fresh = self.backend.get_nodes(std::slice::from_ref(&link)).await?;
                if let Some(node) = fresh.into_iter().next().flatten() {
                    let mut tree = self.tree.write();
                    tree.set_remote(ino, Some(RemoteState::from_remote(&node)));
                }
                return Err(BackendError::Transient("name hash refreshed".into()));
            }
            Err(error) => return Err(error),
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

    async fn trash(&self, batch: &[(Ino, LinkId)]) {
        check_not_hot(&self.metrics, "trash");
        let links: Vec<LinkId> = batch.iter().map(|(_, link)| link.clone()).collect();
        let results = match self.backend.trash(&links).await {
            Ok(results) => results,
            Err(error) => {
                for (ino, _) in batch {
                    self.step_failed(*ino, false, &error);
                }
                return;
            }
        };
        for ((ino, link), result) in batch.iter().zip(results) {
            match result {
                // Already gone (e.g. trashed with its folder) counts as done.
                Ok(()) | Err(BackendError::NotFound) => {
                    let droppable = {
                        let stamp = self.stamp();
                        let mut tree = self.tree.write();
                        tree.note_trashed(link.clone(), stamp);
                        if tree.link_of(*ino) == Some(link) {
                            tree.set_remote(*ino, None);
                        }
                        if let Some(node) = tree.get_mut(*ino) {
                            node.deleted = false;
                        }
                        tree.is_droppable(*ino)
                    };
                    self.persist(&[*ino]);
                    self.journal.succeeded(*ino, false);
                    if droppable {
                        self.tree.write().drop_node(*ino);
                        self.unstage(*ino);
                        self.persist(&[*ino]);
                        self.remove_staging_after_commit(*ino);
                    }
                }
                Err(error) => self.step_failed(*ino, false, &error),
            }
        }
    }

    async fn upload(self: Arc<Self>, ino: Ino, target: Target) {
        match self.upload_now(ino, &target).await {
            Ok(()) => self.journal.succeeded(ino, true),
            Err(UploadError::Backend(error)) => self.step_failed(ino, true, &error),
            Err(UploadError::Park(reason)) => {
                tracing::warn!(ino, reason, "upload parked");
                self.journal.park(ino, true, reason);
            }
        }
    }

    async fn upload_now(&self, ino: Ino, target: &Target) -> std::result::Result<(), UploadError> {
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
            self.materialize(ino, &missing, Priority::Readahead)
                .await
                .map_err(UploadError::core)?;
        }
        let (path, base_rev, gens, mtime, mode) = {
            let staged = staged.lock();
            if !staged.fully_materialized() {
                return Err(UploadError::Backend(BackendError::Transient(
                    "staging changed while materializing".into(),
                )));
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
        &self,
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
                    Err(BackendError::AlreadyExists {
                        existing: Some(existing),
                    }) => {
                        // Our own earlier upload, if the daemon died before
                        // recording it (same content).
                        let adopted = self.same_content(&existing, snapshot.sha1).await?;
                        match adopted {
                            Some(rev) => Uploaded {
                                link: existing,
                                rev,
                                parent: Some(parent.clone()),
                                name: name.clone(),
                                name_hash: None,
                            },
                            None => {
                                return Err(UploadError::Park(format!(
                                    "a different file named {name:?} exists remotely (conflict handling: M3)"
                                )));
                            }
                        }
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            Target::Revision { link } => {
                let remote = remote
                    .filter(|r| &r.link == link)
                    .ok_or(UploadError::Backend(BackendError::Transient(
                        "the remote node changed before the upload".into(),
                    )))?;
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
                        Err(BackendError::RevisionConflict { .. }) => {
                            match self.same_content(link, snapshot.sha1).await? {
                                Some(rev) => Some(rev),
                                None => {
                                    return Err(UploadError::Park(
                                        "the file changed remotely (conflict handling: M3)".into(),
                                    ));
                                }
                            }
                        }
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
        self.commit_upload(ino, &uploaded, snapshot);
        Ok(())
    }

    /// The active revision of `link` if its content has `sha1`.
    async fn same_content(
        &self,
        link: &LinkId,
        sha1: [u8; 20],
    ) -> std::result::Result<Option<RevisionId>, UploadError> {
        check_not_hot(&self.metrics, "get_nodes");
        let nodes = self.backend.get_nodes(std::slice::from_ref(link)).await?;
        Ok(nodes
            .into_iter()
            .next()
            .flatten()
            .filter(|n| n.kind == NodeKind::File && n.sha1 == Some(sha1) && !n.trashed)
            .and_then(|n| n.active_revision))
    }

    /// Records an upload (§7.4 7–8): the node is clean only if nothing
    /// changed since the snapshot. A clean node's staging goes once the row
    /// naming the uploaded revision is durable.
    fn commit_upload(&self, ino: Ino, uploaded: &Uploaded, snapshot: &Snapshot) {
        let staged = self.staged_entry(ino);
        let mut staged_guard = staged.as_ref().map(|s| s.lock());
        let stamp = self.stamp();
        let clean = {
            let mut tree = self.tree.write();
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
                return;
            };
            node.remote_stamp = stamp;
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
        if let Err(error) = self.cache.seed(
            &uploaded.link,
            &uploaded.rev,
            &snapshot.path,
            self.backend.upload_block_size(),
        ) {
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
}

impl UploadError {
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
    let mut buffer = vec![0u8; 1 << 20];
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
