//! Pins (DESIGN.md §5.3, D060): files kept offline.
//!
//! A row in `pins` holds a file, or a whole folder, by inode. One task
//! reconciles: it walks the pinned subtrees, tells the cache which
//! revisions are pinned now (the current revision of each clean remote
//! file), and fetches what is missing. It looks at the tree each time it is
//! woken, so a change of any kind (an event, a rename, an upload, a removal)
//! has the same effect, and the old revision of a file is let go as soon as
//! the node names a new one. Staged files fetch nothing: their content is
//! local, or in a base the cache protects (§6.2).

use super::Engine;
use crate::cache::{BlockKey, Priority};
use crate::errors::{CoreError, Result};
use crate::health::Connectivity;
use crate::rows::sql_ino;
use crate::vfs::{Content, Ino, Tree};
use pdmount_backend::{LinkId, RevisionId};
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Free disk space pins never use.
const FREE_RESERVE: u64 = 1 << 30;
/// How often the task looks again without being woken, and while the disk
/// is full.
const RECHECK: Duration = Duration::from_secs(300);
const FULL_DISK_RECHECK: Duration = Duration::from_secs(60);
/// Wakes within this time share one walk: a rename storm elsewhere in the
/// tree must not keep it walking.
const SETTLE: Duration = Duration::from_millis(500);
/// A large file gives up its background turn this often, so that it does
/// not hold up event polling until it is complete (D027).
const BLOCKS_PER_TURN: usize = 16;

/// A row of `pins`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PinRow {
    path_at_set: String,
}

#[derive(Debug)]
pub(crate) struct Pins {
    /// Guards one field; takes nothing while held.
    roots: parking_lot::Mutex<BTreeMap<Ino, PinRow>>,
    /// Counts wakes, so that a fetch notices one between two files.
    generation: AtomicU64,
    wake: tokio::sync::Notify,
    /// The fetcher is waiting for free disk space.
    no_space: AtomicBool,
    /// Free bytes the tests pretend to have; unset, plenty.
    #[cfg(test)]
    pub(super) test_free_bytes: parking_lot::Mutex<Option<u64>>,
}

impl Pins {
    pub(super) fn load(store: &crate::store::Store) -> Result<Self> {
        let roots = store.read(|conn| {
            let mut statement = conn.prepare("SELECT ino, path_at_set FROM pins")?;
            let rows = statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, i64>(0)? as Ino,
                        PinRow {
                            path_at_set: row.get(1)?,
                        },
                    ))
                })?
                .collect::<rusqlite::Result<BTreeMap<_, _>>>()?;
            Ok(rows)
        })?;
        Ok(Self {
            roots: parking_lot::Mutex::new(roots),
            generation: AtomicU64::new(0),
            wake: tokio::sync::Notify::new(),
            no_space: AtomicBool::new(false),
            #[cfg(test)]
            test_free_bytes: parking_lot::Mutex::default(),
        })
    }

    fn nudge(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.wake.notify_one();
    }

    fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(super) fn nudge_for_tests(&self) {
        self.nudge();
    }
}

/// One pinned file with remote content.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PinnedFile {
    link: LinkId,
    rev: RevisionId,
    size: u64,
}

/// What a walk over the pinned subtrees found to do.
#[derive(Debug, Default)]
struct Plan {
    /// Pinned folders not listed yet, depth first.
    dirs: Vec<Ino>,
    /// Pinned files not fully in the disk cache.
    files: Vec<PinnedFile>,
}

/// How a round of work ended.
enum Round {
    /// Nothing left to do.
    Idle,
    /// Look at the tree again now.
    Again,
    /// Something failed; look again after a backoff.
    Failed,
}

/// `pin list`: one pinned path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinView {
    /// `None`: not reachable right now.
    pub path: Option<String>,
    pub path_at_set: String,
    pub files: u64,
    pub fetched_bytes: u64,
    /// As far as the pinned folders are listed.
    pub total_bytes: u64,
    pub state: PinState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinState {
    Fetching,
    Complete,
    WaitingOffline,
    WaitingSpace,
}

impl PinState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fetching => "fetching",
            Self::Complete => "complete",
            Self::WaitingOffline => "waiting-offline",
            Self::WaitingSpace => "waiting-space",
        }
    }
}

/// `status`: all pins together; a pin inside another counts once.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PinSummary {
    pub paths: u64,
    pub fetched_bytes: u64,
    pub total_bytes: u64,
}

/// Files with remote content, staged files' sizes and unlisted folders at or
/// below `top`.
fn walk(tree: &Tree, top: Ino) -> (Vec<PinnedFile>, u64, u64, Vec<Ino>) {
    let (mut files, mut staged, mut staged_bytes, mut dirs) = (Vec::new(), 0, 0, Vec::new());
    for ino in tree.subtree(top) {
        let Some(node) = tree.get(ino) else { continue };
        if !node.attached || node.deleted {
            continue;
        }
        if node.is_dir() {
            if !node.listed && node.remote.is_some() {
                dirs.push(ino);
            }
            continue;
        }
        match &node.content {
            Content::Remote {
                link,
                rev: Some(rev),
            } => files.push(PinnedFile {
                link: link.clone(),
                rev: rev.clone(),
                size: node.size,
            }),
            Content::Staged => {
                staged += 1;
                staged_bytes += node.size;
            }
            Content::Remote { rev: None, .. } => {}
        }
    }
    (files, staged, staged_bytes, dirs)
}

impl Engine {
    /// Wakes the pin task after a change to the tree. Nothing to do without
    /// pins: unpinning the last one wakes the task itself.
    pub(crate) fn pins_changed(&self) {
        if !self.pins.roots.lock().is_empty() {
            self.pins.nudge();
        }
    }

    /// Keeps a pin on a name when a new file replaces the pinned one
    /// (an atomic save, §7.6).
    pub(super) fn move_pin(&self, from: Ino, to: Ino) {
        let moved = {
            let mut roots = self.pins.roots.lock();
            let Some(row) = roots.remove(&from) else {
                return;
            };
            roots.entry(to).or_insert(row).clone()
        };
        self.store.write(move |conn| {
            conn.execute("DELETE FROM pins WHERE ino = ?1", [sql_ino(from)])?;
            conn.execute(
                "INSERT OR IGNORE INTO pins(ino, path_at_set, created_at)
                 VALUES (?1, ?2, strftime('%s','now'))",
                (sql_ino(to), &moved.path_at_set),
            )
            .map(drop)
        });
        self.pins.nudge();
    }

    fn free_disk_bytes(&self) -> u64 {
        #[cfg(test)]
        {
            self.pins.test_free_bytes.lock().unwrap_or(u64::MAX)
        }
        #[cfg(not(test))]
        {
            nix::sys::statvfs::statvfs(self.cache.dir()).map_or(u64::MAX, |stat| {
                stat.blocks_available() * stat.fragment_size()
            })
        }
    }

    /// `pin <path>`. Refused when what is known of the path does not fit on
    /// the disk, leaving [`FREE_RESERVE`].
    pub async fn pin_set(self: &Arc<Self>, path: &str) -> Result<()> {
        let ino = self.resolve(path).await?;
        if self.pins.roots.lock().contains_key(&ino) {
            return Ok(());
        }
        let this = Arc::clone(self);
        let (fetched, total) = tokio::task::spawn_blocking(move || {
            let (files, ..) = walk(&this.tree.read(), ino);
            this.fetched_of(&files)
        })
        .await
        .map_err(|error| CoreError::Internal(format!("pin: {error}")))?;
        let missing = total - fetched;
        let free = self.free_disk_bytes().saturating_sub(FREE_RESERVE);
        if missing > free {
            return Err(CoreError::Refused(format!(
                "no space left on device: {path} needs {missing} more bytes, {free} are free above the 1 GiB kept free"
            )));
        }
        let row = PinRow {
            path_at_set: path.to_owned(),
        };
        self.pins.roots.lock().insert(ino, row.clone());
        let created_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64);
        self.store.write(move |conn| {
            conn.execute(
                "INSERT OR REPLACE INTO pins(ino, path_at_set, created_at) VALUES (?1, ?2, ?3)",
                (sql_ino(ino), &row.path_at_set, created_at),
            )
            .map(drop)
        });
        self.pins.nudge();
        Ok(())
    }

    /// `unpin <path>`: removes the pin set on exactly this path.
    pub async fn pin_unset(self: &Arc<Self>, path: &str) -> Result<()> {
        let ino = self.resolve(path).await?;
        if self.pins.roots.lock().remove(&ino).is_none() {
            let through = {
                let roots: Vec<Ino> = self.pins.roots.lock().keys().copied().collect();
                let tree = self.tree.read();
                roots
                    .into_iter()
                    .find(|&root| tree.is_within(ino, root))
                    .and_then(|root| tree.path(root))
            };
            return Err(CoreError::Refused(match through {
                Some(root) => format!("{path} is pinned through {root}; unpin that path"),
                None => format!("{path} is not pinned"),
            }));
        }
        self.store.write(move |conn| {
            conn.execute("DELETE FROM pins WHERE ino = ?1", [sql_ino(ino)])
                .map(drop)
        });
        self.pins.nudge();
        Ok(())
    }

    /// Fetched and total bytes of `files`. Reads the store when a layout is
    /// not in memory, so not for the hot path.
    fn fetched_of(&self, files: &[PinnedFile]) -> (u64, u64) {
        let (mut fetched, mut total) = (0, 0);
        for file in files {
            total += file.size;
            if let Some(layout) = self.cache.layout_local(&file.link, &file.rev, file.size) {
                fetched += self.cache.bytes_on_disk(&file.link, &file.rev, &layout);
            }
        }
        (fetched, total)
    }

    /// `pin list`.
    pub async fn pin_list(self: &Arc<Self>) -> Result<Vec<PinView>> {
        let this = Arc::clone(self);
        tokio::task::spawn_blocking(move || this.pin_views())
            .await
            .map_err(|error| CoreError::Internal(format!("pin list: {error}")))
    }

    fn pin_views(&self) -> Vec<PinView> {
        let roots: Vec<(Ino, PinRow)> = self
            .pins
            .roots
            .lock()
            .iter()
            .map(|(ino, row)| (*ino, row.clone()))
            .collect();
        let online = self.health.state() == Connectivity::Online;
        let no_space = self.pins.no_space.load(Ordering::Relaxed);
        roots
            .into_iter()
            .map(|(ino, row)| {
                let (path, files, staged, staged_bytes, dirs) = {
                    let tree = self.tree.read();
                    let (files, staged, staged_bytes, dirs) = walk(&tree, ino);
                    (tree.path(ino), files, staged, staged_bytes, dirs)
                };
                let (fetched, total) = self.fetched_of(&files);
                let state = if fetched == total && dirs.is_empty() {
                    PinState::Complete
                } else if !online {
                    PinState::WaitingOffline
                } else if no_space {
                    PinState::WaitingSpace
                } else {
                    PinState::Fetching
                };
                PinView {
                    path,
                    path_at_set: row.path_at_set,
                    files: files.len() as u64 + staged,
                    fetched_bytes: fetched + staged_bytes,
                    total_bytes: total + staged_bytes,
                    state,
                }
            })
            .collect()
    }

    /// `status`. `None` without pins.
    pub async fn pin_summary(self: &Arc<Self>) -> Result<Option<PinSummary>> {
        let roots: Vec<Ino> = self.pins.roots.lock().keys().copied().collect();
        if roots.is_empty() {
            return Ok(None);
        }
        let this = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            let (mut files, mut staged_bytes) = (Vec::new(), 0);
            {
                let tree = this.tree.read();
                for &root in &roots {
                    let inside = roots
                        .iter()
                        .any(|&other| other != root && tree.is_within(root, other));
                    if !inside {
                        let (found, _, bytes, _) = walk(&tree, root);
                        files.extend(found);
                        staged_bytes += bytes;
                    }
                }
            }
            let (fetched, total) = this.fetched_of(&files);
            Some(PinSummary {
                paths: roots.len() as u64,
                fetched_bytes: fetched + staged_bytes,
                total_bytes: total + staged_bytes,
            })
        })
        .await
        .map_err(|error| CoreError::Internal(format!("pin status: {error}")))
    }

    /// Drops the pins of nodes that are gone, tells the cache which
    /// revisions are pinned now, and finds what is missing. Local only;
    /// reads the store, so it runs on a blocking thread.
    fn pin_plan(&self) -> Plan {
        let roots: Vec<Ino> = self.pins.roots.lock().keys().copied().collect();
        let mut gone = Vec::new();
        let mut plan = Plan::default();
        let mut pinned = HashSet::new();
        let mut files = Vec::new();
        {
            let tree = self.tree.read();
            for root in roots {
                if !tree.get(root).is_some_and(|node| node.attached) {
                    gone.push(root);
                    continue;
                }
                let (found, _, _, dirs) = walk(&tree, root);
                plan.dirs.extend(dirs);
                for file in found {
                    if pinned.insert((file.link.clone(), file.rev.clone())) {
                        files.push(file);
                    }
                }
            }
        }
        if !gone.is_empty() {
            let mut roots = self.pins.roots.lock();
            for ino in &gone {
                roots.remove(ino);
            }
            drop(roots);
            self.store.write(move |conn| {
                for ino in gone {
                    conn.execute("DELETE FROM pins WHERE ino = ?1", [sql_ino(ino)])?;
                }
                Ok(())
            });
        }
        self.cache.set_pinned_revisions(&pinned);
        for file in files {
            let (fetched, total) = self.fetched_of(std::slice::from_ref(&file));
            if fetched < total {
                plan.files.push(file);
            }
        }
        plan
    }

    /// The pin task: looks at the tree whenever it is woken, at the start,
    /// and every [`RECHECK`], and works until nothing is missing.
    pub(crate) async fn run_pins(self: Arc<Self>) {
        let mut failures = 0;
        loop {
            let seen = self.pins.generation();
            let this = Arc::clone(&self);
            let round = match tokio::task::spawn_blocking(move || this.pin_plan()).await {
                Ok(plan) => self.pin_round(plan, seen).await,
                Err(error) => {
                    tracing::error!(%error, "the pin walk did not finish");
                    Round::Failed
                }
            };
            match round {
                Round::Again => {}
                Round::Idle => {
                    failures = 0;
                    self.pins.no_space.store(false, Ordering::Relaxed);
                    tokio::select! {
                        () = self.pins.wake.notified() => {}
                        () = tokio::time::sleep(RECHECK) => {}
                    }
                    tokio::time::sleep(SETTLE).await;
                }
                // No parking: a pin is retried until it is complete or gone.
                Round::Failed => {
                    failures += 1;
                    tokio::time::sleep(crate::journal::backoff(failures)).await;
                }
            }
        }
    }

    /// Lists what is unlisted, else fetches what is missing, one paced turn
    /// at a time; a wake ends the round, so that the work follows the tree.
    async fn pin_round(self: &Arc<Self>, plan: Plan, seen: u64) -> Round {
        if plan.dirs.is_empty() && plan.files.is_empty() {
            return Round::Idle;
        }
        if !self.wait_until_online().await {
            return Round::Again;
        }
        let mut failed = false;
        // Listings first: what they find may belong to this round's walk.
        for dir in plan.dirs {
            if self.pins.generation() != seen {
                return Round::Again;
            }
            let turn = self.gate.background().await;
            let listed = self.ensure_listed_now(dir).await;
            drop(turn);
            // Also when it is gone: until an event says so, the folder would
            // be asked for again and again without a backoff.
            if let Err(error) = listed {
                tracing::debug!(dir, %error, "pinned folder not listed");
                failed = true;
            }
        }
        for file in plan.files {
            if self.pins.generation() != seen {
                return Round::Again;
            }
            if self.free_disk_bytes() < FREE_RESERVE {
                if !self.pins.no_space.swap(true, Ordering::Relaxed) {
                    tracing::warn!("pins wait: less than 1 GiB of disk space is free");
                }
                tokio::select! {
                    () = self.pins.wake.notified() => {}
                    () = tokio::time::sleep(FULL_DISK_RECHECK) => {}
                }
                return Round::Again;
            }
            self.pins.no_space.store(false, Ordering::Relaxed);
            if let Err(error) = self.fetch_pinned(&file).await {
                tracing::debug!(link = %file.link, %error, "pinned file not fetched");
                // Offline: the next round waits. Another file may still work
                // after any other failure.
                if self.health.state() != Connectivity::Online {
                    return Round::Again;
                }
                failed = true;
            }
        }
        if failed { Round::Failed } else { Round::Again }
    }

    /// `false`: woken while waiting, so the tree may have changed.
    async fn wait_until_online(&self) -> bool {
        tokio::select! {
            () = self.health.wait_online() => true,
            () = self.pins.wake.notified() => false,
        }
    }

    /// Fetches one revision's missing blocks at prefetch priority, in the
    /// background turns of D027.
    async fn fetch_pinned(&self, file: &PinnedFile) -> Result<()> {
        let mut turn = self.gate.background().await;
        let layout = self.cache.layout(&file.link, &file.rev, file.size).await?;
        let mut fetched = 0;
        for span in &layout.blocks {
            let key = BlockKey::new(&file.link, &file.rev, span.index);
            if self.cache.on_disk(&key) {
                continue;
            }
            if fetched == BLOCKS_PER_TURN {
                drop(turn);
                turn = self.gate.background().await;
                fetched = 0;
            }
            self.cache.get(&key, span.len, Priority::Prefetch).await?;
            fetched += 1;
        }
        drop(turn);
        // A block the disk cache could not take: without this the next round
        // would download the file again at once.
        if self.cache.bytes_on_disk(&file.link, &file.rev, &layout) < file.size {
            return Err(CoreError::Internal(
                "a pinned block did not reach the disk cache".into(),
            ));
        }
        Ok(())
    }
}
