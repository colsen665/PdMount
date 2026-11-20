//! Conflict notifications (DESIGN.md §10.8, D077, M6.2): one desktop
//! notification per conflict the user has to look at (state `open`), with
//! the file's path in its mount, what happened in plain words, and a
//! **Show** action that opens the conflict copy's folder. `notice` conflicts
//! (the engine settled them) stay in the log and `pdmount conflicts list`.
//!
//! One [`Announcer`] per daemon takes the conflicts of every tree it
//! [follows](Announcer::follow), so the limit is the profile's: up to three
//! notifications of their own per 30 s, the rest summed in one ("5 more
//! conflicts", [`crate::notify::Burst`]). [`Announcer::conflict`] queues one
//! conflict; a conflict counts as notified when the notification service
//! took the notification that names or sums it ([`notified`]: the row's
//! `notified_at`, so it is shown at most once).
//!
//! Conflicts recorded while no notification service ran (M6.3): when the
//! daemon starts, or `notify_watch.rs` sees the service or its bus appear,
//! the `open` conflicts of every followed tree that were not shown go out,
//! each from its own store: up to three on their own, more in one summary
//! ([`missed_body`]); one resolved meanwhile is not shown. With
//! `notify.conflicts = false` they are marked shown without a notification,
//! so turning the setting on later shows nothing old. A notification the
//! service did not take while it ran (a timeout, an error) is tried again
//! [`RETRY`] later: its conflict stays unshown, held ones stay held.
//!
//! While notifications with **Show** are open, the announcer listens for
//! `ActionInvoked` and `NotificationClosed` ([`crate::notify::subscribe`]);
//! a **Show** asks the file manager on the same bus
//! (`org.freedesktop.FileManager1.ShowItems` with the copy's `file://` URI),
//! and when that call fails on a bus that passed the user check, runs
//! `xdg-open` on the copy's folder with the bus and the display of the
//! desktop's session (D102), in a process group of its own.
//!
//! `notify.conflicts = false` turns the notifications off;
//! `notify.conflict_names = false` leaves the file and the tree out of the
//! text (a lock screen). Names come from other devices (D070): control and
//! bidirectional formatting characters are replaced, and `notify.rs`
//! escapes the body for a server that reads markup.

use crate::notify::{self, Burst, Event, Handle, Notification};
use crate::notify_watch;
use pdmount_core::config::Config;
use pdmount_core::conflicts::{ConflictKind, ConflictState, ConflictView};
use pdmount_core::engine::Engine;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc};

/// The summary of one conflict's notification.
pub const SUMMARY: &str = "PdMount: conflict";
/// The summary of the notification that sums the conflicts beyond the burst.
pub const SUMMARY_MORE: &str = "PdMount: conflicts";
/// The action key of **Show**.
pub const SHOW: &str = "show";
/// Notifications of their own per [`GAP`].
const BURST: usize = 3;
const GAP: Duration = Duration::from_secs(30);
/// Conflicts held for the summary with their ids; beyond, only counted.
const MAX_HELD: usize = 256;
/// Notifications with **Show** that are remembered; the oldest go first.
const MAX_OPEN: usize = 64;
/// How long `ShowItems` may take: a file manager that D-Bus starts at the
/// call needs more than one call's 2 s.
const SHOW_TIMEOUT: Duration = Duration::from_secs(10);
const FILE_MANAGER: &str = "org.freedesktop.FileManager1";
const FILE_MANAGER_PATH: &str = "/org/freedesktop/FileManager1";

/// The `[notify]` settings for conflicts (Appendix B).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Settings {
    /// `notify.conflicts`.
    pub enabled: bool,
    /// `notify.conflict_names`.
    pub names: bool,
}

impl Settings {
    pub fn of(config: &Config) -> Self {
        Self {
            enabled: config.notify.conflicts,
            names: config.notify.conflict_names,
        }
    }
}

/// Where a tree's files are on this machine, and how its conflicts name it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Place {
    /// "My Files" or the tree's name, when the profile has several trees.
    pub label: Option<String>,
    /// The local folder the engine's paths (`/a/b`) start at.
    pub root: PathBuf,
    /// A shared single file: where it shows (its tree's paths do not).
    pub file: Option<PathBuf>,
}

impl Place {
    /// My Files at `mountpoint`: named only when the profile has further
    /// roots.
    pub fn my_files(mountpoint: &Path, several: bool) -> Self {
        Self {
            label: several.then(|| "My Files".to_owned()),
            root: mountpoint.to_owned(),
            file: None,
        }
    }

    /// A further tree `name`, a folder of the mount at `mountpoint`.
    pub fn folder(name: &str, mountpoint: &Path) -> Self {
        Self {
            label: Some(name.to_owned()),
            root: mountpoint.join(name),
            file: None,
        }
    }

    /// A further tree `name` that is its mount's only one: the mount shows
    /// it at `mountpoint` itself, without a folder of its name (a `test`
    /// root with one tree, `pdmount_fuse::mount_trees`).
    pub fn alone(name: &str, mountpoint: &Path) -> Self {
        Self {
            label: Some(name.to_owned()),
            root: mountpoint.to_owned(),
            file: None,
        }
    }

    /// A shared single file `name`, shown at `local`.
    pub fn file(name: &str, local: PathBuf) -> Self {
        Self {
            label: Some(name.to_owned()),
            root: local.clone(),
            file: Some(local),
        }
    }

    /// The local path of `path` (an engine path, `/a/b`).
    fn local(&self, path: &str) -> PathBuf {
        match &self.file {
            Some(file) => file.clone(),
            None => self.root.join(path.trim_start_matches('/')),
        }
    }
}

/// Records that conflicts of a tree were shown.
type Marker = Box<dyn Fn(&[i64]) + Send + Sync>;

/// A tree whose conflicts are announced: its place and how to look a
/// conflict up, list the ones not shown yet and mark one as shown (the
/// engine's registry and store, held weakly so that a held conflict does not
/// keep a stopped tree's engine).
pub struct Tree {
    pub place: Place,
    view: Box<dyn Fn(i64) -> Option<ConflictView> + Send + Sync>,
    /// The ids of the `open` conflicts not shown yet, oldest first; `None`
    /// once the engine is gone.
    waiting: Box<dyn Fn() -> Option<Vec<i64>> + Send + Sync>,
    mark: Marker,
}

impl Tree {
    pub fn new(place: Place, engine: &Arc<Engine>) -> Arc<Self> {
        let weak: Weak<Engine> = Arc::downgrade(engine);
        let (for_waiting, for_mark) = (weak.clone(), weak.clone());
        Arc::new(Self {
            place,
            view: Box::new(move |id| weak.upgrade()?.conflict_view(id)),
            waiting: Box::new(move || Some(for_waiting.upgrade()?.unnotified_conflicts())),
            mark: Box::new(move |ids| {
                if let Some(engine) = for_mark.upgrade() {
                    engine.mark_conflicts_notified(ids);
                }
            }),
        })
    }

    fn waiting(&self) -> Option<Vec<i64>> {
        (self.waiting)()
    }

    fn mark(&self, ids: &[i64]) {
        (self.mark)(ids);
    }

    /// The conflict `id` while it is `open` and was not shown.
    fn pending(&self, id: i64) -> Option<ConflictView> {
        let view = (self.view)(id).filter(|view| view.state == ConflictState::Open)?;
        self.waiting()?.contains(&id).then_some(view)
    }
}

// ---- The text ------------------------------------------------------------------

/// What happened, in plain words, for the kinds whose conflicts start
/// `open` (§10.8).
fn what(kind: ConflictKind) -> &'static str {
    match kind {
        ConflictKind::EditEdit => "Changed here and on another device; both copies kept.",
        ConflictKind::CreateCreate => "Created here and on another device; both copies kept.",
        ConflictKind::NameTaken => {
            "Another device created a file of this name while a file here that is not \
             uploaded had it; both copies kept, the one here renamed."
        }
        ConflictKind::LocalDeleteRemoteEdit => {
            "Deleted here but changed on another device; the changed file was brought back."
        }
        _ => "A conflict was recorded; see `pdmount conflicts list`.",
    }
}

/// `orig_name` in the folder of `copy` (both engine paths).
fn beside(copy: &str, orig_name: &str) -> String {
    match copy.rsplit_once('/') {
        Some((parent, _)) if !parent.is_empty() => format!("{parent}/{orig_name}"),
        _ => format!("/{orig_name}"),
    }
}

/// The file the notification names: the original name beside the conflict
/// copy, or the file that came back (`local_delete_remote_edit`).
fn named_path(view: &ConflictView) -> Option<String> {
    match view.kind {
        ConflictKind::LocalDeleteRemoteEdit => view.other_path.clone().or(view.path.clone()),
        _ => view
            .path
            .as_deref()
            .map(|copy| beside(copy, &view.orig_name))
            .or(view.other_path.clone()),
    }
}

/// The body of one conflict's notification.
pub fn body(place: &Place, view: &ConflictView, names: bool) -> String {
    let what = what(view.kind);
    if !names {
        return what.to_owned();
    }
    let path = match place.file {
        Some(_) => None,
        None => Some(named_path(view).unwrap_or_else(|| view.orig_name.clone())),
    };
    let location = match (&place.label, path) {
        (Some(label), Some(path)) => format!("{label}: {path}"),
        (Some(label), None) => label.clone(),
        (None, Some(path)) => path,
        (None, None) => view.orig_name.clone(),
    };
    format!("{}\n{what}", crate::notify::clean_name(&location))
}

/// The body of the notification that sums `count` conflicts.
pub fn more_body(count: u32) -> String {
    let noun = if count == 1 { "conflict" } else { "conflicts" };
    format!("{count} more {noun}; see `pdmount conflicts list`")
}

/// The body of the notification that sums `count` conflicts recorded while
/// no notification service ran (M6.3).
pub fn missed_body(count: u32) -> String {
    format!(
        "{count} conflicts were recorded while no desktop was listening; see `pdmount conflicts list`"
    )
}

/// What **Show** shows: the conflict copy, or the file that came back.
pub fn target(place: &Place, view: &ConflictView) -> Option<PathBuf> {
    let path = match view.kind {
        ConflictKind::LocalDeleteRemoteEdit => view.other_path.as_ref().or(view.path.as_ref()),
        _ => view.path.as_ref().or(view.other_path.as_ref()),
    };
    match &place.file {
        Some(file) => Some(file.clone()),
        None => path.map(|path| place.local(path)),
    }
}

/// `path` as a `file://` URI: every byte but the unreserved ones and `/`
/// percent-encoded.
pub fn file_uri(path: &Path) -> String {
    let mut uri = String::from("file://");
    for &byte in path.as_os_str().as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/') {
            uri.push(char::from(byte));
        } else {
            uri.push_str(&format!("%{byte:02X}"));
        }
    }
    uri
}

// ---- Sending ---------------------------------------------------------------------

/// Where notifications go: the routed bus (the daemon), or a fixed one
/// (unit tests on a private bus).
#[derive(Clone, Debug)]
enum Bus {
    Routed,
    #[cfg(test)]
    Fixed(String),
}

impl Bus {
    async fn send(&self, notification: &Notification) -> Option<Handle> {
        match self {
            Self::Routed => notify::send(notification).await,
            #[cfg(test)]
            Self::Fixed(address) => notify::send_to(address, notification).await.ok(),
        }
    }

    async fn has_actions(&self) -> bool {
        let caps = match self {
            Self::Routed => notify::capabilities().await,
            #[cfg(test)]
            Self::Fixed(address) => notify::capabilities_on(address).await,
        };
        caps.is_some_and(|caps| caps.actions)
    }
}

/// A sent conflict notification: its handle, and what its **Show** shows
/// (`None`: no action offered).
pub struct Sent {
    pub handle: Handle,
    pub show: Option<PathBuf>,
}

/// Sends one conflict's notification: with **Show** when there is something
/// to show and the server has actions. `None` when it did not go out (no
/// bus, no server, no answer in time).
async fn send_one(bus: &Bus, place: &Place, view: &ConflictView, names: bool) -> Option<Sent> {
    let mut notification = Notification::new(SUMMARY, body(place, view, names));
    let show = match target(place, view) {
        Some(target) if bus.has_actions().await => Some(target),
        _ => None,
    };
    if show.is_some() {
        notification = notification.action(SHOW, "Show");
    }
    let handle = bus.send(&notification).await?;
    Some(Sent { handle, show })
}

/// The conflicts `ids` of `tree` were shown: the notification service took
/// the notification that names or sums them. They are marked in the tree's
/// store (`conflicts.notified_at`, M6.3), so they are not shown again.
pub fn notified(tree: &Tree, ids: &[i64]) {
    tracing::debug!(
        tree = tree.place.label.as_deref().unwrap_or(""),
        ?ids,
        "conflict notification sent"
    );
    if !ids.is_empty() {
        tree.mark(ids);
    }
}

// ---- Show ------------------------------------------------------------------------

/// **Show**: the file manager on `bus` shows `target`, or else `opener`
/// (`xdg-open`) opens its folder, with `bus` and the desktop's display. The
/// fallback runs only when the bus, one that passed the user check (D102),
/// answered with an error (no file manager there): a bus that cannot be
/// reached, is another user's, went away during the call or did not answer
/// in time gets nothing, not a program started with its address (M6.5 V-6).
pub(crate) async fn show(target: &Path, bus: &str, opener: &Path) {
    show_within(target, bus, opener, SHOW_TIMEOUT).await;
}

/// [`show`] with `limit` for the call (shorter in tests).
async fn show_within(target: &Path, bus: &str, opener: &Path, limit: Duration) {
    let uri = file_uri(target);
    match show_items(bus, &uri, limit).await {
        Ok(()) => tracing::info!(uri, "conflict: the file manager shows the copy"),
        Err(ShowError::Call(error)) => {
            let folder = target.parent().unwrap_or(target);
            tracing::info!(
                error,
                ?folder,
                "conflict: no file manager answered ShowItems; opening the folder"
            );
            if let Err(error) = open_folder(opener, folder, bus) {
                tracing::warn!(error, "conflict: cannot open the folder");
            }
        }
        Err(ShowError::Lost(error)) => {
            tracing::warn!(error, "conflict: the bus did not answer; nothing opened");
            crate::bus::drop_if_gone_async(bus).await;
        }
        Err(ShowError::Bus(error)) => {
            tracing::warn!(error, "conflict: cannot show the copy on that bus");
        }
    }
}

/// Why `ShowItems` did not show the copy.
#[derive(Debug)]
enum ShowError {
    /// No connection: the bus cannot be reached or is not this user's.
    Bus(String),
    /// The bus is this user's and answered the call with an error (no file
    /// manager).
    Call(String),
    /// No answer: the connection failed during the call, or the call ran out
    /// of time (the bus may be gone; a file manager still starting shows the
    /// copy itself).
    Lost(String),
}

/// Whether `error` is the bus's (or a service's) error reply, as opposed to
/// a failure of the connection itself.
fn answered(error: &zbus::Error) -> bool {
    matches!(error, zbus::Error::MethodError(..) | zbus::Error::FDO(_))
}

/// `org.freedesktop.FileManager1.ShowItems([uri], "")` on `bus`, a bus of
/// this user only (`bus::connect`), within `limit`.
async fn show_items(bus: &str, uri: &str, limit: Duration) -> Result<(), ShowError> {
    let connection = crate::bus::connect_with(bus, limit)
        .await
        .map_err(ShowError::Bus)?;
    let arguments = (vec![uri], "");
    let call = connection.call_method(
        Some(FILE_MANAGER),
        FILE_MANAGER_PATH,
        Some(FILE_MANAGER),
        "ShowItems",
        &arguments,
    );
    match tokio::time::timeout(limit + Duration::from_millis(500), call).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(error)) if answered(&error) => Err(ShowError::Call(format!("ShowItems: {error}"))),
        Ok(Err(error)) => Err(ShowError::Lost(format!("ShowItems: {error}"))),
        Err(_) => Err(ShowError::Lost(format!(
            "ShowItems: no answer within {} ms",
            limit.as_millis()
        ))),
    }
}

/// `opener` as the program to run: a bare name is looked up in the
/// absolute directories of `PATH` only, so that an empty or relative entry
/// cannot pick a program from the daemon's working directory (M6.5 V-8).
fn resolve_opener(opener: &Path) -> Option<PathBuf> {
    if opener.components().count() > 1 || opener.is_absolute() {
        return Some(opener.to_owned());
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(opener))
        .find(|candidate| candidate.is_file())
}

/// Starts `opener <folder>` with `bus` as its session bus and the display
/// of the last attach (the daemon's own without one); not waited for. In a
/// process group of its own, so that a signal to the daemon's group does not
/// close the file manager it may start (the service's cgroup still holds
/// it under systemd, D139); in `/`, and without the service manager's
/// `NOTIFY_SOCKET`.
fn open_folder(opener: &Path, folder: &Path, bus: &str) -> Result<(), String> {
    let program = resolve_opener(opener)
        .ok_or_else(|| format!("{} is not in an absolute PATH directory", opener.display()))?;
    let mut command = tokio::process::Command::new(&program);
    command
        .arg(folder)
        .current_dir("/")
        .process_group(0)
        .env("DBUS_SESSION_BUS_ADDRESS", bus)
        .env_remove("NOTIFY_SOCKET")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if let Some(display) = crate::bus::desktop_display() {
        for name in crate::bus::DISPLAY_VARS {
            match display.get(name) {
                Some(value) => command.env(name, value),
                None => command.env_remove(name),
            };
        }
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("cannot run {}: {error}", opener.display()))?;
    tokio::spawn(async move {
        if let Ok(status) = child.wait().await
            && !status.success()
        {
            tracing::debug!(%status, "the folder opener failed");
        }
    });
    Ok(())
}

// ---- The announcer ---------------------------------------------------------------

enum Item {
    Conflict {
        tree: Arc<Tree>,
        id: i64,
    },
    /// Conflicts that were recorded but not seen (a lagging receiver).
    Missed(u32),
    /// A tree whose conflicts are announced from now on: the ones it still
    /// has unshown are announced when a notification service is there.
    Follow(Arc<Tree>),
    /// A notification service is available (it started, or the bus did,
    /// `notify_watch.rs`): the unshown conflicts of every tree go out.
    Up,
}

/// The daemon's conflict notifications: every followed tree's conflicts go
/// through one task, so the limits hold for the profile.
#[derive(Clone)]
pub struct Announcer {
    items: mpsc::UnboundedSender<Item>,
}

struct Options {
    settings: Settings,
    burst: usize,
    gap: Duration,
    bus: Bus,
    opener: PathBuf,
    /// The bus to watch for the notification service and how often to look
    /// again; `None`: no watching.
    watch: Option<(notify_watch::Target, Duration)>,
    /// How long after a send the service did not take everything unshown is
    /// tried again ([`RETRY`]).
    retry: Duration,
}

/// After a send the service did not take (a timeout, a hiccup), the
/// unshown conflicts and the held ones are tried again this much later,
/// whether or not the service is seen to restart.
const RETRY: Duration = Duration::from_secs(60);

/// Ends the watcher's task with the announcer's.
struct Watcher(tokio::task::JoinHandle<()>);

impl Drop for Watcher {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl Announcer {
    /// Starts the task on the current runtime; it ends when every
    /// announcer handle and follower is gone.
    pub fn start(settings: Settings) -> Self {
        Self::start_with(Options {
            settings,
            burst: BURST,
            gap: GAP,
            bus: Bus::Routed,
            opener: PathBuf::from("xdg-open"),
            watch: Some((notify_watch::Target::Routed, notify_watch::RETRY)),
            retry: RETRY,
        })
    }

    fn start_with(options: Options) -> Self {
        let (items, receiver) = mpsc::unbounded_channel();
        // With notifications off nothing is shown, so nothing is waited for.
        let watcher = options
            .watch
            .clone()
            .filter(|_| options.settings.enabled)
            .map(|(target, retry)| {
                let items = items.downgrade();
                Watcher(tokio::spawn(notify_watch::watch(
                    target,
                    retry,
                    move || {
                        if let Some(items) = items.upgrade() {
                            let _ = items.send(Item::Up);
                        }
                    },
                )))
            });
        tokio::spawn(run(options, receiver, watcher));
        Self { items }
    }

    /// Announces a conflict of `tree`, if it is still `open` and unshown
    /// when its turn comes.
    pub fn conflict(&self, tree: &Arc<Tree>, id: i64) {
        let _ = self.items.send(Item::Conflict {
            tree: Arc::clone(tree),
            id,
        });
    }

    /// Logs every new conflict of `engine` and announces it, until the
    /// engine is gone (the task's handle); the conflicts it has already
    /// that were never shown (recorded while no desktop ran) are announced
    /// when a notification service is there (M6.3). `further`: the further
    /// tree's name (its log line names the tree); `None` for My Files.
    pub fn follow(
        &self,
        further: Option<&str>,
        place: Place,
        engine: &Arc<Engine>,
    ) -> tokio::task::JoinHandle<()> {
        use tokio::sync::broadcast::error::RecvError;
        let mut notices = engine.subscribe_conflicts();
        let tree = Tree::new(place, engine);
        let _ = self.items.send(Item::Follow(Arc::clone(&tree)));
        let further = further.map(str::to_owned);
        let this = self.clone();
        tokio::spawn(async move {
            loop {
                match notices.recv().await {
                    Ok(notice) => {
                        match &further {
                            None => tracing::info!(
                                id = notice.id,
                                kind = notice.kind.as_str(),
                                path = notice.path.as_deref().unwrap_or(""),
                                "conflict recorded; see `pdmount conflicts show {}`",
                                notice.id
                            ),
                            Some(name) => tracing::info!(
                                tree = name,
                                id = notice.id,
                                kind = notice.kind.as_str(),
                                path = notice.path.as_deref().unwrap_or(""),
                                "conflict recorded in a further tree"
                            ),
                        }
                        // Id 0: a newer remote revision, not yet a conflict.
                        if notice.id != 0 && notice.kind.initial_state() == ConflictState::Open {
                            this.conflict(&tree, notice.id);
                        }
                    }
                    Err(RecvError::Lagged(missed)) => {
                        tracing::info!(missed, "conflicts recorded; see `pdmount conflicts list`");
                        let missed = u32::try_from(missed).unwrap_or(u32::MAX);
                        let _ = this.items.send(Item::Missed(missed));
                    }
                    Err(RecvError::Closed) => return,
                }
            }
        })
    }
}

/// A notification with **Show** that is still on screen.
struct Open {
    handle: Handle,
    target: PathBuf,
}

/// What the announcer's task keeps between items.
struct State {
    burst: Burst,
    /// Conflicts held beyond the burst, for the summary.
    held: Vec<(Arc<Tree>, i64)>,
    /// Conflicts counted for the summary without an id.
    missed: u32,
    open: Vec<Open>,
    /// Subscribed only while notifications with Show are open.
    events: Option<broadcast::Receiver<Event>>,
    /// The followed trees, whose unshown conflicts a service that appears
    /// is told about.
    trees: Vec<Arc<Tree>>,
    /// A notification service was available the last time the watcher
    /// looked.
    up: bool,
    /// When to try again after a send the service did not take; the held
    /// ones wait for it.
    retry: Option<Instant>,
}

impl State {
    fn is_held(&self, tree: &Arc<Tree>, id: i64) -> bool {
        self.held
            .iter()
            .any(|(held, held_id)| Arc::ptr_eq(held, tree) && *held_id == id)
    }

    /// Holds a conflict for the summary (only counted beyond [`MAX_HELD`]).
    fn hold(&mut self, tree: Arc<Tree>, id: i64) {
        if self.held.len() < MAX_HELD {
            self.held.push((tree, id));
        } else {
            self.missed = self.missed.saturating_add(1);
        }
    }

    /// Adds `tree` to the followed ones, once.
    fn remember(&mut self, tree: &Arc<Tree>) {
        if !self.trees.iter().any(|known| Arc::ptr_eq(known, tree)) {
            self.trees.push(Arc::clone(tree));
        }
    }

    /// A send the service did not take: everything unshown is tried again
    /// after [`Options::retry`], unless the service shows up first.
    fn failed(&mut self, options: &Options) {
        if self.retry.is_none() {
            tracing::debug!(
                after_s = options.retry.as_secs(),
                "a conflict notification did not go out; trying again later"
            );
            self.retry = Some(Instant::now() + options.retry);
        }
    }

    /// One conflict: shown now when the burst allows, else held for the
    /// summary. One shown already, resolved, or held is skipped. With
    /// notifications off it is only marked, so turning them on later shows
    /// nothing old.
    async fn conflict(&mut self, options: &Options, tree: Arc<Tree>, id: i64) {
        if tree.pending(id).is_none() {
            return;
        }
        if !options.settings.enabled {
            tree.mark(&[id]);
            return;
        }
        if self.is_held(&tree, id) {
            return;
        }
        if self.burst.admit(Instant::now()) {
            match announce(options, &tree, id, &mut self.open, &mut self.events).await {
                Outcome::Shown => {}
                Outcome::Settled => self.burst.retract(),
                // It leaves the window and stays unshown in its store: the
                // retry's catch-up finds it there.
                Outcome::Failed => {
                    self.burst.retract();
                    self.remember(&tree);
                    self.failed(options);
                }
            }
        } else {
            self.hold(tree, id);
        }
    }

    /// A tree to follow: with notifications off its unshown conflicts are
    /// marked; with a service up they are announced now.
    async fn follow(&mut self, options: &Options, tree: Arc<Tree>) {
        self.remember(&tree);
        if !options.settings.enabled {
            if let Some(ids) = tree.waiting() {
                tree.mark(&ids);
            }
        } else if self.up {
            self.catch_up(options, &[tree]).await;
        }
    }

    /// With notifications off, a lagging receiver's missed conflicts are
    /// marked like the others: every followed tree's unshown ones.
    fn mark_all(&mut self) {
        self.trees.retain(|tree| match tree.waiting() {
            Some(ids) => {
                tree.mark(&ids);
                true
            }
            None => false,
        });
    }

    /// A notification service is available: every followed tree's unshown
    /// conflicts go out. Trees whose engine is gone are forgotten.
    async fn appeared(&mut self, options: &Options) {
        self.up = true;
        self.retry = None;
        self.again(options).await;
    }

    /// Every followed tree's unshown conflicts go out (a service appeared,
    /// or a retry is due); the held ones follow at their deadline.
    async fn again(&mut self, options: &Options) {
        self.trees.retain(|tree| tree.waiting().is_some());
        let trees = self.trees.clone();
        self.catch_up(options, &trees).await;
    }

    /// The held conflicts and the missed count, once the window has room: in
    /// one notification, or the one still open on its own. When the service
    /// does not take it they stay held, and are tried again at the retry.
    async fn flush_held(&mut self, options: &Options) {
        let Some(count) = self.burst.flush(Instant::now()) else {
            return;
        };
        match announce_held(
            options,
            &self.held,
            self.missed,
            &mut self.open,
            &mut self.events,
        )
        .await
        {
            Outcome::Shown => {
                self.held.clear();
                self.missed = 0;
            }
            Outcome::Settled => {
                self.burst.retract();
                self.held.clear();
                self.missed = 0;
            }
            Outcome::Failed => {
                self.burst.retract();
                self.burst.hold(count);
                self.failed(options);
            }
        }
    }

    /// The open, unshown conflicts of `trees`: up to the burst each in its
    /// own notification, more in one that sums them (`missed_body`).
    async fn catch_up(&mut self, options: &Options, trees: &[Arc<Tree>]) {
        let mut pending: Vec<(Arc<Tree>, i64)> = Vec::new();
        for tree in trees {
            for id in tree.waiting().unwrap_or_default() {
                if !self.is_held(tree, id) {
                    pending.push((Arc::clone(tree), id));
                }
            }
        }
        if pending.is_empty() {
            return;
        }
        tracing::info!(
            count = pending.len(),
            "conflicts recorded while no notification service ran are announced"
        );
        let count = u32::try_from(pending.len()).unwrap_or(u32::MAX);
        if pending.len() > options.burst {
            if !self.burst.admit(Instant::now()) {
                // No room: the refused admit counts one held, the others
                // are held with it, so the burst's count is theirs.
                self.burst.hold(count.saturating_sub(1));
                for (tree, id) in pending {
                    self.hold(tree, id);
                }
                return;
            }
            let notification = Notification::new(SUMMARY_MORE, missed_body(count));
            if options.bus.send(&notification).await.is_some() {
                for tree in trees {
                    let ids: Vec<i64> = pending
                        .iter()
                        .filter(|(pending, _)| Arc::ptr_eq(pending, tree))
                        .map(|(_, id)| *id)
                        .collect();
                    notified(tree, &ids);
                }
            } else {
                self.burst.retract();
                self.failed(options);
            }
            return;
        }
        for (tree, id) in pending {
            self.conflict(options, tree, id).await;
        }
    }
}

/// The announcer's task.
async fn run(
    options: Options,
    mut items: mpsc::UnboundedReceiver<Item>,
    _watcher: Option<Watcher>,
) {
    let mut state = State {
        burst: Burst::new(options.burst, options.gap),
        held: Vec::new(),
        missed: 0,
        open: Vec::new(),
        events: None,
        trees: Vec::new(),
        up: false,
        retry: None,
    };
    loop {
        // While a retry is due, the held ones wait for it.
        let due = match state.retry {
            Some(_) => None,
            None => state.burst.deadline(),
        };
        let held_due = sleep_until(due);
        let retry_due = sleep_until(state.retry);
        tokio::select! {
            item = items.recv() => match item {
                None => return,
                Some(Item::Conflict { tree, id }) => state.conflict(&options, tree, id).await,
                Some(Item::Follow(tree)) => state.follow(&options, tree).await,
                Some(Item::Up) => state.appeared(&options).await,
                Some(Item::Missed(_)) if !options.settings.enabled => state.mark_all(),
                Some(Item::Missed(count)) => {
                    state.burst.hold(count);
                    state.missed = state.missed.saturating_add(count);
                }
            },
            () = held_due => state.flush_held(&options).await,
            () = retry_due => {
                state.retry = None;
                state.again(&options).await;
            }
            event = next_event(&mut state.events) => match event {
                // Once per notification: a server shows it once, and a
                // second signal (another process of the session) opens
                // nothing more (M6.5 V-9).
                Ok(Event::Action { handle, key }) if key == SHOW => {
                    if let Some(at) = state.open.iter().position(|open| open.handle == handle) {
                        let shown = state.open.remove(at);
                        if state.open.is_empty() {
                            state.events = None;
                        }
                        let opener = options.opener.clone();
                        tokio::spawn(async move { show(&shown.target, &handle.bus, &opener).await });
                    }
                }
                Ok(Event::Closed { handle, .. }) => {
                    state.open.retain(|open| open.handle != handle);
                    if state.open.is_empty() {
                        state.events = None;
                    }
                }
                Ok(Event::Action { .. }) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => state.events = None,
            },
        }
    }
}

/// Sleeps until `due`; never without one.
async fn sleep_until(due: Option<Instant>) {
    match due {
        Some(due) => tokio::time::sleep_until(tokio::time::Instant::from_std(due)).await,
        None => std::future::pending().await,
    }
}

/// The next signal while subscribed; never without a subscription.
async fn next_event(
    events: &mut Option<broadcast::Receiver<Event>>,
) -> Result<Event, broadcast::error::RecvError> {
    match events.as_mut() {
        Some(events) => events.recv().await,
        None => std::future::pending().await,
    }
}

/// What became of a notification.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    /// The service took it.
    Shown,
    /// Nothing to send: the conflicts were settled or shown meanwhile.
    Settled,
    /// The service did not take it (none, a timeout, an error).
    Failed,
}

/// Sends conflict `id`'s notification and remembers its **Show**.
async fn announce(
    options: &Options,
    tree: &Arc<Tree>,
    id: i64,
    open: &mut Vec<Open>,
    events: &mut Option<broadcast::Receiver<Event>>,
) -> Outcome {
    let Some(view) = tree.pending(id) else {
        return Outcome::Settled;
    };
    // Subscribed before the send, so an early click is not missed.
    if events.is_none() {
        *events = Some(notify::subscribe());
    }
    let went_out = match send_one(&options.bus, &tree.place, &view, options.settings.names).await {
        Some(sent) => {
            notified(tree, &[id]);
            if let Some(target) = sent.show {
                if open.len() >= MAX_OPEN {
                    open.remove(0);
                }
                open.push(Open {
                    handle: sent.handle,
                    target,
                });
            }
            Outcome::Shown
        }
        None => {
            tracing::debug!(id, "the conflict's notification did not go out");
            Outcome::Failed
        }
    };
    if open.is_empty() {
        *events = None;
    }
    went_out
}

/// The conflicts held beyond the burst: the one still open on its own, or
/// a sum of them (with the missed ones, whose state is not known).
async fn announce_held(
    options: &Options,
    held: &[(Arc<Tree>, i64)],
    missed: u32,
    open: &mut Vec<Open>,
    events: &mut Option<broadcast::Receiver<Event>>,
) -> Outcome {
    let still: Vec<&(Arc<Tree>, i64)> = held
        .iter()
        .filter(|(tree, id)| tree.pending(*id).is_some())
        .collect();
    let count = u32::try_from(still.len())
        .unwrap_or(u32::MAX)
        .saturating_add(missed);
    match (count, still.first()) {
        (0, _) => Outcome::Settled,
        (1, Some((tree, id))) => announce(options, tree, *id, open, events).await,
        _ => {
            let notification = Notification::new(SUMMARY_MORE, more_body(count));
            if options.bus.send(&notification).await.is_none() {
                return Outcome::Failed;
            }
            for (tree, id) in still {
                notified(tree, &[*id]);
            }
            Outcome::Shown
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::tests::{PrivateBus, private_bus};
    use crate::notify_stub::{Options as StubOptions, connect, connect_file_manager};
    use std::collections::HashMap;

    fn view(kind: ConflictKind, path: Option<&str>, other: Option<&str>) -> ConflictView {
        ConflictView {
            id: 7,
            kind,
            state: kind.initial_state(),
            path: path.map(str::to_owned),
            other_path: other.map(str::to_owned),
            orig_name: "report.odt".into(),
            conflict_name: Some("report (conflict laptop 2026-11-20 10-00-00 UTC).odt".into()),
            local_rev: None,
            remote_rev: None,
            local_device: "laptop".into(),
            created_at: 0,
            detail: None,
        }
    }

    const COPY: &str = "/docs/report (conflict laptop 2026-11-20 10-00-00 UTC).odt";

    #[test]
    fn the_body_names_the_file_and_says_what_happened() {
        let mine = Place::my_files(Path::new("/m"), false);
        let edit = view(ConflictKind::EditEdit, Some(COPY), Some("/docs/report.odt"));
        assert_eq!(
            body(&mine, &edit, true),
            "/docs/report.odt\nChanged here and on another device; both copies kept."
        );
        assert_eq!(
            body(&mine, &edit, false),
            "Changed here and on another device; both copies kept.",
            "conflict_names = false leaves the file out"
        );
        let several = Place::my_files(Path::new("/m"), true);
        assert!(body(&several, &edit, true).starts_with("My Files: /docs/report.odt\n"));
        let team = Place::folder("Team", Path::new("/s"));
        assert!(body(&team, &edit, true).starts_with("Team: /docs/report.odt\n"));
        assert!(!body(&team, &edit, false).contains("Team"));

        let create = view(
            ConflictKind::CreateCreate,
            Some("/report (conflict x).odt"),
            None,
        );
        assert_eq!(
            body(&mine, &create, true),
            "/report.odt\nCreated here and on another device; both copies kept."
        );
        let back = view(
            ConflictKind::LocalDeleteRemoteEdit,
            None,
            Some("/a/report.odt"),
        );
        assert_eq!(
            body(&mine, &back, true),
            "/a/report.odt\nDeleted here but changed on another device; the changed file was brought back."
        );
        let taken = view(ConflictKind::NameTaken, Some(COPY), None);
        assert!(body(&mine, &taken, true).starts_with("/docs/report.odt\nAnother device created"));
        // Both copies gone: the name alone.
        let gone = view(ConflictKind::EditEdit, None, None);
        assert!(body(&mine, &gone, true).starts_with("report.odt\n"));
        // A shared file: its name, which is the label.
        let file = Place::file("plan.txt", PathBuf::from("/s/Shared files/plan.txt"));
        assert!(body(&file, &edit, true).starts_with("plan.txt\n"));
    }

    #[test]
    fn names_from_other_devices_cannot_break_the_text() {
        let mine = Place::my_files(Path::new("/m"), false);
        let mut odd = view(ConflictKind::EditEdit, Some("/a\nb/x"), None);
        odd.orig_name = "evil\u{202e}txt.exe\u{1b}[31m".into();
        let text = body(&mine, &odd, true);
        let first = text.lines().next().expect("a line");
        assert_eq!(first, "/a\u{fffd}b/evil\u{fffd}txt.exe\u{fffd}[31m");
        assert_eq!(text.lines().count(), 2);
    }

    #[test]
    fn show_targets_the_copy_in_its_mount() {
        let mine = Place::my_files(Path::new("/m"), false);
        let edit = view(ConflictKind::EditEdit, Some(COPY), Some("/docs/report.odt"));
        assert_eq!(
            target(&mine, &edit),
            Some(PathBuf::from(
                "/m/docs/report (conflict laptop 2026-11-20 10-00-00 UTC).odt"
            ))
        );
        let team = Place::folder("Team", Path::new("/s"));
        assert_eq!(
            target(&team, &view(ConflictKind::CreateCreate, Some("/x"), None)),
            Some(PathBuf::from("/s/Team/x"))
        );
        // A mount's only tree is at the mountpoint, named all the same.
        let alone = Place::alone("Team", Path::new("/s"));
        assert_eq!(
            target(&alone, &view(ConflictKind::CreateCreate, Some("/x"), None)),
            Some(PathBuf::from("/s/x"))
        );
        assert!(body(&alone, &edit, true).starts_with("Team: /docs/report.odt\n"));
        let back = view(ConflictKind::LocalDeleteRemoteEdit, None, Some("/a/b"));
        assert_eq!(target(&mine, &back), Some(PathBuf::from("/m/a/b")));
        assert_eq!(
            target(&mine, &view(ConflictKind::EditEdit, None, None)),
            None
        );
        assert_eq!(
            file_uri(Path::new("/m/a <b>&/x y%.txt")),
            "file:///m/a%20%3Cb%3E%26/x%20y%25.txt"
        );
        assert_eq!(file_uri(Path::new("/m/ä")), "file:///m/%C3%A4");
    }

    #[test]
    fn the_sum_says_how_many() {
        assert_eq!(
            more_body(1),
            "1 more conflict; see `pdmount conflicts list`"
        );
        assert_eq!(
            more_body(5),
            "5 more conflicts; see `pdmount conflicts list`"
        );
    }

    // ---- On a private bus with the stub ----

    fn bus() -> Option<(tempfile::TempDir, PrivateBus)> {
        let Some(dbus_daemon) = crate::doctor::find_in_path("dbus-daemon") else {
            eprintln!("dbus-daemon not installed; skipped");
            return None;
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let bus = private_bus(&dbus_daemon, dir.path());
        Some((dir, bus))
    }

    fn lines(log: &Path, op: &str) -> Vec<serde_json::Value> {
        std::fs::read_to_string(log)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter(|line| line["op"] == op)
            .collect()
    }

    async fn wait_lines(log: &Path, op: &str, count: usize) -> Vec<serde_json::Value> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let found = lines(log, op);
            if found.len() >= count || Instant::now() > deadline {
                return found;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// The ids a fixed tree was told were shown.
    type Marks = Arc<parking_lot::Mutex<Vec<i64>>>;

    /// A tree's conflicts, by id; a test may add one later.
    type Views = Arc<parking_lot::Mutex<HashMap<i64, ConflictView>>>;

    /// A tree of fixed conflicts, by id, and the ids marked as shown.
    fn fixed_tree(root: &Path, views: Vec<ConflictView>) -> (Arc<Tree>, Marks) {
        let (tree, marks, _) = growing_tree(root, views);
        (tree, marks)
    }

    /// [`fixed_tree`], with the conflicts, to which a test adds.
    fn growing_tree(root: &Path, views: Vec<ConflictView>) -> (Arc<Tree>, Marks, Views) {
        let views: Views = Arc::new(parking_lot::Mutex::new(
            views.into_iter().map(|v| (v.id, v)).collect(),
        ));
        let marks = Marks::default();
        let (for_waiting, for_mark, for_view) =
            (Arc::clone(&views), Arc::clone(&marks), Arc::clone(&views));
        let marked = Arc::clone(&marks);
        let tree = Arc::new(Tree {
            place: Place::my_files(root, false),
            view: Box::new(move |id| for_view.lock().get(&id).cloned()),
            waiting: Box::new(move || {
                let marks = marked.lock();
                let mut ids: Vec<i64> = for_waiting
                    .lock()
                    .values()
                    .filter(|v| v.state == ConflictState::Open && !marks.contains(&v.id))
                    .map(|v| v.id)
                    .collect();
                ids.sort_unstable();
                Some(ids)
            }),
            mark: Box::new(move |ids| for_mark.lock().extend_from_slice(ids)),
        });
        (tree, marks, views)
    }

    /// A tree of fixed conflicts, by id.
    fn tree(root: &Path, views: Vec<ConflictView>) -> Arc<Tree> {
        fixed_tree(root, views).0
    }

    fn numbered(id: i64, kind: ConflictKind, path: &str) -> ConflictView {
        ConflictView {
            id,
            ..view(kind, Some(path), None)
        }
    }

    fn options(bus: &PrivateBus, opener: PathBuf, names: bool) -> Options {
        Options {
            settings: Settings {
                enabled: true,
                names,
            },
            burst: 3,
            gap: Duration::from_secs(2),
            bus: Bus::Fixed(bus.address.clone()),
            opener,
            watch: None,
            retry: Duration::from_millis(500),
        }
    }

    /// `options`, watching `bus` for the service every 100 ms.
    fn watching(bus: &PrivateBus) -> Options {
        Options {
            watch: Some((
                notify_watch::Target::Fixed(bus.address.clone()),
                Duration::from_millis(100),
            )),
            ..options(bus, PathBuf::from("false"), true)
        }
    }

    async fn marked(marks: &Marks, count: usize) -> Vec<i64> {
        let deadline = Instant::now() + Duration::from_secs(10);
        while marks.lock().len() < count && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let mut ids = marks.lock().clone();
        ids.sort_unstable();
        ids
    }

    fn follow(announcer: &Announcer, tree: &Arc<Tree>) {
        announcer
            .items
            .send(Item::Follow(Arc::clone(tree)))
            .expect("the announcer runs");
    }

    /// Open ones are named (escaped), notices are not shown, the fourth and
    /// fifth are summed after the gap, and Show reaches the file manager.
    #[tokio::test]
    async fn open_conflicts_are_announced_three_at_a_time_and_shown() {
        let Some((dir, bus)) = bus() else { return };
        let log = dir.path().join("stub.log");
        let _stub = connect(Some(&bus.address), &log, StubOptions::default())
            .await
            .expect("stub");
        let fm_log = dir.path().join("fm.log");
        let _fm = connect_file_manager(Some(&bus.address), &fm_log)
            .await
            .expect("file manager");
        let root = dir.path().join("mnt");
        let tree = tree(
            &root,
            vec![
                ConflictView {
                    orig_name: "a <b>&.txt".into(),
                    ..numbered(1, ConflictKind::EditEdit, "/a <b>& (conflict x).txt")
                },
                numbered(2, ConflictKind::RemoteDeleteLocalEdit, "/notice"),
                numbered(3, ConflictKind::CreateCreate, "/c3 (conflict x)"),
                numbered(4, ConflictKind::CreateCreate, "/c4 (conflict x)"),
                numbered(5, ConflictKind::CreateCreate, "/c5 (conflict x)"),
                numbered(6, ConflictKind::CreateCreate, "/c6 (conflict x)"),
            ],
        );
        let announcer = Announcer::start_with(options(&bus, PathBuf::from("false"), true));
        for id in 1..=6 {
            announcer.conflict(&tree, id);
        }
        let sent = wait_lines(&log, "notify", 3).await;
        assert_eq!(sent.len(), 3, "{sent:?}");
        assert_eq!(sent[0]["summary"], SUMMARY);
        assert_eq!(
            sent[0]["body"],
            "/a &lt;b&gt;&amp;.txt\nChanged here and on another device; both copies kept."
        );
        assert_eq!(sent[0]["actions"], serde_json::json!(["show", "Show"]));
        assert!(sent[1]["body"].as_str().unwrap().contains("Created here"));

        // After the gap: 5 and 6 in one.
        let sent = wait_lines(&log, "notify", 4).await;
        assert_eq!(sent.len(), 4, "{sent:?}");
        assert_eq!(sent[3]["summary"], SUMMARY_MORE);
        assert_eq!(sent[3]["body"], more_body(2));
        assert_eq!(sent[3]["actions"], serde_json::json!([]));

        // Show on the first: the file manager gets the copy's URI.
        let id = u32::try_from(sent[0]["id"].as_u64().unwrap()).unwrap();
        crate::notify_stub::emit(Some(bus.address.clone()), id, Some(SHOW.into()), 0)
            .await
            .expect("emit");
        let shown = wait_lines(&fm_log, "show_items", 1).await;
        assert_eq!(
            shown[0]["uris"],
            serde_json::json!([file_uri(&root.join("a <b>& (conflict x).txt"))])
        );
    }

    /// A name with bidirectional formatting characters reaches the server
    /// with them replaced (`notify::clean_name`), and escaped.
    #[tokio::test]
    async fn a_bidi_name_is_cleaned_on_the_bus() {
        let Some((dir, bus)) = bus() else { return };
        let log = dir.path().join("stub.log");
        let _stub = connect(Some(&bus.address), &log, StubOptions::default())
            .await
            .expect("stub");
        let tree = tree(
            &dir.path().join("mnt"),
            vec![ConflictView {
                orig_name: "<i>invoice\u{202e}fdp\u{2066}.exe".into(),
                ..numbered(1, ConflictKind::EditEdit, "/d\u{200f}ir/x (conflict x)")
            }],
        );
        let announcer = Announcer::start_with(options(&bus, PathBuf::from("false"), true));
        announcer.conflict(&tree, 1);
        let sent = wait_lines(&log, "notify", 1).await;
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert_eq!(
            sent[0]["body"],
            "/d\u{fffd}ir/&lt;i&gt;invoice\u{fffd}fdp\u{fffd}.exe\n\
             Changed here and on another device; both copies kept."
        );
    }

    /// Without a file manager, Show runs the opener on the copy's folder
    /// with the notification's bus; a closed notification no longer shows.
    #[tokio::test]
    async fn show_falls_back_to_the_opener() {
        let Some((dir, bus)) = bus() else { return };
        let log = dir.path().join("stub.log");
        let _stub = connect(Some(&bus.address), &log, StubOptions::default())
            .await
            .expect("stub");
        let opened = dir.path().join("opened");
        let opener = dir.path().join("opener");
        std::fs::write(
            &opener,
            format!(
                "#!/bin/sh\nprintf '%s %s\\n' \"$1\" \"$DBUS_SESSION_BUS_ADDRESS\" >>'{}'\n",
                opened.display()
            ),
        )
        .expect("opener");
        std::fs::set_permissions(&opener, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .expect("mode");
        let root = dir.path().join("mnt");
        let tree = tree(
            &root,
            vec![
                numbered(1, ConflictKind::EditEdit, "/d/one (conflict x)"),
                numbered(2, ConflictKind::EditEdit, "/d/two (conflict x)"),
            ],
        );
        let announcer = Announcer::start_with(options(&bus, opener, false));
        announcer.conflict(&tree, 1);
        announcer.conflict(&tree, 2);
        let sent = wait_lines(&log, "notify", 2).await;
        assert_eq!(sent.len(), 2);
        assert_eq!(
            sent[0]["body"], "Changed here and on another device; both copies kept.",
            "no names"
        );
        let first = u32::try_from(sent[0]["id"].as_u64().unwrap()).unwrap();
        let second = u32::try_from(sent[1]["id"].as_u64().unwrap()).unwrap();
        // The second is closed: its Show is forgotten.
        crate::notify_stub::emit(Some(bus.address.clone()), second, None, 2)
            .await
            .expect("emit");
        tokio::time::sleep(Duration::from_millis(300)).await;
        crate::notify_stub::emit(Some(bus.address.clone()), second, Some(SHOW.into()), 0)
            .await
            .expect("emit");
        // Twice: the second opens nothing (once per notification).
        for _ in 0..2 {
            crate::notify_stub::emit(Some(bus.address.clone()), first, Some(SHOW.into()), 0)
                .await
                .expect("emit");
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while !opened.exists() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        let text = std::fs::read_to_string(&opened).expect("the opener ran");
        assert_eq!(
            text,
            format!("{} {}\n", root.join("d").display(), bus.address),
            "only the open one's folder, once"
        );
    }

    /// A file manager whose `ShowItems` never answers.
    struct Hung;

    #[zbus::interface(name = "org.freedesktop.FileManager1")]
    impl Hung {
        async fn show_items(&self, _uris: Vec<String>, _startup_id: String) {
            std::future::pending::<()>().await;
        }
    }

    /// An opener that appends its argument to `opened`.
    fn opener_in(dir: &Path, opened: &Path) -> PathBuf {
        let opener = dir.join("opener");
        std::fs::write(
            &opener,
            format!("#!/bin/sh\necho \"$1\" >>'{}'\n", opened.display()),
        )
        .expect("opener");
        std::fs::set_permissions(&opener, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .expect("mode");
        opener
    }

    /// A call that runs out of time is no answer: the bus may be gone (a
    /// logout), so no opener starts with its address (M6.5 V-6).
    #[tokio::test]
    async fn show_opens_nothing_when_the_bus_does_not_answer() {
        let Some((dir, bus)) = bus() else { return };
        let _hung = zbus::connection::Builder::address(bus.address.as_str())
            .expect("address")
            .serve_at(FILE_MANAGER_PATH, Hung)
            .expect("serve")
            .name(FILE_MANAGER)
            .expect("name")
            .build()
            .await
            .expect("file manager");
        let opened = dir.path().join("opened");
        let opener = opener_in(dir.path(), &opened);
        show_within(
            &dir.path().join("d/copy"),
            &bus.address,
            &opener,
            Duration::from_millis(300),
        )
        .await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!opened.exists(), "the opener ran");
    }

    /// Only an error reply counts as the bus's answer.
    #[test]
    fn only_an_error_reply_is_an_answer() {
        let unknown = zbus::Error::FDO(Box::new(zbus::fdo::Error::ServiceUnknown("x".into())));
        assert!(answered(&unknown));
        let closed = zbus::Error::InputOutput(Arc::new(std::io::Error::from(
            std::io::ErrorKind::BrokenPipe,
        )));
        assert!(!answered(&closed));
    }

    /// A bare opener name is looked up in absolute `PATH` entries only; a
    /// path is taken as it is.
    #[test]
    fn the_opener_comes_from_an_absolute_path_directory() {
        let given = Path::new("/usr/bin/xdg-open");
        assert_eq!(resolve_opener(given).as_deref(), Some(given));
        let relative = Path::new("./opener");
        assert_eq!(resolve_opener(relative).as_deref(), Some(relative));
        // `sh` is in an absolute directory of the test's PATH.
        let sh = resolve_opener(Path::new("sh")).expect("sh");
        assert!(sh.is_absolute(), "{}", sh.display());
        assert_eq!(resolve_opener(Path::new("pdmount-no-such-opener")), None);
    }

    /// A bus that cannot be used (unreachable here; another user's is the
    /// same error kind, `bus::connect`) gets no `xdg-open` with its address.
    #[tokio::test]
    async fn show_runs_nothing_on_a_bus_it_cannot_use() {
        let dir = tempfile::tempdir().expect("tempdir");
        let opened = dir.path().join("opened");
        let opener = dir.path().join("opener");
        std::fs::write(
            &opener,
            format!("#!/bin/sh\necho \"$1\" >>'{}'\n", opened.display()),
        )
        .expect("opener");
        std::fs::set_permissions(&opener, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .expect("mode");
        let address = format!("unix:path={}", dir.path().join("none").display());
        show(&dir.path().join("d/copy"), &address, &opener).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!opened.exists(), "the opener ran");
    }

    /// A notification the service did not answer (a call lost while it
    /// runs) is tried again after the retry time, with no restart of the
    /// service to set it off.
    #[tokio::test]
    async fn a_send_the_service_dropped_is_tried_again() {
        let Some((dir, bus)) = bus() else { return };
        let log = dir.path().join("stub.log");
        let stub_options = StubOptions {
            drop_first: 1,
            ..StubOptions::default()
        };
        let _stub = connect(Some(&bus.address), &log, stub_options)
            .await
            .expect("stub");
        let (tree, marks) = fixed_tree(
            dir.path(),
            vec![numbered(1, ConflictKind::EditEdit, "/a (conflict x)")],
        );
        let announcer = Announcer::start_with(options(&bus, PathBuf::from("false"), true));
        follow(&announcer, &tree);
        announcer.conflict(&tree, 1);
        // The first call times out (2.5 s), the retry comes 0.5 s later.
        assert_eq!(marked(&marks, 1).await, [1]);
        assert_eq!(lines(&log, "dropped").len(), 1);
        let sent = lines(&log, "notify");
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert!(sent[0]["body"].as_str().unwrap().contains("Changed here"));
    }

    /// A sum the service did not take keeps its conflicts and the missed
    /// count: they go out together once the service is back.
    #[tokio::test]
    async fn a_sum_the_service_did_not_take_is_kept() {
        let Some((dir, bus)) = bus() else { return };
        let log = dir.path().join("stub.log");
        let stub = connect(Some(&bus.address), &log, StubOptions::default())
            .await
            .expect("stub");
        let views = (1..=3)
            .map(|id| numbered(id, ConflictKind::EditEdit, &format!("/f{id} (conflict x)")))
            .collect();
        let (tree, marks) = fixed_tree(dir.path(), views);
        let announcer = Announcer::start_with(Options {
            burst: 1,
            gap: Duration::from_secs(1),
            ..options(&bus, PathBuf::from("false"), true)
        });
        for id in 1..=3 {
            announcer.conflict(&tree, id);
        }
        assert_eq!(marked(&marks, 1).await, [1]);
        // The service stops before the sum of 2 and 3 (and 2 missed) is due.
        drop(stub);
        announcer.items.send(Item::Missed(2)).expect("runs");
        tokio::time::sleep(Duration::from_millis(1800)).await;
        assert_eq!(*marks.lock(), [1], "the sum did not go out");
        let _stub = connect(Some(&bus.address), &log, StubOptions::default())
            .await
            .expect("stub again");
        let sent = wait_lines(&log, "notify", 2).await;
        assert_eq!(sent.len(), 2, "{sent:?}");
        assert_eq!(sent[1]["summary"], SUMMARY_MORE);
        assert_eq!(sent[1]["body"], more_body(4));
        assert_eq!(marked(&marks, 3).await, [1, 2, 3]);
    }

    /// Missed ones caught up while the window is full wait for it, summed
    /// in one notification.
    #[tokio::test]
    async fn missed_ones_wait_for_a_full_window() {
        let Some((dir, bus)) = bus() else { return };
        let log = dir.path().join("stub.log");
        let _stub = connect(Some(&bus.address), &log, StubOptions::default())
            .await
            .expect("stub");
        let live = tree(
            dir.path(),
            (1..=3)
                .map(|id| numbered(id, ConflictKind::EditEdit, &format!("/l{id} (conflict x)")))
                .collect(),
        );
        let (missed, marks) = fixed_tree(
            dir.path(),
            (1..=5)
                .map(|id| numbered(id, ConflictKind::EditEdit, &format!("/m{id} (conflict x)")))
                .collect(),
        );
        let announcer = Announcer::start_with(watching(&bus));
        tokio::time::sleep(Duration::from_millis(500)).await;
        for id in 1..=3 {
            announcer.conflict(&live, id);
        }
        assert_eq!(wait_lines(&log, "notify", 3).await.len(), 3);
        follow(&announcer, &missed);
        let sent = wait_lines(&log, "notify", 4).await;
        assert_eq!(sent.len(), 4, "{sent:?}");
        assert_eq!(sent[3]["body"], more_body(5));
        assert_eq!(marked(&marks, 5).await, [1, 2, 3, 4, 5]);
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(lines(&log, "notify").len(), 4);
    }

    /// `notify.conflicts = false` and a lagging receiver: the conflicts it
    /// missed are marked too, so turning the setting on shows none of them.
    #[tokio::test]
    async fn turned_off_marks_the_conflicts_a_lag_missed() {
        let Some((dir, bus)) = bus() else { return };
        let (tree, marks, views) = growing_tree(
            dir.path(),
            vec![numbered(1, ConflictKind::EditEdit, "/x (conflict x)")],
        );
        let mut options = options(&bus, PathBuf::from("false"), true);
        options.settings.enabled = false;
        let announcer = Announcer::start_with(options);
        follow(&announcer, &tree);
        assert_eq!(marked(&marks, 1).await, [1]);
        views
            .lock()
            .insert(2, numbered(2, ConflictKind::EditEdit, "/y (conflict x)"));
        announcer.items.send(Item::Missed(1)).expect("runs");
        assert_eq!(marked(&marks, 2).await, [1, 2]);
    }

    /// `notify.conflicts = false`: nothing is sent; the conflicts, new or
    /// there at the start, are marked, so turning it on shows nothing old.
    #[tokio::test]
    async fn turned_off_sends_nothing_and_marks() {
        let Some((dir, bus)) = bus() else { return };
        let log = dir.path().join("stub.log");
        let _stub = connect(Some(&bus.address), &log, StubOptions::default())
            .await
            .expect("stub");
        let (tree, marks) = fixed_tree(
            dir.path(),
            vec![
                numbered(1, ConflictKind::EditEdit, "/x (conflict x)"),
                numbered(2, ConflictKind::EditEdit, "/y (conflict x)"),
                numbered(3, ConflictKind::RemoteDeleteLocalEdit, "/notice"),
            ],
        );
        let mut options = watching(&bus);
        options.settings.enabled = false;
        let announcer = Announcer::start_with(options);
        follow(&announcer, &tree);
        assert_eq!(marked(&marks, 2).await, [1, 2]);
        announcer.conflict(&tree, 1);
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(lines(&log, "notify").is_empty());
        assert_eq!(*marks.lock(), [1, 2], "each marked once");
    }

    /// M6.3: conflicts there before the service ran are shown once it
    /// does; a resolved one and a notice are not; the next appearance of the
    /// service shows nothing again.
    #[tokio::test]
    async fn missed_conflicts_are_shown_when_the_service_appears() {
        let Some((dir, bus)) = bus() else { return };
        let log = dir.path().join("stub.log");
        let (tree, marks) = fixed_tree(
            dir.path(),
            vec![
                numbered(1, ConflictKind::EditEdit, "/a (conflict x)"),
                ConflictView {
                    state: ConflictState::Resolved,
                    ..numbered(2, ConflictKind::EditEdit, "/b (conflict x)")
                },
                numbered(3, ConflictKind::CreateCreate, "/c (conflict x)"),
                numbered(4, ConflictKind::RenameRename, "/notice"),
            ],
        );
        let announcer = Announcer::start_with(watching(&bus));
        follow(&announcer, &tree);
        // No service: nothing goes out, nothing is marked.
        tokio::time::sleep(Duration::from_millis(700)).await;
        assert!(marks.lock().is_empty());
        let stub = connect(Some(&bus.address), &log, StubOptions::default())
            .await
            .expect("stub");
        let sent = wait_lines(&log, "notify", 2).await;
        assert_eq!(sent.len(), 2, "{sent:?}");
        assert!(sent[0]["body"].as_str().unwrap().contains("Changed here"));
        assert!(sent[1]["body"].as_str().unwrap().contains("Created here"));
        assert_eq!(marked(&marks, 2).await, [1, 3]);
        // The service restarts: the same conflicts are not shown again.
        drop(stub);
        tokio::time::sleep(Duration::from_millis(300)).await;
        let _stub = connect(Some(&bus.address), &log, StubOptions::default())
            .await
            .expect("stub again");
        tokio::time::sleep(Duration::from_millis(800)).await;
        assert_eq!(lines(&log, "notify").len(), 2);
        assert_eq!(*marks.lock(), [1, 3]);
    }

    /// Conflicts that arrive while the service is absent do not use up the
    /// window: once it runs they are shown one by one, not summed.
    #[tokio::test]
    async fn failed_sends_do_not_use_up_the_window() {
        let Some((dir, bus)) = bus() else { return };
        let log = dir.path().join("stub.log");
        let views = (1..=3)
            .map(|id| numbered(id, ConflictKind::EditEdit, &format!("/f{id} (conflict x)")))
            .collect();
        let (tree, marks) = fixed_tree(dir.path(), views);
        let announcer = Announcer::start_with(watching(&bus));
        follow(&announcer, &tree);
        for id in 1..=3 {
            announcer.conflict(&tree, id);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(marks.lock().is_empty());
        let _stub = connect(Some(&bus.address), &log, StubOptions::default())
            .await
            .expect("stub");
        let sent = wait_lines(&log, "notify", 3).await;
        assert_eq!(sent.len(), 3, "{sent:?}");
        assert!(sent.iter().all(|line| line["summary"] == SUMMARY));
        assert_eq!(marked(&marks, 3).await, [1, 2, 3]);
    }

    /// More than three missed are summed in one notification, the other
    /// way round too: the service runs before the tree is followed.
    #[tokio::test]
    async fn many_missed_conflicts_are_summed() {
        let Some((dir, bus)) = bus() else { return };
        let log = dir.path().join("stub.log");
        let _stub = connect(Some(&bus.address), &log, StubOptions::default())
            .await
            .expect("stub");
        let views = (1..=5)
            .map(|id| numbered(id, ConflictKind::EditEdit, &format!("/f{id} (conflict x)")))
            .collect();
        let (tree, marks) = fixed_tree(dir.path(), views);
        let announcer = Announcer::start_with(watching(&bus));
        // The watcher has seen the service before the tree arrives.
        tokio::time::sleep(Duration::from_millis(500)).await;
        follow(&announcer, &tree);
        let sent = wait_lines(&log, "notify", 1).await;
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert_eq!(sent[0]["summary"], SUMMARY_MORE);
        assert_eq!(sent[0]["body"], missed_body(5));
        assert_eq!(marked(&marks, 5).await, [1, 2, 3, 4, 5]);
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(lines(&log, "notify").len(), 1);
    }

    /// Two trees: each is marked in its own, and three missed in all are
    /// shown one by one.
    #[tokio::test]
    async fn each_tree_marks_its_own_conflicts() {
        let Some((dir, bus)) = bus() else { return };
        let log = dir.path().join("stub.log");
        let (first, first_marks) = fixed_tree(
            dir.path(),
            vec![numbered(1, ConflictKind::EditEdit, "/a (conflict x)")],
        );
        let (second, second_marks) = fixed_tree(
            dir.path(),
            vec![
                numbered(1, ConflictKind::EditEdit, "/b (conflict x)"),
                numbered(2, ConflictKind::EditEdit, "/c (conflict x)"),
            ],
        );
        let announcer = Announcer::start_with(watching(&bus));
        follow(&announcer, &first);
        follow(&announcer, &second);
        let _stub = connect(Some(&bus.address), &log, StubOptions::default())
            .await
            .expect("stub");
        assert_eq!(wait_lines(&log, "notify", 3).await.len(), 3);
        assert_eq!(marked(&first_marks, 1).await, [1]);
        assert_eq!(marked(&second_marks, 2).await, [1, 2]);
    }
}
