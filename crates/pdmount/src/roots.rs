//! Further roots (M5.10; DESIGN.md §14.5; D079, D080, D110).
//!
//! Each `[[roots]]` entry is a mount of its own, next to My Files, in the
//! same daemon and session. A mount holds one tree or several; each tree is a
//! backend value for its volume and root folder ([`ProtonBackend::tree`],
//! [`FakeBackend::tree`]) with its own engine, store, staging and block cache
//! under `trees/<key>/` (§13.1), over the session's connectivity and
//! transfer limits. Its events come from the daemon's [`Pollers`], one per
//! volume. Every further tree's write gate is closed (M5.14 opens it), so
//! its write paths answer EROFS; the `test` kind's `writable` opens it for
//! tests.
//!
//! Failures stay with their mount: a tree that cannot open is left out of
//! its mount (and named in `status`), a mount whose trees all fail or whose
//! mountpoint cannot be mounted is reported as failed, and My Files and the
//! other mounts go on. The roots mount in the background after the daemon
//! reported ready, so a slow or failing one never delays or fails the start.
//! `unmount <mountpoint>` and `mount [-o ro] <mountpoint>` act on one
//! further mount through the control socket; see [`RootState`] for how they
//! run without holding up `status`. A mount unmounted from outside is noticed
//! within [`WATCH_INTERVAL`] and its trees are stopped.
//!
//! Kinds: `shared` (M5.12, D112: one tree per share of the shared-with-me
//! listing, trees that come and go while mounted; see [`shared`]),
//! `photos` (M5.13, D113: the Photos tree, not an engine; its own mount
//! over [`pdmount_fuse::mount_read_only`], its own event poll) and `test`
//! (test builds, trees named in the config on a fake drive). Computers
//! (out of M5's scope, D122) is refused by the config.

mod shared;

use crate::control::{self, Request};
use crate::mounts;
use anyhow::Context;
use pdmount_backend::fake::{FakeBackend, MAIN_VOLUME};
use pdmount_backend::photos::Photos;
use pdmount_backend::proton::ProtonBackend;
use pdmount_backend::shared::SharedWithMe;
use pdmount_backend::{Backend, LinkId};
use pdmount_core::cache::{CacheOptions, DiskBudget, SharedCap};
use pdmount_core::config::{RootConfig, RootKind};
use pdmount_core::conflicts::system_clock;
use pdmount_core::device::DeviceIdentity;
use pdmount_core::engine::marker::{MARKER_WINDOW, MarkerOutcome};
use pdmount_core::engine::{Engine, EngineOptions};
use pdmount_core::health::Health;
use pdmount_core::photos::{PhotosOptions, PhotosTree, ThumbnailOptions, local_month};
use pdmount_core::poller::{Pollers, VolumeUse};
use pdmount_core::readonly::ReadOnlyTree;
use pdmount_core::{Config, ProfilePaths};
use shared::SharedRoot;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long `unmount <further mountpoint>` uploads first, as `--wait` does.
const UNMOUNT_DRAIN: Duration = Duration::from_secs(600);
/// How long a stopped tree's running steps may go on before its store is
/// committed; the rest is retried at the next mount.
const STOP_GRACE: Duration = Duration::from_secs(10);
/// How long a stop waits for the FUSE session to end after the unmount.
const STOP_SESSION: Duration = Duration::from_secs(5);
/// The control call of a further unmount: the drain, the unmount, the stop.
const UNMOUNT_CALL: Duration = Duration::from_secs(
    UNMOUNT_DRAIN.as_secs() + 2 * (STOP_GRACE.as_secs() + STOP_SESSION.as_secs()) + 30,
);
/// The control call of a further mount: opening its trees may need the
/// network on Proton.
const MOUNT_CALL: Duration = Duration::from_secs(120);
/// How often the daemon looks for further mounts unmounted from outside.
const WATCH_INTERVAL: Duration = Duration::from_secs(1);
/// After a second stop signal (D092): how long the roots' stop still waits
/// for a FUSE session, a command's step or a gone share's cleanup.
const HURRIED_WAIT: Duration = Duration::from_millis(300);
const CONTROL_TIMEOUT: Duration = Duration::from_secs(30);

/// Where further trees get their backends: the session's.
#[derive(Clone)]
pub enum TreeSource {
    Proton(Arc<ProtonBackend>),
    Fake(FakeBackend),
}

impl TreeSource {
    /// The backend of the tree with root `root` on `volume`.
    fn tree(&self, volume: &str, root: &str) -> Arc<dyn Backend> {
        let root = LinkId::new(root);
        match self {
            Self::Proton(proton) => Arc::new(proton.tree(volume, root)),
            Self::Fake(fake) => Arc::new(fake.tree(volume, root)),
        }
    }

    /// The shared-with-me listing and the core event stream (M5.12).
    fn shares(&self) -> Arc<dyn SharedWithMe> {
        match self {
            Self::Proton(proton) => Arc::clone(proton) as Arc<dyn SharedWithMe>,
            Self::Fake(fake) => Arc::new(fake.clone()),
        }
    }

    /// The Photos volume and its timeline (M5.13).
    fn photos(&self) -> Arc<dyn Photos> {
        match self {
            Self::Proton(proton) => Arc::clone(proton) as Arc<dyn Photos>,
            Self::Fake(fake) => Arc::new(fake.clone()),
        }
    }

    /// The backend of the Photos tree: through the Photos clients on
    /// Proton.
    fn photos_tree(&self, volume: &str, root: &LinkId) -> Arc<dyn Backend> {
        match self {
            Self::Proton(proton) => Arc::new(proton.photos_tree(volume, root.clone())),
            Self::Fake(fake) => Arc::new(fake.tree(volume, root.clone())),
        }
    }

    /// The user's own volume (My Files').
    fn own_volume(&self) -> String {
        match self {
            Self::Proton(proton) => proton.volume_id().to_owned(),
            Self::Fake(_) => MAIN_VOLUME.to_owned(),
        }
    }
}

/// A tree to open: its folder name, volume and root, and its write gate.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TreeSpec {
    name: String,
    volume: String,
    root: String,
    /// The user's own volume (My Files', devices'): the short interval.
    /// A tree on another user's volume is a share: its pending marker names
    /// no device (D076).
    own: bool,
    writable: bool,
    /// `pending_marker` of its `[[roots]]` entry (D076, D114).
    pending_marker: bool,
}

/// An open tree.
#[derive(Clone)]
struct OpenTree {
    name: String,
    volume: String,
    own: bool,
    engine: Arc<Engine>,
}

/// A mounted root: its trees and FUSE session.
struct Active {
    trees: Vec<OpenTree>,
    mounted: pdmount_fuse::Mounted,
    /// Trees left out, with the reason.
    failed: Vec<(String, String)>,
    /// A Shared root: its trees follow the listing (D112).
    shared: Option<Arc<SharedRoot>>,
    /// A Photos root: its one tree, not an engine (D113).
    photos: Option<Arc<PhotosTree>>,
}

impl Active {
    /// Every tree of the mount now, a Shared root's included.
    fn all_trees(&self) -> Vec<OpenTree> {
        let mut trees = self.trees.clone();
        if let Some(shared) = &self.shared {
            trees.extend(shared.trees());
        }
        trees
    }
}

/// Why a mount is being stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Leaving {
    /// `pdmount unmount <mountpoint>`: its trees upload first.
    Command,
    /// Its session ended without the daemon, e.g. `fusermount3 -u`.
    Outside,
    /// The daemon stops.
    Stop,
}

/// A further mount's state. A command changes it in short steps under
/// [`Roots::mounts`]' lock; its long work (opening the trees, the drain,
/// the stop) runs between two steps without the lock, in `Mounting` or
/// `Unmounting`. So `status` always answers, and a second command on the
/// same mount is refused while one runs.
enum RootState {
    Unmounted {
        /// The last session ended from outside.
        outside: bool,
    },
    Mounting,
    Mounted(Box<Active>),
    Unmounting {
        trees: Vec<OpenTree>,
        why: Leaving,
    },
    Failed(String),
}

impl RootState {
    /// What `status` says.
    fn text(&self) -> String {
        match self {
            Self::Unmounted { outside: false } => "not mounted".into(),
            Self::Unmounted { outside: true } => "not mounted (unmounted from outside)".into(),
            Self::Mounting => "mounting".into(),
            Self::Mounted(active) if active.mounted.is_finished() => {
                "unmounted from outside".into()
            }
            Self::Mounted(_) => "mounted".into(),
            Self::Unmounting {
                why: Leaving::Command,
                ..
            } => "unmounting: uploading first".into(),
            Self::Unmounting {
                why: Leaving::Outside,
                ..
            } => "stopping: unmounted from outside".into(),
            Self::Unmounting {
                why: Leaving::Stop, ..
            } => "stopping".into(),
            Self::Failed(error) => format!("failed: {error}"),
        }
    }

    /// Why a command cannot act on a mount in this state.
    fn refusal(&self) -> &'static str {
        match self {
            Self::Unmounted { .. } => "is not mounted",
            Self::Failed(_) => "is not mounted (its last mount failed)",
            Self::Mounting => "is being mounted",
            Self::Mounted(_) => "is already mounted",
            Self::Unmounting {
                why: Leaving::Command,
                ..
            } => "is being unmounted",
            Self::Unmounting { .. } => "is being stopped",
        }
    }

    fn busy(&self) -> bool {
        matches!(self, Self::Mounting | Self::Unmounting { .. })
    }
}

struct RootMount {
    config: RootConfig,
    mountpoint: PathBuf,
    /// The last `mount`'s `-o ro`, or the config's `read_only`.
    read_only: bool,
    state: RootState,
}

impl RootMount {
    fn new(config: RootConfig, mountpoint: PathBuf) -> Self {
        Self {
            read_only: config.read_only,
            config,
            mountpoint,
            state: RootState::Unmounted { outside: false },
        }
    }

    /// The first step of a mount: from `Unmounted` or `Failed` only.
    /// `option_ro`: a command's `-o ro` (`None` at the daemon's start); the
    /// config's `read_only` holds either way. Returns whether the mount is
    /// read-only, `daemon_ro` (the daemon's `-o ro`) included.
    fn begin_mount(&mut self, option_ro: Option<bool>, daemon_ro: bool) -> anyhow::Result<bool> {
        if !matches!(
            self.state,
            RootState::Unmounted { .. } | RootState::Failed(_)
        ) {
            anyhow::bail!("{} {}", self.mountpoint.display(), self.state.refusal());
        }
        if let Some(option_ro) = option_ro {
            self.read_only = self.config.read_only || option_ro;
        }
        self.state = RootState::Mounting;
        Ok(self.read_only || daemon_ro || self.config.kind == RootKind::Photos)
    }

    /// The first step of an unmount or a stop: from `Mounted` only.
    fn begin_unmount(&mut self, why: Leaving) -> anyhow::Result<Box<Active>> {
        match std::mem::replace(&mut self.state, RootState::Mounting) {
            RootState::Mounted(active) => {
                self.state = RootState::Unmounting {
                    trees: active.all_trees(),
                    why,
                };
                Ok(active)
            }
            other => {
                self.state = other;
                anyhow::bail!("{} {}", self.mountpoint.display(), self.state.refusal())
            }
        }
    }

    /// A mount whose session ended from outside goes to `Unmounting`;
    /// `detached`: it is no longer in the mount table, so it ended for the
    /// user although open files keep its session.
    fn take_finished(&mut self, detached: bool) -> Option<Box<Active>> {
        match &self.state {
            RootState::Mounted(active) if detached || active.mounted.is_finished() => {
                self.begin_unmount(Leaving::Outside).ok()
            }
            _ => None,
        }
    }

    /// The mount as `status` shows it; `waiting`: the daemon has no session
    /// for further trees yet (D106); `intervals`: each volume's poll
    /// interval now (D107).
    fn reply(
        &self,
        daemon_ro: bool,
        waiting: bool,
        intervals: &HashMap<String, Duration>,
    ) -> control::RootReply {
        let tree_reply = |tree: &OpenTree, role: Option<&str>| {
            let queue = tree.engine.queue_counts();
            let note = tree.engine.write_was_refused().then(|| {
                "the server refused a change; the changes wait until the role allows writing"
                    .to_owned()
            });
            control::TreeReply {
                incoming: tree.engine.incoming_markers(),
                incoming_not_read: tree.engine.incoming_not_read(),
                note,
                name: tree.name.clone(),
                writable: tree.engine.write_gate_open(),
                pending: queue.pending as u64,
                parked: queue.parked as u64,
                events_last_poll_secs: tree.engine.events_last_poll().map(|age| age.as_secs()),
                error: None,
                cache_bytes: tree.engine.cache_usage().0,
                role: role.map(str::to_owned),
                poll_interval_secs: intervals.get(&tree.volume).map(Duration::as_secs),
                detail: None,
            }
        };
        let failed_reply = |(name, error): &(String, String)| control::TreeReply {
            incoming: Vec::new(),
            incoming_not_read: 0,
            note: None,
            name: name.clone(),
            writable: false,
            pending: 0,
            parked: 0,
            events_last_poll_secs: None,
            error: Some(error.clone()),
            cache_bytes: 0,
            role: None,
            poll_interval_secs: None,
            detail: None,
        };
        let trees = match &self.state {
            RootState::Mounted(active) => {
                let mut replies: Vec<control::TreeReply> = active
                    .trees
                    .iter()
                    .map(|tree| tree_reply(tree, None))
                    .collect();
                replies.extend(active.failed.iter().map(failed_reply));
                if let Some(shared) = &active.shared {
                    let view = shared.view();
                    replies.extend(
                        view.trees
                            .iter()
                            .map(|(tree, role)| tree_reply(tree, Some(role.as_str()))),
                    );
                    replies.extend(view.failed.iter().map(failed_reply));
                }
                if let Some(photos) = &active.photos {
                    replies.push(photos_reply(photos));
                }
                replies
            }
            RootState::Unmounting { trees, .. } => {
                trees.iter().map(|tree| tree_reply(tree, None)).collect()
            }
            _ => Vec::new(),
        };
        control::RootReply {
            mountpoint: self.mountpoint.clone(),
            kind: self.config.kind.as_str().to_owned(),
            state: match self.state {
                RootState::Unmounted { .. } if waiting => "waits for the keyring".into(),
                _ => self.state.text(),
            },
            // Photos is read-only whatever the config says (D080).
            read_only: self.read_only || daemon_ro || self.config.kind == RootKind::Photos,
            trees,
        }
    }
}

/// What a daemon needs to open further trees.
pub struct RootsContext {
    pub paths: ProfilePaths,
    pub config: Config,
    pub device: DeviceIdentity,
    /// My Files' connectivity, shared by every tree (§9.6).
    pub health: Arc<Health>,
    /// My Files' bulk-read guard, shared by every tree (D111).
    pub scan: Arc<pdmount_core::scan::ScanGuard>,
    /// My Files' transfer limits, shared by every tree (D033, D110).
    pub limits: Arc<pdmount_core::limits::SessionLimits>,
    /// The profile's disk cap, shared with My Files (D110 amended).
    pub budget: Arc<DiskBudget>,
    /// `mount -o ro` of the daemon: every mount read-only.
    pub read_only: bool,
    pub home: PathBuf,
}

/// The further mounts of a daemon.
pub struct Roots {
    context: RootsContext,
    pollers: Arc<Pollers>,
    /// Held for short steps only, never across an await.
    mounts: parking_lot::Mutex<Vec<RootMount>>,
    /// `true` from the daemon's stop on: no mount starts, a running
    /// unmount's drain ends.
    stopping: tokio::sync::watch::Sender<bool>,
    /// The flush's deadline, from the daemon's stop on.
    stop_deadline: parking_lot::Mutex<Option<Instant>>,
    /// When a second stop signal came (D092): the drains end, the waits for
    /// sessions, steps and commands get [`HURRIED_WAIT`] at most.
    hurried: tokio::sync::watch::Sender<Option<Instant>>,
    /// Gone shares whose cleanup still runs (D114).
    cleanups: Arc<Cleanups>,
    /// The session's backends and the cache key; `None` while the start
    /// waits for the keyring (D106, D110 amended).
    session: tokio::sync::watch::Sender<Option<Session>>,
}

/// What further trees open over: the session's backends and cache key.
#[derive(Clone)]
pub struct Session {
    pub source: TreeSource,
    pub master_key: Option<[u8; 32]>,
}

const WAITS_FOR_KEYRING: &str =
    "the daemon waits for the keyring; the further roots mount once it answers";

/// The mountpoints of `[[roots]]`, checked against My Files' and each
/// other: none may be inside another (a FUSE mount under a FUSE mount).
pub fn check_mountpoints(config: &Config, home: &Path, my_files: &Path) -> anyhow::Result<()> {
    let mut seen = vec![my_files.to_owned()];
    for mountpoint in config.root_mountpoints(home) {
        if let Some(other) = seen
            .iter()
            .find(|other| mountpoint.starts_with(other) || other.starts_with(&mountpoint))
        {
            anyhow::bail!(
                "the root mountpoint {} overlaps {}; give each root a mountpoint of its own",
                mountpoint.display(),
                other.display()
            );
        }
        seen.push(mountpoint);
    }
    Ok(())
}

/// The further root mounted at `mountpoint`, if the config has one.
pub fn configured(config: &Config, home: &Path, mountpoint: &Path) -> Option<usize> {
    config
        .root_mountpoints(home)
        .iter()
        .position(|candidate| candidate == mountpoint)
}

impl Roots {
    pub fn new(context: RootsContext, pollers: Arc<Pollers>) -> Arc<Self> {
        let home = context.home.clone();
        let mounts = context
            .config
            .roots
            .iter()
            .zip(context.config.root_mountpoints(&home))
            .map(|(config, mountpoint)| RootMount::new(config.clone(), mountpoint))
            .collect();
        Arc::new(Self {
            context,
            pollers,
            mounts: parking_lot::Mutex::new(mounts),
            stopping: tokio::sync::watch::Sender::new(false),
            stop_deadline: parking_lot::Mutex::new(None),
            hurried: tokio::sync::watch::Sender::new(None),
            cleanups: Arc::default(),
            session: tokio::sync::watch::Sender::new(None),
        })
    }

    /// The session the trees open over: at once after a start that had
    /// the keyring, or when the keyring wait ends (D106, D110 amended).
    pub fn set_session(&self, session: Session) {
        self.session.send_replace(Some(session));
    }

    fn session(&self) -> Option<Session> {
        self.session.borrow().clone()
    }

    /// After the daemon reported ready: mounts every configured root in the
    /// background once the session is there (a failure is logged and kept
    /// for `status`), and watches for mounts whose session ends from
    /// outside.
    pub fn start(self: &Arc<Self>) {
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let mut session = this.session.subscribe();
            let mut stopping = this.stopping.subscribe();
            let ready = tokio::select! {
                ready = session.wait_for(Option::is_some) => ready.is_ok(),
                _ = stopping.wait_for(|stopping| *stopping) => false,
            };
            if ready {
                this.mount_all().await;
            }
        });
        let this = Arc::clone(self);
        tokio::spawn(async move { this.watch().await });
    }

    fn is_stopping(&self) -> bool {
        *self.stopping.borrow()
    }

    /// The deadline a tree stopped now gets for its running steps: the
    /// flush's from the daemon's stop on (one grace, D092), else
    /// [`STOP_GRACE`] from now.
    fn stop_deadline(&self) -> Instant {
        self.stop_deadline
            .lock()
            .unwrap_or_else(|| Instant::now() + STOP_GRACE)
    }

    /// The deadline of a gone share's running steps: [`STOP_GRACE`] from
    /// now, and no later than the flush's deadline once the daemon stops.
    fn cleanup_deadline(&self) -> Instant {
        let grace = Instant::now() + STOP_GRACE;
        self.stop_deadline
            .lock()
            .map_or(grace, |deadline| deadline.min(grace))
    }

    /// A second stop signal (D092): the further roots' drains end, and their
    /// stop no longer waits for running steps, sessions or commands, so My
    /// Files' store is committed and the tokens saved before the
    /// supervisor's kill.
    pub fn hurry(&self) {
        let first = self.hurried.send_if_modified(|hurried| {
            if hurried.is_some() {
                return false;
            }
            *hurried = Some(Instant::now());
            true
        });
        if !first {
            return;
        }
        let mut engines = self.cleanups.engines();
        for mount in self.mounts.lock().iter() {
            match &mount.state {
                RootState::Mounted(active) => {
                    engines.extend(active.all_trees().into_iter().map(|tree| tree.engine));
                }
                RootState::Unmounting { trees, .. } => {
                    engines.extend(trees.iter().map(|tree| Arc::clone(&tree.engine)));
                }
                _ => {}
            }
        }
        for engine in engines {
            engine.end_shutdown_wait();
        }
    }

    fn hurried_at(&self) -> Option<Instant> {
        *self.hurried.borrow()
    }

    /// `end`, or earlier after a second stop signal.
    fn wait_end(&self, end: Instant) -> Instant {
        self.hurried_at()
            .map_or(end, |at| end.min(at + HURRIED_WAIT))
    }

    /// Resolves at the second stop signal.
    async fn until_hurried(&self) {
        let mut hurried = self.hurried.subscribe();
        let _ = hurried.wait_for(Option::is_some).await;
    }

    async fn mount_all(self: &Arc<Self>) {
        let count = self.mounts.lock().len();
        for index in 0..count {
            if self.is_stopping() {
                break;
            }
            if let Err(error) = self.mount_index(index, None).await {
                tracing::error!(error = %format!("{error:#}"), "a further root did not mount");
            }
        }
    }

    /// Stops the trees of a mount whose session ended from outside, within
    /// a second.
    async fn watch(&self) {
        let mut stopping = self.stopping.subscribe();
        loop {
            tokio::select! {
                () = tokio::time::sleep(WATCH_INTERVAL) => {}
                _ = stopping.wait_for(|stopping| *stopping) => return,
            }
            let count = self.mounts.lock().len();
            for index in 0..count {
                self.reap(index).await;
            }
        }
    }

    /// `pdmount mount [-o ro] <mountpoint>` while the daemon runs.
    pub async fn mount(self: &Arc<Self>, mountpoint: &Path, read_only: bool) -> anyhow::Result<()> {
        let index = self.index_of(mountpoint)?;
        self.mount_index(index, Some(read_only)).await
    }

    fn index_of(&self, mountpoint: &Path) -> anyhow::Result<usize> {
        self.mounts
            .lock()
            .iter()
            .position(|mount| mount.mountpoint == mountpoint)
            .with_context(|| format!("no root is configured at {}", mountpoint.display()))
    }

    async fn mount_index(
        self: &Arc<Self>,
        index: usize,
        option_ro: Option<bool>,
    ) -> anyhow::Result<()> {
        // A session that ended from outside is stopped first.
        self.reap(index).await;
        self.settle(index).await;
        let (config, mountpoint, read_only) = {
            let mut mounts = self.mounts.lock();
            if self.is_stopping() {
                anyhow::bail!("the daemon is stopping");
            }
            if self.session.borrow().is_none() {
                anyhow::bail!(WAITS_FOR_KEYRING);
            }
            let mount = &mut mounts[index];
            let read_only = mount.begin_mount(option_ro, self.context.read_only)?;
            (mount.config.clone(), mount.mountpoint.clone(), read_only)
        };
        let opened = self.open_mount(&config, &mountpoint, read_only).await;
        let late = {
            let mut mounts = self.mounts.lock();
            let mount = &mut mounts[index];
            match opened {
                Err(error) => {
                    mount.state = RootState::Failed(format!("{error:#}"));
                    return Err(error.context(format!("cannot mount {}", mountpoint.display())));
                }
                // A stop began meanwhile: it waits for this mount to end.
                Ok(active) if self.is_stopping() => {
                    mount.state = RootState::Unmounting {
                        trees: active.trees.clone(),
                        why: Leaving::Stop,
                    };
                    active
                }
                Ok(active) => {
                    for tree in &active.trees {
                        announce_tree(&tree.name, &tree.engine);
                        tree.engine.start_background_polled();
                        self.pollers
                            .add(&tree.volume, tree.own, Arc::clone(&tree.engine));
                    }
                    mount.state = RootState::Mounted(active);
                    tracing::info!(
                        mountpoint = %mountpoint.display(),
                        kind = config.kind.as_str(),
                        read_only,
                        "further root mounted"
                    );
                    return Ok(());
                }
            }
        };
        // As the stop does with a mount it takes: unmounted (detached when
        // busy) before its trees stop, so no mount is left behind.
        detach(&mountpoint).await;
        self.stop(late, self.stop_deadline()).await;
        self.set_state(index, RootState::Unmounted { outside: false });
        anyhow::bail!("the daemon is stopping")
    }

    fn set_state(&self, index: usize, state: RootState) {
        self.mounts.lock()[index].state = state;
    }

    fn state_is(&self, index: usize, test: impl Fn(&RootState) -> bool) -> bool {
        test(&self.mounts.lock()[index].state)
    }

    /// Stops the trees of a mount whose session ended from outside, or
    /// that is no longer in the mount table (`fusermount3 -uz` while a file
    /// is open keeps the session); `true` if it did.
    async fn reap(&self, index: usize) -> bool {
        let mountpoint = {
            let mounts = self.mounts.lock();
            let mount = &mounts[index];
            if !matches!(mount.state, RootState::Mounted(_)) {
                return false;
            }
            mount.mountpoint.clone()
        };
        // A table that cannot be read proves nothing.
        let detached = matches!(mounts::find(&mountpoint), Ok(None));
        let Some(active) = self.mounts.lock()[index].take_finished(detached) else {
            return false;
        };
        tracing::warn!(
            mountpoint = %mountpoint.display(),
            "a further root was unmounted from outside; stopping its trees"
        );
        self.stop(active, self.stop_deadline()).await;
        self.set_state(index, RootState::Unmounted { outside: true });
        tracing::info!(mountpoint = %mountpoint.display(), "further root stopped");
        true
    }

    /// Waits while another task stops a mount that ended from outside;
    /// `true` if it waited.
    async fn settle(&self, index: usize) -> bool {
        let outside = |state: &RootState| {
            matches!(
                state,
                RootState::Unmounting {
                    why: Leaving::Outside,
                    ..
                }
            )
        };
        let end = Instant::now() + STOP_GRACE + STOP_SESSION;
        let mut waited = false;
        while self.state_is(index, outside) && Instant::now() < end {
            waited = true;
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        waited
    }

    /// The trees a root maps.
    fn specs(&self, config: &RootConfig, source: &TreeSource) -> anyhow::Result<Vec<TreeSpec>> {
        match config.kind {
            // Its trees come from the listing once mounted; Photos' tree is
            // not an engine.
            RootKind::Shared | RootKind::Photos => Ok(Vec::new()),
            RootKind::Test => {
                if !cfg!(feature = "fake-backend") || !matches!(source, TreeSource::Fake(_)) {
                    anyhow::bail!("roots of kind \"test\" need a test build and a fake drive");
                }
                Ok(config
                    .tree
                    .iter()
                    .map(|tree| TreeSpec {
                        name: tree.name.clone(),
                        volume: tree.volume.clone(),
                        root: tree.root.clone(),
                        own: tree.volume == MAIN_VOLUME,
                        writable: tree.writable,
                        pending_marker: config.pending_marker,
                    })
                    .collect())
            }
            kind => anyhow::bail!("roots of kind {:?} are not available yet", kind.as_str()),
        }
    }

    /// Opens a root's trees and mounts them. Their background work starts
    /// when the caller keeps the mount; a Shared root's starts with each
    /// share it opens.
    async fn open_mount(
        self: &Arc<Self>,
        config: &RootConfig,
        mountpoint: &Path,
        read_only: bool,
    ) -> anyhow::Result<Box<Active>> {
        let session = self.session().context(WAITS_FOR_KEYRING)?;
        let specs = self.specs(config, &session.source)?;
        clear_dead_mount(mountpoint).await?;
        std::fs::create_dir_all(mountpoint)
            .with_context(|| format!("cannot create {}", mountpoint.display()))?;
        if config.kind == RootKind::Shared {
            return self
                .open_shared(mountpoint, read_only, session, config.pending_marker)
                .await;
        }
        if config.kind == RootKind::Photos {
            return self.open_photos(mountpoint, session).await;
        }
        let mut opened = Vec::new();
        let mut failed = Vec::new();
        for spec in specs {
            // The trees opened so far are stopped by the late path.
            if self.is_stopping() {
                break;
            }
            match self.open_tree(&spec, &session, read_only).await {
                Ok((engine, invalidations)) => opened.push((spec, engine, invalidations)),
                Err(error) => {
                    let error = format!("{error:#}");
                    tracing::warn!(tree = spec.name, error, "a tree did not open");
                    failed.push((spec.name, error));
                }
            }
        }
        if opened.is_empty() {
            let reasons: Vec<String> = failed
                .iter()
                .map(|(name, error)| format!("{name}: {error}"))
                .collect();
            anyhow::bail!("no tree opened ({})", reasons.join("; "));
        }
        let mut trees = Vec::new();
        let mut parts = Vec::new();
        for (spec, engine, invalidations) in opened {
            parts.push(pdmount_fuse::MountTree {
                name: spec.name.clone(),
                engine: Arc::clone(&engine),
                invalidations,
                volume_use: (!spec.own)
                    .then(|| VolumeUse::new(Arc::clone(self.pollers.cadence()), &spec.volume)),
                place: pdmount_fuse::TreePlace::Folder,
            });
            trees.push(OpenTree {
                name: spec.name,
                volume: spec.volume,
                own: spec.own,
                engine,
            });
        }
        let target = mountpoint.to_owned();
        let mounted =
            tokio::task::spawn_blocking(move || pdmount_fuse::mount_trees(parts, &target)).await?;
        match mounted {
            Ok(mounted) => Ok(Box::new(Active {
                trees,
                mounted,
                failed,
                shared: None,
                photos: None,
            })),
            Err(error) => {
                for tree in &trees {
                    let _ = tree.engine.shutdown().await;
                }
                Err(error).with_context(|| format!("cannot mount at {}", mountpoint.display()))
            }
        }
    }

    /// A Shared root (D112): an empty mount whose trees follow the
    /// shared-with-me listing from now on.
    async fn open_shared(
        self: &Arc<Self>,
        mountpoint: &Path,
        read_only: bool,
        session: Session,
        pending_marker: bool,
    ) -> anyhow::Result<Box<Active>> {
        let target = mountpoint.to_owned();
        let config = self.context.config.clone();
        let ro = read_only;
        let mounted = tokio::task::spawn_blocking(move || {
            pdmount_fuse::mount_dynamic(Vec::new(), &target, &config, ro, shared::FILES_FOLDER)
        })
        .await?
        .with_context(|| format!("cannot mount at {}", mountpoint.display()))?;
        let set = mounted
            .tree_set()
            .context("a Shared mount without its tree set")?;
        let shared = SharedRoot::start(self, session, set, read_only, pending_marker).await;
        Ok(Box::new(Active {
            trees: Vec::new(),
            mounted,
            failed: Vec::new(),
            shared: Some(shared),
            photos: None,
        }))
    }

    /// A Photos root (D113): the Photos tree over the user's Photos volume,
    /// read-only, in a mount of its own. Its state is under `trees/<key>/`
    /// like a further tree's, its blocks count against the profile's cap,
    /// and it polls the Photos volume itself at `sync.poll_interval`.
    async fn open_photos(
        self: &Arc<Self>,
        mountpoint: &Path,
        session: Session,
    ) -> anyhow::Result<Box<Active>> {
        let api = session.source.photos();
        let volume = api.photos_volume().await?.context(
            "this account has no Photos yet; the official apps make it at the first upload",
        )?;
        let paths = &self.context.paths;
        let key = pdmount_core::paths::tree_key(&volume.volume, &volume.root.0);
        let mut config = self.context.config.clone();
        config.access = Default::default();
        config.fsync.remote_paths.clear();
        config.sync.pending_marker = false;
        config.roots.clear();
        let options = PhotosOptions {
            volume: volume.volume.clone(),
            root: volume.root.clone(),
            state_db: paths.tree_state_db(&key),
            cache: CacheOptions {
                dir: paths.tree_blocks_dir(&key),
                master_key: session.master_key,
                ram_bytes: pdmount_core::pool::MIN_RAM_BYTES,
                disk_bytes: config.cache.max_bytes.0,
                shared: Some(SharedCap {
                    budget: Arc::clone(&self.context.budget),
                    keeps_half: false,
                }),
                // The session's, from `limits` below (D033, D110).
                downloads: None,
            },
            config,
            health: Some(Arc::clone(&self.context.health)),
            placer: local_month,
            // The URIs are those of the photos' paths in this mount (D115).
            thumbnails: std::path::absolute(mountpoint)
                .ok()
                .map(|mountpoint| ThumbnailOptions {
                    dir: paths.thumbnails_dir.clone(),
                    mountpoint,
                }),
            // The session's bulk-read guard, download permits and background
            // gate, as every tree's (D110, D111).
            scan: Some(Arc::clone(&self.context.scan)),
            limits: Some(Arc::clone(&self.context.limits)),
        };
        let backend = session.source.photos_tree(&volume.volume, &volume.root);
        let (tree, invalidations) = PhotosTree::open(backend, api, options)
            .await
            .context("cannot open the Photos tree")?;
        let target = mountpoint.to_owned();
        let mounted_tree = Arc::clone(&tree) as Arc<dyn ReadOnlyTree>;
        let mounted = tokio::task::spawn_blocking(move || {
            pdmount_fuse::mount_read_only(mounted_tree, invalidations, &target)
        })
        .await?;
        let mounted = match mounted {
            Ok(mounted) => mounted,
            Err(error) => {
                let _ = tree.shutdown().await;
                return Err(error)
                    .with_context(|| format!("cannot mount at {}", mountpoint.display()));
            }
        };
        tree.start();
        tracing::info!(volume = volume.volume, "the Photos tree is mounted");
        Ok(Box::new(Active {
            trees: Vec::new(),
            mounted,
            failed: Vec::new(),
            shared: None,
            photos: Some(tree),
        }))
    }

    /// Deletes a gone share's state and cached blocks (`trees/<key>/`).
    fn purge_tree(&self, key: &str) {
        let paths = &self.context.paths;
        for dir in [
            paths.tree_state_db(key).parent().map(Path::to_owned),
            paths.tree_blocks_dir(key).parent().map(Path::to_owned),
        ]
        .into_iter()
        .flatten()
        {
            match std::fs::remove_dir_all(&dir) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    tracing::warn!(dir = %dir.display(), %error, "cannot delete a gone share's state")
                }
            }
        }
    }

    /// One tree's engine: its own store, staging and cache (§13.1), the
    /// session's connectivity, and a config without My Files' path rules.
    async fn open_tree(
        &self,
        spec: &TreeSpec,
        session: &Session,
        read_only: bool,
    ) -> anyhow::Result<(
        Arc<Engine>,
        tokio::sync::mpsc::UnboundedReceiver<pdmount_core::vfs::Invalidation>,
    )> {
        let backend = session.source.tree(&spec.volume, &spec.root);
        self.open_tree_on(spec, backend, session, read_only).await
    }

    /// [`Roots::open_tree`] over `backend`.
    async fn open_tree_on(
        &self,
        spec: &TreeSpec,
        backend: Arc<dyn Backend>,
        session: &Session,
        read_only: bool,
    ) -> anyhow::Result<(
        Arc<Engine>,
        tokio::sync::mpsc::UnboundedReceiver<pdmount_core::vfs::Invalidation>,
    )> {
        let paths = &self.context.paths;
        let key = pdmount_core::paths::tree_key(&spec.volume, &spec.root);
        let mut config = self.context.config.clone();
        // Rules and globs name paths of My Files; the marker is the root's
        // own opt-in (D076, D114).
        config.access = Default::default();
        config.fsync.remote_paths.clear();
        config.sync.pending_marker = spec.pending_marker;
        config.roots.clear();
        let options = EngineOptions {
            state_db: paths.tree_state_db(&key),
            staging_dir: paths.tree_staging_dir(&key),
            local_ignore: Some(paths.local_ignore_file()),
            // The root's `/.pdignore` only on the user's own volume (D124).
            remote_ignore: spec.own,
            cache: CacheOptions {
                dir: paths.tree_blocks_dir(&key),
                master_key: session.master_key,
                // The smallest pool: a further tree is browsed more than
                // worked in (D110).
                ram_bytes: pdmount_core::pool::MIN_RAM_BYTES,
                disk_bytes: config.cache.max_bytes.0,
                // One cap for the profile; My Files keeps half of it.
                shared: Some(SharedCap {
                    budget: Arc::clone(&self.context.budget),
                    keeps_half: false,
                }),
                downloads: None,
            },
            config,
            device: self.context.device.clone(),
            clock: system_clock(),
            read_only,
            write_gate: spec.writable,
            health: Some(Arc::clone(&self.context.health)),
            scan: Some(Arc::clone(&self.context.scan)),
            limits: Some(Arc::clone(&self.context.limits)),
        };
        if let Some(parent) = options.state_db.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
        }
        let opened = Engine::open(backend, options)
            .await
            .with_context(|| format!("cannot open the tree {}", spec.name))?;
        // In a share the marker names no device (D076).
        opened.0.set_marker_anonymous(!spec.own);
        Ok(opened)
    }

    /// Uploads what the trees have pending, all at once, until `deadline`
    /// or a second stop signal; with `until_stop`, also until the daemon's
    /// stop begins. At the daemon's stop (not `until_stop`), a tree whose
    /// root asks for it leaves its pending marker in the last
    /// [`MARKER_WINDOW`] (D076, D114).
    async fn drain(&self, trees: &[OpenTree], deadline: Instant, until_stop: bool) {
        let all = futures::future::join_all(
            trees
                .iter()
                .map(|tree| drain_tree(tree, deadline, !until_stop)),
        );
        let mut stopping = self.stopping.subscribe();
        let stop = async {
            if until_stop {
                let _ = stopping.wait_for(|stopping| *stopping).await;
            } else {
                std::future::pending::<()>().await;
            }
        };
        tokio::select! {
            _ = all => {}
            () = stop => {}
            () = self.until_hurried() => {}
        }
    }

    /// Where a gone share's queued changes go (D080, D114):
    /// `orphaned/<name> (<time>)/` in the profile's data directory.
    fn orphan_dir(&self, name: &str) -> PathBuf {
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
        // Room for the stamp within a file name's 255 bytes.
        let mut end = name.len().min(200);
        while !name.is_char_boundary(end) {
            end -= 1;
        }
        let base = self.context.paths.orphaned_dir();
        let mut dir = base.join(format!("{} ({stamp})", &name[..end]));
        let mut n = 2;
        while dir.exists() {
            dir = base.join(format!("{} ({stamp}, {n})", &name[..end]));
            n += 1;
        }
        dir
    }

    /// Exports the queued changes of a share that is gone (RQ3, D080,
    /// D114): its queue is held, the content of each queued file and the
    /// list of changes go to a new directory under `orphaned/`, with a
    /// desktop notification. Returns the directory and how many files'
    /// content could not be read, or `None` when nothing could be written.
    async fn orphan(
        &self,
        name: &str,
        engine: &Arc<Engine>,
        reason: &str,
    ) -> Option<(PathBuf, usize)> {
        use pdmount_core::engine::orphan::OrphanManifest;
        engine.withdraw_write_access();
        let dir = self.orphan_dir(name);
        let entries = match engine.export_queued(&dir).await {
            Ok(entries) => entries,
            Err(error) => {
                tracing::error!(dir = %dir.display(), %error, "cannot export a gone share's changes");
                return None;
            }
        };
        let manifest = OrphanManifest::new(name, reason, entries);
        if let Err(error) = manifest.write(&dir) {
            tracing::error!(dir = %dir.display(), %error, "cannot write the list of a gone share's changes");
            return None;
        }
        let count = manifest.entries.len();
        let incomplete = manifest.incomplete();
        tracing::warn!(
            share = name,
            reason,
            changes = count,
            incomplete,
            dir = %dir.display(),
            "a share went with changes queued; they are kept in orphaned/"
        );
        let noun = if count == 1 { "change" } else { "changes" };
        // The name is plain text in the summary; the body may be markup.
        crate::notify::desktop(
            &format!("PdMount: {count} {noun} to {name} kept"),
            "The share is gone, so they cannot be uploaded. `pdmount status` shows where they are.",
        );
        Some((dir, incomplete))
    }

    /// A gone tree's state: deleted, or with `keep` moved into that
    /// directory (`state/`); its cached blocks are deleted either way.
    fn retire_tree(&self, key: &str, keep: Option<&Path>) {
        let paths = &self.context.paths;
        if let (Some(keep), Some(state)) = (keep, paths.tree_state_db(key).parent()) {
            match std::fs::rename(state, keep.join("state")) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    tracing::error!(
                        dir = %state.display(),
                        %error,
                        "cannot move a gone share's state beside its orphaned changes; it stays"
                    );
                    return;
                }
            }
        }
        self.purge_tree(key);
    }

    /// `unmount <mountpoint>` of a further root: uploads what is pending
    /// (up to [`UNMOUNT_DRAIN`]), then unmounts it; the daemon and the
    /// other mounts go on. A mount that was unmounted from outside is only
    /// stopped. A busy mount stays mounted. When the daemon's stop begins
    /// meanwhile, the drain ends and the mount is stopped as the stop
    /// stops the others (D110): unmounted, detached when busy, its trees
    /// stopped.
    pub async fn unmount(&self, mountpoint: &Path) -> anyhow::Result<()> {
        let index = self.index_of(mountpoint)?;
        if self.reap(index).await || self.settle(index).await {
            return Ok(());
        }
        let active = {
            let mut mounts = self.mounts.lock();
            if self.is_stopping() {
                anyhow::bail!("the daemon is stopping");
            }
            mounts[index].begin_unmount(Leaving::Command)?
        };
        tracing::info!(mountpoint = %mountpoint.display(), "unmounting a further root");
        self.drain(&active.all_trees(), Instant::now() + UNMOUNT_DRAIN, true)
            .await;
        if !self.is_stopping() {
            let target = mountpoint.to_owned();
            let unmounted =
                tokio::task::spawn_blocking(move || crate::daemon::fusermount(&target, false))
                    .await
                    .map_err(anyhow::Error::from)
                    .and_then(|result| result);
            match unmounted {
                Ok(()) => {
                    self.stop(active, Instant::now() + STOP_GRACE).await;
                    self.set_state(index, RootState::Unmounted { outside: false });
                    tracing::info!(mountpoint = %mountpoint.display(), "further root unmounted");
                    return Ok(());
                }
                Err(error) => {
                    // Busy: it stays mounted. One that ended meanwhile is
                    // stopped by the watch. Under the lock, so the daemon's
                    // stop either finds it mounted or is seen here.
                    let mut mounts = self.mounts.lock();
                    if !self.is_stopping() {
                        mounts[index].state = RootState::Mounted(active);
                        return Err(error);
                    }
                }
            }
        }
        tracing::warn!(
            mountpoint = %mountpoint.display(),
            "the daemon stops: unmounting the further root without uploading the rest"
        );
        detach(mountpoint).await;
        self.stop(active, self.stop_deadline()).await;
        self.set_state(index, RootState::Unmounted { outside: false });
        anyhow::bail!(
            "the daemon is stopping; {} was unmounted",
            mountpoint.display()
        )
    }

    /// Ends a mount's session and stops its trees, all at once: running
    /// steps get until `deadline`, then the stores are committed. A Shared
    /// root stops following the listing first, and the cleanups of its gone
    /// shares end before the deadline. A session that outlives the wait (a
    /// detached mount with files open) gets its trees retired, so nothing
    /// written there later is acknowledged.
    async fn stop(&self, active: Box<Active>, deadline: Instant) {
        let Active {
            mut trees,
            mounted,
            shared,
            photos,
            ..
        } = *active;
        if let Some(shared) = shared {
            trees.extend(shared.close().await);
            if !self.cleanups.wait_all(self.wait_end(deadline)).await {
                tracing::warn!("a gone share's cleanup still runs; it goes on in the background");
            }
        }
        if let Some(photos) = photos
            && let Err(error) = photos.shutdown().await
        {
            tracing::error!(%error, "could not commit the Photos tree's store");
        }
        let end = self.wait_end(Instant::now() + STOP_SESSION);
        while !mounted.is_finished() && Instant::now() < end {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        if !mounted.is_finished() {
            for tree in &trees {
                tree.engine.retire();
            }
        }
        // Dropping the session unmounts it if it still is mounted.
        let _ = tokio::task::spawn_blocking(move || drop(mounted)).await;
        let hurried = self.hurried_at().is_some();
        futures::future::join_all(trees.into_iter().map(|tree| async move {
            self.pollers.remove(&tree.engine);
            if hurried {
                tree.engine.end_shutdown_wait();
            }
            if let Err(error) = tree.engine.shutdown_by(deadline).await {
                tracing::error!(tree = tree.name, %error, "could not commit a tree's store");
            }
        }))
        .await;
    }

    /// At the daemon's stop: the mounts upload until `deadline`, all at
    /// once, then each is unmounted (detached when busy) and its trees
    /// stopped. A command still running on a mount ends its step first (an
    /// unmount's drain ends now) and is waited for until shortly after the
    /// deadline; a mount it leaves mounted is stopped too. After
    /// [`Self::hurry`] nothing waits longer than [`HURRIED_WAIT`].
    pub async fn shutdown(&self, deadline: Instant) {
        *self.stop_deadline.lock() = Some(deadline);
        self.stopping.send_replace(true);
        let end = deadline.max(Instant::now()) + STOP_SESSION;
        loop {
            let taken: Vec<(usize, PathBuf, Box<Active>)> = {
                let mut mounts = self.mounts.lock();
                mounts
                    .iter_mut()
                    .enumerate()
                    .filter(|(_, mount)| matches!(mount.state, RootState::Mounted(_)))
                    .filter_map(|(index, mount)| {
                        let active = mount.begin_unmount(Leaving::Stop).ok()?;
                        Some((index, mount.mountpoint.clone(), active))
                    })
                    .collect()
            };
            if !taken.is_empty() {
                futures::future::join_all(taken.into_iter().map(
                    |(index, mountpoint, active)| async move {
                        if !active.mounted.is_finished() {
                            self.drain(&active.all_trees(), deadline, false).await;
                        }
                        detach(&mountpoint).await;
                        self.stop(active, deadline).await;
                        self.set_state(index, RootState::Unmounted { outside: false });
                    },
                ))
                .await;
                continue;
            }
            if !self.mounts.lock().iter().any(|mount| mount.state.busy())
                || Instant::now() >= self.wait_end(end)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        if !self.cleanups.wait_all(self.wait_end(end)).await {
            tracing::warn!("a gone share's cleanup did not end before the stop");
        }
        self.pollers.stop();
    }

    /// The mounts as `status` shows them; never waits for a command.
    pub fn status(&self) -> Vec<control::RootReply> {
        let waiting = self.session.borrow().is_none();
        let intervals = self.pollers.intervals();
        self.mounts
            .lock()
            .iter()
            .map(|mount| mount.reply(self.context.read_only, waiting, &intervals))
            .collect()
    }
}

/// The status line of the Photos tree: its stages (D113).
fn photos_reply(photos: &PhotosTree) -> control::TreeReply {
    let status = photos.status();
    let timeline = if status.timeline_complete {
        "timeline read"
    } else {
        "reading the timeline"
    };
    let fetching = if status.fetching > 0 {
        format!(", names of {} month(s) being fetched", status.fetching)
    } else {
        String::new()
    };
    control::TreeReply {
        name: "Photos".into(),
        writable: false,
        pending: 0,
        parked: 0,
        events_last_poll_secs: status.last_poll.map(|age| age.as_secs()),
        error: None,
        cache_bytes: photos.cache_usage().0,
        role: None,
        poll_interval_secs: Some(ReadOnlyTree::config(photos).sync.poll_interval.0.as_secs()),
        detail: Some(format!(
            "{} photos, {} named, {timeline}{fetching}",
            status.photos, status.named
        )),
        incoming: Vec::new(),
        incoming_not_read: 0,
        note: None,
    }
}

/// One tree's part of [`Roots::drain`]; `marker`: at the daemon's stop, so
/// a tree that asks for it leaves its pending marker when the grace is
/// about to run out with changes left, and trashes it again when they are
/// all uploaded before the end (D076, D096).
async fn drain_tree(tree: &OpenTree, deadline: Instant, marker: bool) {
    let engine = &tree.engine;
    let left = || deadline.saturating_duration_since(Instant::now());
    let counts = if marker && engine.pending_marker_enabled() && engine.write_gate_open() {
        let point = deadline
            .checked_sub(MARKER_WINDOW)
            .unwrap_or_else(Instant::now)
            .max(Instant::now());
        let first = engine
            .drain(point.saturating_duration_since(Instant::now()))
            .await;
        if first.pending == 0 {
            first
        } else {
            let marked = engine.leave_pending_marker(deadline).await;
            let rest = engine.drain(left()).await;
            if matches!(marked, MarkerOutcome::Written { .. }) && rest.pending == 0 {
                engine.trash_pending_marker_by(deadline).await;
            }
            rest
        }
    } else {
        engine.drain(left()).await
    };
    if counts.pending > 0 {
        tracing::warn!(
            tree = tree.name,
            pending = counts.pending,
            "unmounting with changes left; they upload at the next mount"
        );
    }
}

/// Unmounts a further mount at the daemon's stop: `fusermount3 -u`, and
/// `-uz` (a lazy detach) when it is busy, as My Files' stop does; nothing
/// when it is not mounted any more.
async fn detach(mountpoint: &Path) {
    let mountpoint = mountpoint.to_owned();
    let _ = tokio::task::spawn_blocking(move || {
        if matches!(mounts::find(&mountpoint), Ok(Some(_)))
            && crate::daemon::fusermount(&mountpoint, false).is_err()
        {
            let _ = crate::daemon::fusermount(&mountpoint, true);
        }
    })
    .await;
}

/// Before a further root mounts: a pdmount mount left at its mountpoint by
/// a daemon that crashed or was killed (its connection answers ENOTCONN) is
/// detached, as `--clear-stale` does for My Files (D093); anything else
/// mounted there refuses the mount.
async fn clear_dead_mount(mountpoint: &Path) -> anyhow::Result<()> {
    let Some(entry) = mounts::find(mountpoint)? else {
        return Ok(());
    };
    if entry.is_pdmount() {
        let target = mountpoint.to_owned();
        // Bounded: a live mount whose daemon hangs counts as live.
        let dead = tokio::task::spawn_blocking(move || mounts::is_dead(&target)).await;
        if matches!(dead, Ok(true)) {
            tracing::warn!(
                mountpoint = %mountpoint.display(),
                "removing the dead mount a stopped daemon left"
            );
            let target = mountpoint.to_owned();
            tokio::task::spawn_blocking(move || crate::daemon::fusermount(&target, true)).await??;
            return Ok(());
        }
        // Live: never detached (D127); its source names its profile.
        let whose = entry
            .profile()
            .map_or_else(String::new, |owner| format!(" (profile {owner})"));
        anyhow::bail!(
            "a live PdMount mount{whose} is already at {}; give each root its own mountpoint (D127)",
            mountpoint.display()
        );
    }
    anyhow::bail!("something is already mounted at {}", mountpoint.display())
}

/// The cleanups of gone shares that still run (D114): the wait for their
/// last event pass, the export to `orphaned/`, the store's commit and the
/// deletion of `trees/<key>/`, by tree key. A share whose cleanup runs is
/// not opened again until it ends (two engines on one store), and a stop
/// waits for them within its deadline.
#[derive(Default)]
pub(crate) struct Cleanups {
    running: parking_lot::Mutex<HashMap<String, Cleanup>>,
}

struct Cleanup {
    engine: Arc<Engine>,
    done: tokio::sync::watch::Receiver<bool>,
}

impl Cleanups {
    /// Runs `work`, the cleanup of the tree `key` whose engine is `engine`.
    pub(crate) fn spawn(
        self: &Arc<Self>,
        key: String,
        engine: Arc<Engine>,
        work: impl std::future::Future<Output = ()> + Send + 'static,
    ) {
        let (done, receiver) = tokio::sync::watch::channel(false);
        self.running.lock().insert(
            key.clone(),
            Cleanup {
                engine,
                done: receiver.clone(),
            },
        );
        let this = Arc::clone(self);
        tokio::spawn(async move {
            work.await;
            let mut running = this.running.lock();
            if running
                .get(&key)
                .is_some_and(|cleanup| cleanup.done.same_channel(&receiver))
            {
                running.remove(&key);
            }
            drop(running);
            let _ = done.send(true);
        });
    }

    /// Whether the cleanup of `key` runs.
    pub(crate) fn running(&self, key: &str) -> bool {
        self.running.lock().contains_key(key)
    }

    /// The engines being cleaned up, so a second stop signal ends their
    /// waits too.
    fn engines(&self) -> Vec<Arc<Engine>> {
        self.running
            .lock()
            .values()
            .map(|cleanup| Arc::clone(&cleanup.engine))
            .collect()
    }

    /// Waits until every cleanup running now has ended, or `until`;
    /// whether they all ended.
    pub(crate) async fn wait_all(&self, until: Instant) -> bool {
        let receivers: Vec<_> = self
            .running
            .lock()
            .values()
            .map(|cleanup| cleanup.done.clone())
            .collect();
        let all = futures::future::join_all(receivers.into_iter().map(|mut done| async move {
            // A cleanup whose task is gone has ended too.
            let _ = done.wait_for(|done| *done).await;
        }));
        tokio::time::timeout_at(until.into(), all).await.is_ok()
    }
}

/// Logs and announces what a further tree's engine reports, naming the
/// tree: other devices' pending markers (D076; one notification per pass,
/// D126) and new conflicts (§10.8). The tasks end with the engine.
fn announce_tree(name: &str, engine: &Engine) {
    tokio::spawn(crate::incoming::announce_in(
        Some(name.to_owned()),
        engine.subscribe_incoming(),
    ));
    let mut notices = engine.subscribe_conflicts();
    let name = name.to_owned();
    tokio::spawn(async move {
        use tokio::sync::broadcast::error::RecvError;
        loop {
            match notices.recv().await {
                Ok(notice) => {
                    tracing::info!(
                        tree = name,
                        id = notice.id,
                        kind = notice.kind.as_str(),
                        path = notice.path.as_deref().unwrap_or(""),
                        "conflict recorded in a further tree"
                    );
                    // The tree's name is plain text in the summary; the
                    // body may be read as markup.
                    crate::notify::desktop(
                        &format!("PdMount: conflict in {name}"),
                        "Both versions are kept; the other one is a conflict copy beside the file.",
                    );
                }
                Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => return,
            }
        }
    });
}

/// `pdmount unmount [--force] <mountpoint>` of a further root: through the
/// daemon, which uploads the mount's pending changes first; the daemon and
/// My Files go on, also under a service. `--force` detaches it at once
/// (`fusermount3 -uz`), and the daemon then stops its trees.
pub async fn unmount_root(
    paths: &ProfilePaths,
    mountpoint: &Path,
    force: bool,
) -> anyhow::Result<ExitCode> {
    let socket = paths.control_socket();
    if !force {
        let status =
            control::call_ok::<control::StatusReply>(&socket, Request::Status, CONTROL_TIMEOUT)
                .await;
        let pending = status
            .ok()
            .flatten()
            .and_then(|status| {
                status
                    .roots
                    .into_iter()
                    .find(|root| root.mountpoint == mountpoint)
            })
            .map_or(0, |root| root.trees.iter().map(|tree| tree.pending).sum());
        if pending > 0 {
            println!(
                "Uploading {pending} pending changes of {} first (up to {} min)...",
                mountpoint.display(),
                UNMOUNT_DRAIN.as_secs() / 60
            );
        }
        let request = Request::UnmountRoot {
            mountpoint: mountpoint.to_owned(),
        };
        match control::call_ok::<control::Done>(&socket, request, UNMOUNT_CALL).await {
            Ok(Some(_)) => {
                println!("Unmounted {}.", mountpoint.display());
                return Ok(ExitCode::SUCCESS);
            }
            Err(error) => {
                eprintln!("error: cannot unmount {}: {error:#}", mountpoint.display());
                let text = format!("{error:#}");
                if text.contains("busy") {
                    eprintln!("Close the files open under it, or use `pdmount unmount --force`.");
                } else if text.contains("did not answer") {
                    eprintln!("The daemon goes on; `pdmount status` shows the mount's state.");
                }
                return Ok(ExitCode::FAILURE);
            }
            // No daemon: a mount it left behind is removed below.
            Ok(None) => {}
        }
    }
    match mounts::find(mountpoint)? {
        Some(entry) if entry.is_pdmount() => {
            // Another profile's live mount is never removed (D127).
            let profile = paths.profile.as_str();
            if entry.profile().is_some_and(|owner| owner != profile) && !mounts::is_dead(mountpoint)
            {
                let text = crate::daemon::foreign_mount_text(profile, mountpoint, entry.profile());
                eprintln!("error: {text}");
                return Ok(ExitCode::FAILURE);
            }
            let target = mountpoint.to_owned();
            tokio::task::spawn_blocking(move || crate::daemon::fusermount(&target, force))
                .await??;
            println!("Removed the mount at {}.", mountpoint.display());
            Ok(ExitCode::SUCCESS)
        }
        _ => {
            println!("{} is not mounted.", mountpoint.display());
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// `pdmount mount [-o ro] <mountpoint>` of a further root: the running
/// daemon mounts it; without one, the root mounts with the daemon's start.
pub async fn mount_root(
    paths: &ProfilePaths,
    mountpoint: &Path,
    read_only: bool,
) -> anyhow::Result<ExitCode> {
    let socket = paths.control_socket();
    let request = Request::MountRoot {
        mountpoint: mountpoint.to_owned(),
        read_only,
    };
    match control::call_ok::<control::Done>(&socket, request, MOUNT_CALL).await {
        Ok(Some(_)) => {
            let ro = if read_only { " (read-only)" } else { "" };
            println!("Mounted {}{ro}.", mountpoint.display());
            Ok(ExitCode::SUCCESS)
        }
        Ok(None) => {
            eprintln!(
                "error: no daemon runs for this profile; `pdmount mount` starts it, and it mounts {} with My Files",
                mountpoint.display()
            );
            Ok(ExitCode::FAILURE)
        }
        Err(error) => {
            eprintln!("error: cannot mount {}: {error:#}", mountpoint.display());
            if format!("{error:#}").contains("did not answer") {
                eprintln!("The daemon goes on; `pdmount status` shows the mount's state.");
            }
            Ok(ExitCode::FAILURE)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_mountpoints_may_not_overlap() {
        let home = Path::new("/home/u");
        let mut config = Config::default();
        let root = |mountpoint: &str| RootConfig {
            kind: RootKind::Test,
            mountpoint: mountpoint.into(),
            read_only: false,
            pending_marker: false,
            tree: Vec::new(),
        };
        config.roots = vec![root("~/ProtonDrive-Test")];
        let my_files = Path::new("/home/u/ProtonDrive");
        check_mountpoints(&config, home, my_files).expect("apart");
        assert_eq!(
            configured(&config, home, Path::new("/home/u/ProtonDrive-Test")),
            Some(0)
        );
        assert_eq!(configured(&config, home, my_files), None);
        config.roots = vec![root("~/ProtonDrive/inner")];
        assert!(check_mountpoints(&config, home, my_files).is_err());
        config.roots = vec![root("~/a"), root("~/a/b")];
        assert!(check_mountpoints(&config, home, my_files).is_err());
        config.roots = vec![root("/home")];
        assert!(check_mountpoints(&config, home, my_files).is_err());
    }

    fn test_mount(read_only: bool) -> RootMount {
        RootMount::new(
            RootConfig {
                kind: RootKind::Test,
                mountpoint: "/m/test".into(),
                read_only,
                pending_marker: false,
                tree: Vec::new(),
            },
            PathBuf::from("/m/test"),
        )
    }

    fn error_of<T>(result: anyhow::Result<T>) -> String {
        match result {
            Ok(_) => panic!("expected a refusal"),
            Err(error) => format!("{error:#}"),
        }
    }

    #[test]
    fn a_command_is_refused_while_another_runs_on_the_mount() {
        let mut mount = test_mount(false);
        assert_eq!(
            mount.reply(false, false, &HashMap::new()).state,
            "not mounted"
        );
        assert_eq!(
            mount.reply(false, true, &HashMap::new()).state,
            "waits for the keyring"
        );
        assert!(!mount.begin_mount(None, false).expect("from unmounted"));
        assert!(mount.state.busy());
        assert_eq!(mount.reply(false, false, &HashMap::new()).state, "mounting");
        assert!(error_of(mount.begin_mount(Some(false), false)).ends_with("is being mounted"));
        assert!(error_of(mount.begin_unmount(Leaving::Command)).ends_with("is being mounted"));
        assert!(
            matches!(mount.state, RootState::Mounting),
            "a refusal changes nothing"
        );

        mount.state = RootState::Unmounting {
            trees: Vec::new(),
            why: Leaving::Command,
        };
        assert_eq!(
            mount.reply(false, false, &HashMap::new()).state,
            "unmounting: uploading first"
        );
        assert!(error_of(mount.begin_mount(None, false)).ends_with("is being unmounted"));
        assert!(error_of(mount.begin_unmount(Leaving::Command)).ends_with("is being unmounted"));
        mount.state = RootState::Unmounting {
            trees: Vec::new(),
            why: Leaving::Outside,
        };
        assert_eq!(
            mount.reply(false, false, &HashMap::new()).state,
            "stopping: unmounted from outside"
        );
        assert!(error_of(mount.begin_mount(None, false)).ends_with("is being stopped"));
        assert!(mount.take_finished(true).is_none());

        mount.state = RootState::Unmounted { outside: true };
        assert!(!mount.state.busy());
        assert_eq!(
            mount.reply(false, false, &HashMap::new()).state,
            "not mounted (unmounted from outside)"
        );
        assert!(error_of(mount.begin_unmount(Leaving::Command)).ends_with("is not mounted"));
        mount.state = RootState::Failed("no tree opened".into());
        assert_eq!(
            mount.reply(false, false, &HashMap::new()).state,
            "failed: no tree opened"
        );
        assert!(mount.reply(false, false, &HashMap::new()).trees.is_empty());
        assert!(error_of(mount.begin_unmount(Leaving::Command)).contains("its last mount failed"));
        mount
            .begin_mount(None, false)
            .expect("a failed mount can mount again");
    }

    #[test]
    fn read_only_comes_from_the_command_the_config_and_the_daemon() {
        let mut mount = test_mount(false);
        assert!(mount.begin_mount(Some(true), false).expect("ro"));
        assert!(mount.reply(false, false, &HashMap::new()).read_only);
        // The next plain mount is writable again; the daemon's -o ro holds.
        mount.state = RootState::Unmounted { outside: false };
        assert!(!mount.begin_mount(Some(false), false).expect("rw"));
        mount.state = RootState::Unmounted { outside: false };
        assert!(mount.begin_mount(Some(false), true).expect("daemon ro"));
        assert!(mount.reply(true, false, &HashMap::new()).read_only);
        // At the start (no option) the last choice stays; `read_only` in
        // the config holds against a plain mount.
        let mut mount = test_mount(true);
        assert!(mount.begin_mount(None, false).expect("config ro"));
        mount.state = RootState::Unmounted { outside: false };
        assert!(mount.begin_mount(Some(false), false).expect("config ro"));
    }

    /// D114 (review): a gone share's cleanup is kept by tree key until it
    /// ends, so a share that comes back waits for it, and a stop waits for
    /// all of them within its deadline.
    #[tokio::test]
    async fn gone_shares_cleanups_are_tracked_until_they_end() {
        let dir = tempfile::tempdir().expect("tempdir");
        let options = EngineOptions::in_dir(
            dir.path(),
            Config::default(),
            DeviceIdentity {
                id: pdmount_core::device::Uuid::from_u128(5),
                name: "cleanups-test".into(),
            },
            system_clock(),
        );
        let (engine, _invalidations) = Engine::open(Arc::new(FakeBackend::new()), options)
            .await
            .expect("engine");
        let cleanups = Arc::new(Cleanups::default());
        let (release, wait) = tokio::sync::oneshot::channel::<()>();
        cleanups.spawn("key-a".into(), Arc::clone(&engine), async move {
            let _ = wait.await;
        });
        assert!(cleanups.running("key-a"));
        assert!(!cleanups.running("key-b"));
        assert_eq!(cleanups.engines().len(), 1);
        // Not ended by the deadline: the caller is told.
        assert!(
            !cleanups
                .wait_all(Instant::now() + Duration::from_millis(50))
                .await
        );
        assert!(cleanups.running("key-a"));
        release.send(()).expect("release");
        assert!(
            cleanups
                .wait_all(Instant::now() + Duration::from_secs(5))
                .await
        );
        assert!(!cleanups.running("key-a"));
        assert!(cleanups.engines().is_empty());
        // Nothing running: no wait.
        assert!(cleanups.wait_all(Instant::now()).await);
        engine.shutdown().await.expect("shutdown");
    }

    #[test]
    fn the_unmount_call_outlasts_the_daemon_side() {
        assert!(UNMOUNT_CALL > UNMOUNT_DRAIN + STOP_GRACE + STOP_SESSION + CONTROL_TIMEOUT);
    }
}
