//! The Shared mount (M5.12; DESIGN.md §14.5; D107, D112): one tree per share
//! of the shared-with-me listing, under a virtual read-only top level whose
//! trees come and go while it is mounted.
//!
//! - **Listing.** At the mount, on each `DriveShareRefresh` of the core event
//!   stream (polled by [`Pollers::watch_core`]), and after a listing that
//!   failed, [`SharedRoot::refresh`] reads the listing and the share roots
//!   again and applies the difference: new shares open, revoked ones go,
//!   renamed ones (by their owner, D125) change their name in place.
//! - **Names** ([`display_names`]): a share keeps its name; same names in one
//!   folder of the listing get the owner, then a number. A shown name stays
//!   with its share while the share is listed, across remounts too
//!   (`shared-names.json`): a share that arrives later with that name gets
//!   the suffix instead (D125).
//! - **Shared files** sit in the files folder ([`FILES_FOLDER`]); their tree's
//!   root is a virtual folder holding the file ([`ShareTree`]).
//! - **Revocation.** A share leaves at the first "not found" of its root
//!   ([`ShareTree`] tells [`SharedRoot::drop_gone`]) or when the listing no
//!   longer has it, whichever comes first. Its tree leaves the mount (its
//!   open files answer EIO), its poller and engine stop, and its state and
//!   cached blocks are deleted. Changes still queued and local-only files are
//!   exported to `orphaned/` first ([`Roots::orphan`], D114, D124).
//! - **Writes** (M5.14a, D080, D114): a folder share's write gate is open
//!   while the role is `Editor` or `Admin` ([`may_write`]); `Viewer` and
//!   shared single files stay EROFS. A role that no longer allows writing,
//!   read from the listing, or a change the server refuses ("permission
//!   denied"), closes the gate and holds the queued changes; a role that
//!   allows writing again opens it and they upload.

use super::{OpenTree, Roots, STOP_GRACE, Session, TreeSpec};
use pdmount_backend::NodeKind;
use pdmount_backend::shared::{
    OnGone, ShareRole, ShareTree, SharedEntry, SharedListing, SharedWithMe,
};
use pdmount_core::config::SignatureMode;
use pdmount_core::poller::{Pollers, ShareListener, VolumeUse};
use pdmount_fuse::{TreePlace, TreeSet};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::Instant;

/// The files folder of a Shared mount: the shared single files.
pub(super) const FILES_FOLDER: &str = "Shared files";

/// Shares a Shared mount opens, oldest first. Each tree has an engine with
/// its own RAM pool (16 MiB, D110), so this bounds what the listing, which
/// other users fill, can make the daemon hold.
pub(super) const MAX_SHARES: usize = 32;

/// The longest owner a name suffix shows, in bytes.
const MAX_OWNER_BYTES: usize = 64;

/// How long a share that comes back waits for the cleanup of its earlier
/// removal before it is left for a later listing.
const CLEANUP_WAIT: std::time::Duration = std::time::Duration::from_secs(20);

/// A share's tree: its volume and root, the key of its state (§13.1).
pub(super) fn share_key(entry: &SharedEntry) -> String {
    format!("{}~{}", entry.volume, entry.root)
}

/// Whether a share's name can be a file name (the bounds of My Files'
/// remote names, D070): not empty, `.` or `..`, no `/` or NUL, at most
/// 255 bytes.
fn usable(name: &str) -> bool {
    pdmount_core::vfs::validate_name(name.as_bytes()).is_ok() && !name.contains('\0')
}

/// The owner as a name suffix shows it: untrusted input, so control
/// characters and `/` become `_`, and it is cut to [`MAX_OWNER_BYTES`].
fn owner_label(owner: &str) -> String {
    let mut label = String::new();
    for c in owner.trim().chars() {
        let c = if c.is_control() || c == '/' { '_' } else { c };
        if label.len() + c.len_utf8() > MAX_OWNER_BYTES {
            break;
        }
        label.push(c);
    }
    if label.is_empty() {
        "unknown".into()
    } else {
        label
    }
}

/// `<name> (<owner>)`, or with `n` > 1 `<name> (<owner>, n)`, the name cut
/// so that the whole stays within 255 bytes.
fn suffixed(name: &str, owner: &str, n: usize) -> String {
    let suffix = if n == 1 {
        format!(" ({owner})")
    } else {
        format!(" ({owner}, {n})")
    };
    let room = pdmount_core::vfs::MAX_NAME_BYTES.saturating_sub(suffix.len());
    let mut end = name.len().min(room);
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{suffix}", &name[..end])
}

/// The name a share is shown under, as the mount keeps it (D125): the
/// share's own name when that name was given, and the shown name.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(super) struct Named {
    /// The share's own name (as cut to fit) when `shown` was given.
    pub name: String,
    pub shown: String,
}

/// The shown names of a mount, by [`share_key`], as kept in
/// `shared-names.json` (D125).
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct NamesFile {
    version: u64,
    names: BTreeMap<String, Named>,
}

const NAMES_VERSION: u64 = 1;

/// Reads the kept names; a missing or unreadable file keeps none (the
/// shares are then named as if they all arrived now).
fn load_names(path: &std::path::Path) -> BTreeMap<String, Named> {
    let text = match std::fs::read(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return BTreeMap::new(),
        Err(error) => {
            tracing::warn!(%error, "cannot read the shares' kept names; naming them anew");
            return BTreeMap::new();
        }
    };
    match serde_json::from_slice::<NamesFile>(&text) {
        Ok(file) if file.version == NAMES_VERSION => file.names,
        Ok(_) | Err(_) => {
            tracing::warn!("the shares' kept names cannot be read; naming them anew");
            BTreeMap::new()
        }
    }
}

/// Writes the kept names (a new file renamed over the old one).
fn save_names(path: &std::path::Path, names: &BTreeMap<String, Named>) -> std::io::Result<()> {
    let file = NamesFile {
        version: NAMES_VERSION,
        names: names.clone(),
    };
    let text = serde_json::to_vec_pretty(&file).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

/// The names the shares show under (D107, D125), by [`share_key`]. Folders
/// and files are two folders of the listing (the top level and the files
/// folder), each named on its own; in the top level, [`FILES_FOLDER`] is
/// taken.
///
/// A share shown before (`kept`) keeps its shown name while its own name is
/// the one it was given for: a name that arrives later never takes it over,
/// whatever it looks like (`Team (alice@…)` included). The others, shares
/// new to the mount or renamed by their owner, are named in the order the
/// shares were created: a share keeps its own name when no other share has
/// it (shown, or among them); when two or more of them have the same name,
/// or a share's name is shown already or the taken name, each of them
/// becomes `<name> (<owner>)`, and where that is taken too
/// `<name> (<owner>, 2)`, `(…, 3)`. Entries with a name that cannot be a file
/// name are left out.
pub(super) fn display_names(
    entries: &[SharedEntry],
    kept: &BTreeMap<String, Named>,
) -> BTreeMap<String, String> {
    let mut names = BTreeMap::new();
    for (kind, reserved) in [
        (NodeKind::Folder, Some(FILES_FOLDER)),
        (NodeKind::File, None),
    ] {
        let mut folder: Vec<&SharedEntry> = entries
            .iter()
            .filter(|entry| entry.kind == kind && usable(&entry.name))
            .collect();
        // The order the shares were created; ties by share and tree.
        folder.sort_by(|a, b| {
            (a.created, &a.share, share_key(a)).cmp(&(b.created, &b.share, share_key(b)))
        });
        let mut taken: BTreeSet<String> = reserved.into_iter().map(str::to_owned).collect();
        // Shown names stay with the shares that have them.
        let mut fresh = Vec::new();
        for entry in folder {
            let key = share_key(entry);
            match kept.get(&key) {
                Some(named)
                    if named.name == entry.name
                        && usable(&named.shown)
                        && !taken.contains(&named.shown) =>
                {
                    taken.insert(named.shown.clone());
                    names.insert(key, named.shown.clone());
                }
                _ => fresh.push(entry),
            }
        }
        let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
        for entry in &fresh {
            *counts.entry(entry.name.as_str()).or_default() += 1;
        }
        let clashes: Vec<bool> = fresh
            .iter()
            .map(|entry| counts[entry.name.as_str()] > 1 || taken.contains(&entry.name))
            .collect();
        for (entry, _) in fresh.iter().zip(&clashes).filter(|(_, clash)| !**clash) {
            taken.insert(entry.name.clone());
            names.insert(share_key(entry), entry.name.clone());
        }
        for (entry, _) in fresh.iter().zip(&clashes).filter(|(_, clash)| **clash) {
            let owner = owner_label(&entry.owner);
            let mut n = 1;
            let name = loop {
                let candidate = suffixed(&entry.name, &owner, n);
                if !taken.contains(&candidate) {
                    break candidate;
                }
                n += 1;
            };
            taken.insert(name.clone());
            names.insert(share_key(entry), name);
        }
    }
    names
}

/// What a mount keeps of the shares it shows (D125).
fn kept_names(shown: &BTreeMap<String, Shown>) -> BTreeMap<String, Named> {
    shown
        .iter()
        .map(|(key, shown)| {
            (
                key.clone(),
                Named {
                    name: shown.entry.name.clone(),
                    shown: shown.name.clone(),
                },
            )
        })
        .collect()
}

/// Whether a share's tree may be written (D080): a folder, with the role
/// `Editor` or `Admin`. `Viewer` shares and shared single files stay EROFS.
pub(super) fn may_write(entry: &SharedEntry) -> bool {
    entry.kind == NodeKind::Folder && matches!(entry.role, ShareRole::Editor | ShareRole::Admin)
}

/// Why a share left the mount, as `orphaned/` records it.
const LEFT_THE_LISTING: &str = "the share left the shared-with-me listing";
const ROOT_NOT_FOUND: &str = "the share's root is not found";

/// A share as the mount shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Shown {
    pub entry: SharedEntry,
    pub name: String,
}

/// What changed between two listings, by [`share_key`].
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct ListingDiff {
    pub added: Vec<String>,
    /// Revoked, or left: gone from the listing.
    pub removed: Vec<String>,
    /// The new name.
    pub renamed: Vec<(String, String)>,
    /// The new role.
    pub roles: Vec<(String, ShareRole)>,
}

impl ListingDiff {
    pub fn of(before: &BTreeMap<String, Shown>, after: &BTreeMap<String, Shown>) -> Self {
        let mut diff = Self::default();
        for (key, shown) in after {
            match before.get(key) {
                None => diff.added.push(key.clone()),
                Some(old) => {
                    if old.name != shown.name {
                        diff.renamed.push((key.clone(), shown.name.clone()));
                    }
                    if old.entry.role != shown.entry.role {
                        diff.roles.push((key.clone(), shown.entry.role));
                    }
                }
            }
        }
        diff.removed = before
            .keys()
            .filter(|key| !after.contains_key(*key))
            .cloned()
            .collect();
        diff
    }
}

/// A share's name cut to [`pdmount_core::vfs::MAX_NAME_BYTES`] on a
/// character boundary, when that makes it usable: an over-long name is no
/// reason to leave a share out.
fn cut_to_fit(name: &str) -> Option<String> {
    let mut end = name.len().min(pdmount_core::vfs::MAX_NAME_BYTES);
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    let cut = &name[..end];
    usable(cut).then(|| cut.to_owned())
}

/// The listing's entries, with each share whose root cannot be decrypted
/// (D119) kept as it is shown now: it is not gone, so its tree stays open
/// with its name and role. One not shown yet stays out until it can be
/// read.
pub(super) fn with_degraded(
    listing: SharedListing,
    open: &BTreeMap<String, Shown>,
) -> Vec<SharedEntry> {
    let SharedListing {
        mut entries,
        degraded,
    } = listing;
    for share in degraded {
        let key = format!("{}~{}", share.volume, share.root);
        match open.get(&key) {
            Some(shown) => {
                tracing::warn!(
                    share = shown.name,
                    "a share's root cannot be decrypted; it is kept as it was"
                );
                entries.push(shown.entry.clone());
            }
            None => tracing::warn!(
                share = share.share,
                "a share's root cannot be decrypted; it is not shown until it can be read"
            ),
        }
    }
    entries
}

/// The shares to show of a listing: those not known to be gone, with a
/// usable name, at most [`MAX_SHARES`] (oldest first); the rest is
/// returned with the reason it is left out. An over-long name is cut; a
/// share shown now (`open`) whose new name cannot be a file name keeps its
/// old one, so it does not leave the mount (with its changes) for a name
/// another user chose. Shares named before keep their names (`kept`,
/// [`display_names`]).
pub(super) fn select(
    listing: Vec<SharedEntry>,
    gone: &BTreeSet<String>,
    open: &BTreeMap<String, Shown>,
    kept: &BTreeMap<String, Named>,
) -> (BTreeMap<String, Shown>, Vec<(String, String)>) {
    let mut left_out = Vec::new();
    let mut usable_entries = Vec::new();
    for mut entry in listing {
        if gone.contains(&entry.share) {
            continue;
        }
        if !usable(&entry.name) {
            if let Some(cut) = cut_to_fit(&entry.name) {
                entry.name = cut;
            } else if let Some(shown) = open.get(&share_key(&entry)) {
                tracing::warn!(
                    share = entry.share,
                    "a share's new name cannot be a file name; it keeps its old one"
                );
                entry.name = shown.entry.name.clone();
            } else {
                tracing::warn!(
                    share = entry.share,
                    "a share's name cannot be a file name; it is left out"
                );
                continue;
            }
        }
        usable_entries.push(entry);
    }
    usable_entries.sort_by(|a, b| (a.created, &a.share).cmp(&(b.created, &b.share)));
    if usable_entries.len() > MAX_SHARES {
        for entry in usable_entries.drain(MAX_SHARES..) {
            left_out.push((
                entry.name,
                format!("left out: a Shared mount shows at most {MAX_SHARES} shares"),
            ));
        }
    }
    let names = display_names(&usable_entries, kept);
    let shown = usable_entries
        .into_iter()
        .filter_map(|entry| {
            let key = share_key(&entry);
            let name = names.get(&key)?.clone();
            Some((key, Shown { entry, name }))
        })
        .collect();
    (shown, left_out)
}

/// An open share.
struct OpenShare {
    slot: usize,
    tree: OpenTree,
    shown: Shown,
}

/// What a refresh keeps between runs.
#[derive(Default)]
struct Inner {
    open: BTreeMap<String, OpenShare>,
    /// Shares whose root was not found, by share id: not opened again while
    /// the listing still has them (a revocation the core event has not
    /// reported yet).
    gone: BTreeSet<String>,
    /// Shares the last refresh could not open, with the reason.
    failed: Vec<(String, String)>,
    /// The names the shares of the last listing are shown under, as kept
    /// in `shared-names.json` (D125).
    names: BTreeMap<String, Named>,
}

/// What `status` shows of a Shared mount; read without waiting.
#[derive(Clone, Default)]
pub(super) struct View {
    pub trees: Vec<(OpenTree, ShareRole)>,
    pub failed: Vec<(String, String)>,
}

/// A mounted Shared root: its trees follow the listing.
pub(super) struct SharedRoot {
    roots: Weak<Roots>,
    session: Session,
    source: Arc<dyn SharedWithMe>,
    set: TreeSet,
    /// Where the root is mounted: its trees are folders of it, its shared
    /// files are in [`FILES_FOLDER`].
    mountpoint: std::path::PathBuf,
    read_only: bool,
    /// `pending_marker` of the root (D076, D114).
    pending_marker: bool,
    /// The root's mode for unverified signatures (D132).
    signatures: SignatureMode,
    /// Where the shown names are kept between mounts (D125).
    names_file: std::path::PathBuf,
    /// Held through a refresh or a drop, so they run one at a time.
    inner: tokio::sync::Mutex<Inner>,
    view: parking_lot::Mutex<View>,
    /// The last listing failed: the next core poll reads it again.
    stale: AtomicBool,
    closed: AtomicBool,
    /// Keys of shares whose root was not found ([`ShareTree`]).
    gone: tokio::sync::mpsc::UnboundedSender<String>,
    gone_task: parking_lot::Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Keys of shares whose changes the server refused (D114).
    refused: tokio::sync::mpsc::UnboundedSender<String>,
    refused_task: parking_lot::Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// As registered with the pollers, to unregister it.
    listener: parking_lot::Mutex<Option<Arc<dyn ShareListener>>>,
}

impl SharedRoot {
    /// Starts a Shared root on the mounted `set`: the first listing, then
    /// the core event stream.
    pub async fn start(
        roots: &Arc<Roots>,
        session: Session,
        set: TreeSet,
        mountpoint: &std::path::Path,
        read_only: bool,
        pending_marker: bool,
        signatures: SignatureMode,
    ) -> Arc<Self> {
        let source = session.source.shares();
        let (gone, mut gone_rx) = tokio::sync::mpsc::unbounded_channel();
        let (refused, mut refused_rx) = tokio::sync::mpsc::unbounded_channel();
        let names_file = roots.context.paths.shared_names_file();
        let inner = Inner {
            names: load_names(&names_file),
            ..Inner::default()
        };
        let this = Arc::new(Self {
            roots: Arc::downgrade(roots),
            session,
            source: Arc::clone(&source),
            set,
            mountpoint: mountpoint.to_owned(),
            read_only,
            pending_marker,
            signatures,
            names_file,
            inner: tokio::sync::Mutex::new(inner),
            view: parking_lot::Mutex::default(),
            stale: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            gone,
            gone_task: parking_lot::Mutex::default(),
            refused,
            refused_task: parking_lot::Mutex::default(),
            listener: parking_lot::Mutex::default(),
        });
        let weak = Arc::downgrade(&this);
        *this.gone_task.lock() = Some(tokio::spawn(async move {
            while let Some(key) = gone_rx.recv().await {
                let Some(this) = weak.upgrade() else {
                    break;
                };
                this.drop_gone(&key).await;
            }
        }));
        let weak = Arc::downgrade(&this);
        *this.refused_task.lock() = Some(tokio::spawn(async move {
            while let Some(key) = refused_rx.recv().await {
                let Some(this) = weak.upgrade() else {
                    break;
                };
                this.write_refused(&key).await;
            }
        }));
        // The cursor first: a change after it reaches the listing below or
        // the next poll.
        let since = match source.latest_core_event_id().await {
            Ok(since) => Some(since),
            Err(error) => {
                tracing::warn!(%error, "cannot read the core event stream's position");
                None
            }
        };
        this.refresh().await;
        let listener: Arc<dyn ShareListener> = Arc::clone(&this) as _;
        *this.listener.lock() = Some(Arc::clone(&listener));
        let context = &roots.context;
        roots.pollers.watch_core(
            listener,
            source,
            Arc::clone(&context.health),
            context.config.sync.poll_interval.0,
            since,
        );
        this
    }

    fn pollers(&self) -> Option<Arc<Pollers>> {
        Some(Arc::clone(&self.roots.upgrade()?.pollers))
    }

    /// The trees and failures, for `status`.
    pub fn view(&self) -> View {
        self.view.lock().clone()
    }

    /// The open trees.
    pub fn trees(&self) -> Vec<OpenTree> {
        self.view
            .lock()
            .trees
            .iter()
            .map(|(tree, _)| tree.clone())
            .collect()
    }

    fn publish(&self, inner: &Inner) {
        let mut view = self.view.lock();
        view.trees = inner
            .open
            .values()
            .map(|share| {
                let mut tree = share.tree.clone();
                tree.name = share.shown.name.clone();
                (tree, share.shown.entry.role)
            })
            .collect();
        view.failed = inner.failed.clone();
    }

    /// Reads the listing and the share roots again and applies the
    /// difference to the mount. A listing that fails changes nothing; the
    /// next core poll tries again.
    pub async fn refresh(&self) {
        let mut inner = self.inner.lock().await;
        if self.closed.load(Ordering::SeqCst) {
            return;
        }
        let listing = match self.source.shared_listing().await {
            Ok(listing) => listing,
            Err(error) => {
                self.stale.store(true, Ordering::SeqCst);
                tracing::warn!(%error, "cannot read the shared-with-me listing; trying again at the next core poll");
                inner.failed = vec![(
                    "shared-with-me listing".into(),
                    format!("cannot be read: {error}"),
                )];
                self.publish(&inner);
                return;
            }
        };
        self.stale.store(false, Ordering::SeqCst);
        // A gone share that left the listing may come back as a new one.
        inner.gone.retain(|share| {
            listing.entries.iter().any(|entry| &entry.share == share)
                || listing
                    .degraded
                    .iter()
                    .any(|degraded| &degraded.share == share)
        });
        let before: BTreeMap<String, Shown> = inner
            .open
            .iter()
            .map(|(key, share)| (key.clone(), share.shown.clone()))
            .collect();
        let entries = with_degraded(listing, &before);
        let (after, mut failed) = select(entries, &inner.gone, &before, &inner.names);
        let names = kept_names(&after);
        if names != inner.names {
            if let Err(error) = save_names(&self.names_file, &names) {
                tracing::warn!(%error, "cannot keep the shares' names for the next mount");
            }
            inner.names = names;
        }
        let diff = ListingDiff::of(&before, &after);
        for key in &diff.removed {
            if let Some(share) = inner.open.remove(key) {
                tracing::info!(
                    share = share.shown.name,
                    "a share left the listing; removing it"
                );
                self.remove(share, LEFT_THE_LISTING);
            }
        }
        for (key, name) in &diff.renamed {
            if let Some(share) = inner.open.get_mut(key) {
                tracing::info!(from = share.shown.name, to = name, "a share's name changes");
                self.set.rename(share.slot, name);
                share.shown.name = name.clone();
            }
        }
        for (key, role) in &diff.roles {
            if let Some(share) = inner.open.get_mut(key) {
                tracing::info!(
                    share = share.shown.name,
                    role = role.as_str(),
                    "a share's role changed"
                );
                share.shown.entry.role = *role;
                self.apply_role(share);
            }
        }
        for key in &diff.added {
            // A stop that began meanwhile stops the shares opened so far.
            let stopping = self.roots.upgrade().is_none_or(|roots| roots.is_stopping());
            if self.closed.load(Ordering::SeqCst) || stopping {
                break;
            }
            let shown = after[key].clone();
            match self.open(key, &shown).await {
                Ok(share) => {
                    tracing::info!(
                        share = shown.name,
                        role = shown.entry.role.as_str(),
                        kind = ?shown.entry.kind,
                        "a share is mounted"
                    );
                    inner.open.insert(key.clone(), share);
                }
                Err(error) => {
                    let error = format!("{error:#}");
                    tracing::warn!(share = shown.name, error, "a share did not open");
                    failed.push((shown.name.clone(), error));
                }
            }
        }
        inner.failed = failed;
        self.publish(&inner);
    }

    /// Opens or closes a share's write gate after a role change (D114): a
    /// role that allows writing opens it and lets the held changes upload;
    /// one that does not closes it and holds the queued changes.
    fn apply_role(&self, share: &OpenShare) {
        let engine = &share.tree.engine;
        let name = &share.shown.name;
        if may_write(&share.shown.entry) {
            if !engine.write_gate_open() || engine.write_was_refused() {
                let held = engine.restore_write_access();
                tracing::info!(
                    share = name,
                    held,
                    "a share allows writing again; its held changes upload"
                );
            }
        } else if engine.write_gate_open() {
            let held = engine.withdraw_write_access();
            tracing::warn!(
                share = name,
                held,
                "a share no longer allows writing; its changes are kept and wait"
            );
            if held > 0 {
                notify_held(name, held);
            }
        }
    }

    /// The server refused a change in a share (D114): its gate is closed
    /// and its changes held already; the listing is read again for the
    /// role, or to find the share gone.
    async fn write_refused(&self, key: &str) {
        let (name, held) = {
            let inner = self.inner.lock().await;
            match inner.open.get(key) {
                Some(share) => (
                    share.shown.name.clone(),
                    share.tree.engine.queue_counts().parked,
                ),
                None => return,
            }
        };
        notify_held(&name, held);
        self.refresh().await;
    }

    /// Opens a share's tree and adds it to the mount.
    async fn open(&self, key: &str, shown: &Shown) -> anyhow::Result<OpenShare> {
        let roots = self
            .roots
            .upgrade()
            .ok_or_else(|| anyhow::anyhow!("the daemon is stopping"))?;
        let entry = &shown.entry;
        // A removal of the same tree that still runs (the share came back
        // meanwhile) ends first: two engines on one store would lose
        // changes, and its end deletes or moves the store (D114).
        let tree_key = pdmount_core::paths::tree_key(&entry.volume, entry.root.as_str());
        if roots.cleanups.running(&tree_key) {
            let end = Instant::now() + CLEANUP_WAIT;
            while roots.cleanups.running(&tree_key) {
                if self.closed.load(Ordering::SeqCst) || roots.is_stopping() {
                    anyhow::bail!("the mount is stopping");
                }
                if Instant::now() >= end {
                    // The next core poll reads the listing again.
                    self.stale.store(true, Ordering::SeqCst);
                    anyhow::bail!(
                        "its earlier removal is still being cleaned up; it opens at a later listing"
                    );
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        }
        let own = self.session.source.own_volume() == entry.volume;
        let spec = TreeSpec {
            name: shown.name.clone(),
            volume: entry.volume.clone(),
            root: entry.root.as_str().to_owned(),
            own,
            writable: may_write(entry),
            pending_marker: self.pending_marker,
            signatures: self.signatures,
        };
        let on_gone: OnGone = {
            let gone = self.gone.clone();
            let key = key.to_owned();
            Arc::new(move || {
                let _ = gone.send(key.clone());
            })
        };
        let backend = Arc::new(ShareTree::new(
            self.session.source.tree(&entry.volume, entry.root.as_str()),
            entry.root.clone(),
            entry.kind,
            on_gone,
        ));
        let (engine, invalidations) = roots
            .open_tree_on(&spec, backend, &self.session, self.read_only)
            .await?;
        let place = match entry.kind {
            NodeKind::Folder => TreePlace::Folder,
            NodeKind::File => TreePlace::File(pdmount_backend::shared::SHARED_FILE_NAME.into()),
        };
        // A shared file shows from its tree's root listing: list it now.
        if entry.kind == NodeKind::File
            && let Err(error) = engine.ensure_listed(pdmount_core::vfs::ROOT_INO).await
        {
            let _ = engine.shutdown().await;
            return Err(anyhow::Error::from(error).context("cannot read the shared file"));
        }
        let volume_use =
            (!own).then(|| VolumeUse::new(Arc::clone(roots.pollers.cadence()), &entry.volume));
        let added = self.set.add(pdmount_fuse::MountTree {
            name: shown.name.clone(),
            engine: Arc::clone(&engine),
            invalidations,
            volume_use,
            place,
        });
        let slot = match added {
            Ok(slot) => slot,
            Err(error) => {
                let _ = engine.shutdown().await;
                return Err(anyhow::Error::from(error).context("cannot add the share to the mount"));
            }
        };
        // Changes queued while the role still allowed them wait (D114).
        if !may_write(entry) && engine.queue_counts().pending > 0 {
            let held = engine.withdraw_write_access();
            tracing::warn!(
                share = shown.name,
                held,
                "a share does not allow writing; its queued changes are kept and wait"
            );
        }
        let refused = {
            let refused = self.refused.clone();
            let key = key.to_owned();
            Arc::new(move |_held: usize| {
                let _ = refused.send(key.clone());
            })
        };
        engine.follow_write_refusals(refused);
        let place = match entry.kind {
            NodeKind::Folder => {
                crate::conflict_notify::Place::folder(&shown.name, &self.mountpoint)
            }
            NodeKind::File => crate::conflict_notify::Place::file(
                &shown.name,
                self.mountpoint.join(FILES_FOLDER).join(&shown.name),
            ),
        };
        super::announce_tree(&roots, &shown.name, place, &engine);
        engine.start_background_polled();
        roots.pollers.add(&entry.volume, own, Arc::clone(&engine));
        Ok(OpenShare {
            slot,
            tree: OpenTree {
                name: shown.name.clone(),
                volume: entry.volume.clone(),
                own,
                engine,
                file: entry.kind == NodeKind::File,
            },
            shown: shown.clone(),
        })
    }

    /// Takes a share out of the mount at once (paths through it answer
    /// ENOENT, its open files EIO from now on), stops its poller and engine,
    /// and deletes its state and cached blocks: the share is gone. Changes
    /// still queued and local-only files are exported to `orphaned/` first,
    /// with the raw state beside them when some content could not be written
    /// (RQ3, D114, D124); if the export fails, the state stays where it is.
    ///
    /// The engine is retired before anything is counted: a change in flight
    /// that commits later is answered EIO, so none is acknowledged and then
    /// left out of the export. The cleanup runs in the background, kept in
    /// [`Roots`]' cleanups: the share is not opened again until it ends,
    /// and a stop waits for it. It first waits for an event pass that took
    /// the tree before it was retired (later passes skip it).
    fn remove(&self, share: OpenShare, reason: &'static str) {
        self.set.remove(share.slot);
        let engine = Arc::clone(&share.tree.engine);
        engine.retire();
        if let Some(pollers) = self.pollers() {
            pollers.remove(&engine);
        }
        let key = pdmount_core::paths::tree_key(
            &share.shown.entry.volume,
            share.shown.entry.root.as_str(),
        );
        let weak = self.roots.clone();
        let cleanup_key = key.clone();
        let work = async move {
            let name = share.shown.name;
            if tokio::time::timeout(STOP_GRACE, engine.wait_for_event_pass())
                .await
                .is_err()
            {
                tracing::warn!(
                    share = name,
                    "an event pass over a gone share still runs; it changes nothing any more"
                );
            }
            // Staged content no entry uploads (local-only files, edits in a
            // local-only folder) counts as changes to keep: it is only in
            // the staging this removal would delete (RQ3, D114).
            let queued = engine.changes_to_keep();
            let kept = match weak.upgrade() {
                Some(roots) if queued > 0 => roots.orphan(&name, &engine, reason).await,
                _ => None,
            };
            let deadline = weak.upgrade().map_or_else(
                || Instant::now() + STOP_GRACE,
                |roots| roots.cleanup_deadline(),
            );
            if let Err(error) = engine.shutdown_by(deadline).await {
                tracing::error!(tree = name, %error, "could not commit a tree's store");
            }
            drop(engine);
            let Some(roots) = weak.upgrade() else {
                return;
            };
            match kept {
                Some((dir, incomplete)) => {
                    roots.retire_tree(&key, (incomplete > 0).then_some(&*dir))
                }
                None if queued > 0 => tracing::error!(
                    share = name,
                    "a gone share's queued changes could not be exported; its state is kept"
                ),
                None => roots.purge_tree(&key),
            }
        };
        match self.roots.upgrade() {
            Some(roots) => {
                roots
                    .cleanups
                    .spawn(cleanup_key, Arc::clone(&share.tree.engine), work);
            }
            // The daemon is ending: the cleanup runs as far as it gets.
            None => drop(tokio::spawn(work)),
        }
    }

    /// A share's root was not found: it goes now, without waiting for the
    /// core event (M5.9).
    pub async fn drop_gone(&self, key: &str) {
        let mut inner = self.inner.lock().await;
        if self.closed.load(Ordering::SeqCst) {
            return;
        }
        if let Some(share) = inner.open.remove(key) {
            tracing::info!(
                share = share.shown.name,
                "a share's root is not found; removing the share"
            );
            inner.gone.insert(share.shown.entry.share.clone());
            self.remove(share, ROOT_NOT_FOUND);
            self.publish(&inner);
        }
    }

    /// Stops following the listing: no tree comes or goes from now on.
    /// Returns the open trees, which the caller stops.
    pub async fn close(&self) -> Vec<OpenTree> {
        self.closed.store(true, Ordering::SeqCst);
        if let (Some(listener), Some(pollers)) = (self.listener.lock().take(), self.pollers()) {
            pollers.unwatch_core(&listener);
        }
        if let Some(task) = self.gone_task.lock().take() {
            task.abort();
        }
        if let Some(task) = self.refused_task.lock().take() {
            task.abort();
        }
        let mut inner = self.inner.lock().await;
        let trees = std::mem::take(&mut inner.open)
            .into_values()
            .map(|share| share.tree)
            .collect();
        inner.failed.clear();
        self.publish(&inner);
        trees
    }
}

/// A desktop notification that a share's changes wait (D114). The share's
/// name is plain text in the summary, cleaned (`notify::clean_name`); the
/// body may be read as markup.
fn notify_held(name: &str, held: usize) {
    let noun = if held == 1 { "change" } else { "changes" };
    crate::notify::desktop(
        &format!(
            "PdMount: {held} {noun} to {} wait",
            crate::notify::clean_name(name)
        ),
        "The share no longer allows changes; they are kept. See `pdmount status`.",
    );
}

#[async_trait::async_trait]
impl ShareListener for SharedRoot {
    async fn shares_changed(&self) {
        self.refresh().await;
    }

    fn stale(&self) -> bool {
        self.stale.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pdmount_backend::LinkId;
    use std::time::{Duration, UNIX_EPOCH};

    fn entry(share: &str, name: &str, owner: &str, created: u64, kind: NodeKind) -> SharedEntry {
        SharedEntry {
            share: share.into(),
            volume: format!("vol-{owner}"),
            root: LinkId::new(format!("link-{share}")),
            kind,
            name: name.into(),
            owner: owner.into(),
            role: ShareRole::Viewer,
            created: Some(UNIX_EPOCH + Duration::from_secs(created)),
        }
    }

    fn folder(share: &str, name: &str, owner: &str, created: u64) -> SharedEntry {
        entry(share, name, owner, created, NodeKind::Folder)
    }

    /// The names in the order of `entries`.
    fn named(entries: &[SharedEntry]) -> Vec<String> {
        let names = display_names(entries, &BTreeMap::new());
        entries
            .iter()
            .map(|entry| {
                names
                    .get(&share_key(entry))
                    .cloned()
                    .unwrap_or_else(|| "<left out>".into())
            })
            .collect()
    }

    #[test]
    fn a_share_keeps_its_name_unless_another_has_it() {
        let entries = [
            folder("s1", "Team", "alice@example.test", 1),
            folder("s2", "Photos 2024", "bob@example.test", 2),
            folder("s3", "Report (1)", "bob@example.test", 3),
        ];
        assert_eq!(named(&entries), ["Team", "Photos 2024", "Report (1)"]);
    }

    #[test]
    fn same_names_get_the_owner_then_a_number_in_creation_order() {
        let entries = [
            folder("s3", "Team", "alice@example.test", 30),
            folder("s1", "Team", "bob@example.test", 10),
            folder("s2", "Team", "alice@example.test", 20),
            folder("s4", "Other", "carol@example.test", 40),
        ];
        assert_eq!(
            named(&entries),
            [
                "Team (alice@example.test, 2)",
                "Team (bob@example.test)",
                "Team (alice@example.test)",
                "Other",
            ]
        );
        // A suffixed name that another share already has gets a number.
        let entries = [
            folder("s1", "Team (alice)", "zed", 1),
            folder("s2", "Team", "alice", 2),
            folder("s3", "Team", "bob", 3),
        ];
        assert_eq!(
            named(&entries),
            ["Team (alice)", "Team (alice, 2)", "Team (bob)"]
        );
    }

    #[test]
    fn folders_and_files_are_named_in_their_own_folders() {
        let entries = [
            folder("s1", "plan.txt", "alice", 1),
            entry("s2", "plan.txt", "bob", 2, NodeKind::File),
            entry("s3", "notes.md", "bob", 3, NodeKind::File),
            entry("s4", "notes.md", "carol", 4, NodeKind::File),
            // The files folder's name is taken in the top level.
            folder("s5", FILES_FOLDER, "dave", 5),
        ];
        assert_eq!(
            named(&entries),
            [
                "plan.txt",
                "plan.txt",
                "notes.md (bob)",
                "notes.md (carol)",
                "Shared files (dave)",
            ]
        );
    }

    #[test]
    fn untrusted_names_and_owners_stay_within_the_bounds() {
        let long = "n".repeat(250);
        let entries = [
            folder("s1", &long, "alice/../../etc\n\u{7}", 1),
            folder("s2", &long, &"é".repeat(100), 2),
            folder("s3", "..", "x", 3),
            folder("s4", "a/b", "x", 4),
            folder("s5", "", "x", 5),
            folder("s6", "nul\0name", "x", 6),
            folder("s7", "ok", "", 7),
            folder("s8", "ok", "  ", 8),
        ];
        let names = named(&entries);
        assert!(names[0].starts_with("nnn"));
        assert!(names[0].ends_with(" (alice_.._.._etc__)"), "{}", names[0]);
        assert!(names[1].ends_with(')'));
        for name in &names[..2] {
            assert!(name.len() <= 255, "{} bytes", name.len());
            assert!(usable(name), "{name:?}");
        }
        // Cut on a character boundary, at most 64 bytes of owner.
        let owner = names[1]
            .rsplit_once(" (")
            .expect("suffix")
            .1
            .trim_end_matches(')');
        assert!(owner.len() <= MAX_OWNER_BYTES && owner.chars().all(|c| c == 'é'));
        assert_eq!(&names[2..6], ["<left out>"; 4]);
        assert_eq!(&names[6..], ["ok (unknown)", "ok (unknown, 2)"]);
    }

    fn shown(entries: &[SharedEntry]) -> BTreeMap<String, Shown> {
        select(
            entries.to_vec(),
            &BTreeSet::new(),
            &BTreeMap::new(),
            &BTreeMap::new(),
        )
        .0
    }

    /// The shares of a refresh after `before`, as `refresh` names them.
    fn next(before: &BTreeMap<String, Shown>, entries: &[SharedEntry]) -> BTreeMap<String, Shown> {
        select(
            entries.to_vec(),
            &BTreeSet::new(),
            before,
            &kept_names(before),
        )
        .0
    }

    #[test]
    fn the_listing_diff_finds_added_revoked_renamed_and_role_changes() {
        let team = folder("s1", "Team", "alice", 1);
        let docs = folder("s2", "Docs", "bob", 2);
        let before = shown(&[team.clone(), docs.clone()]);
        assert_eq!(ListingDiff::of(&before, &before), ListingDiff::default());

        // A new share of the same name gets the suffix, the shown one keeps
        // its name (D125); Docs is revoked; a third share arrives.
        let team2 = folder("s3", "Team", "carol", 3);
        let mut viewer_to_editor = team.clone();
        viewer_to_editor.role = ShareRole::Editor;
        let file = entry("s4", "a.txt", "dave", 4, NodeKind::File);
        let after = next(&before, &[viewer_to_editor, team2.clone(), file.clone()]);
        let diff = ListingDiff::of(&before, &after);
        let mut added = diff.added.clone();
        added.sort();
        let mut expected = vec![share_key(&team2), share_key(&file)];
        expected.sort();
        assert_eq!(added, expected);
        assert_eq!(diff.removed, [share_key(&docs)]);
        assert!(diff.renamed.is_empty(), "{:?}", diff.renamed);
        assert_eq!(diff.roles, [(share_key(&team), ShareRole::Editor)]);
        assert_eq!(after[&share_key(&team)].name, "Team");
        assert_eq!(after[&share_key(&team2)].name, "Team (carol)");

        // The other share goes again: the names stay as they are.
        let back = next(&after, std::slice::from_ref(&team));
        let diff = ListingDiff::of(&after, &back);
        assert!(diff.renamed.is_empty(), "{:?}", diff.renamed);
        assert_eq!(diff.removed.len(), 2);

        // Its owner renames it: the new name shows.
        let mut renamed = team.clone();
        renamed.name = "Team 2026".into();
        let diff = ListingDiff::of(&back, &next(&back, &[renamed]));
        assert_eq!(diff.renamed, [(share_key(&team), "Team 2026".to_owned())]);
    }

    /// V-3, D125: a share that arrives later, or is renamed, with the name
    /// another share is shown under (a suffixed one included) gets the
    /// suffix; the share that had the name keeps it, across a remount too.
    #[test]
    fn a_shown_name_stays_with_its_share() {
        let alice = folder("s1", "Team", "alice@example.test", 1);
        let bob = folder("s2", "Team", "bob@example.test", 2);
        let other = folder("s3", "Notes", "mallory@example.test", 3);
        let first = shown(&[alice.clone(), bob.clone(), other.clone()]);
        assert_eq!(first[&share_key(&alice)].name, "Team (alice@example.test)");
        assert_eq!(first[&share_key(&bob)].name, "Team (bob@example.test)");

        // Mallory renames her share to Alice's shown name.
        let mut takeover = other.clone();
        takeover.name = "Team (alice@example.test)".into();
        let entries = [alice.clone(), bob.clone(), takeover.clone()];
        let after = next(&first, &entries);
        assert_eq!(after[&share_key(&alice)].name, "Team (alice@example.test)");
        assert_eq!(after[&share_key(&bob)].name, "Team (bob@example.test)");
        assert_eq!(
            after[&share_key(&takeover)].name,
            "Team (alice@example.test) (mallory@example.test)"
        );
        let diff = ListingDiff::of(&first, &after);
        assert_eq!(diff.renamed.len(), 1);
        assert_eq!(diff.renamed[0].0, share_key(&takeover));

        // A newcomer, older by its creation time, with a shown name.
        let newcomer = folder("s0", "Team (bob@example.test)", "eve@example.test", 0);
        let mut entries = entries.to_vec();
        entries.push(newcomer.clone());
        let later = next(&after, &entries);
        assert_eq!(later[&share_key(&bob)].name, "Team (bob@example.test)");
        assert_eq!(
            later[&share_key(&newcomer)].name,
            "Team (bob@example.test) (eve@example.test)"
        );

        // A remount: the names come from the kept file, not from the order
        // of the listing.
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("shared-names.json");
        save_names(&file, &kept_names(&later)).expect("save");
        let kept = load_names(&file);
        assert_eq!(kept, kept_names(&later));
        let (remounted, _) = select(entries.clone(), &BTreeSet::new(), &BTreeMap::new(), &kept);
        for (key, shown) in &later {
            assert_eq!(remounted[key].name, shown.name, "{key}");
        }
        // Without it the newcomer, created first, would have taken Bob's
        // name.
        let (fresh, _) = select(
            entries,
            &BTreeSet::new(),
            &BTreeMap::new(),
            &BTreeMap::new(),
        );
        assert_eq!(fresh[&share_key(&newcomer)].name, "Team (bob@example.test)");

        // A broken or foreign file names the shares anew.
        std::fs::write(&file, b"{not json").expect("write");
        assert!(load_names(&file).is_empty());
        assert!(load_names(&dir.path().join("missing.json")).is_empty());
    }

    #[test]
    fn only_folders_with_an_editor_or_admin_role_are_writable() {
        let mut share = folder("s1", "Team", "alice", 1);
        assert!(!may_write(&share), "viewer");
        share.role = ShareRole::Editor;
        assert!(may_write(&share));
        share.role = ShareRole::Admin;
        assert!(may_write(&share));
        let mut file = entry("s2", "a.txt", "bob", 2, NodeKind::File);
        file.role = ShareRole::Editor;
        assert!(!may_write(&file), "a shared single file stays read-only");
    }

    #[test]
    fn gone_shares_and_shares_over_the_limit_are_left_out() {
        let entries: Vec<SharedEntry> = (0..MAX_SHARES + 2)
            .map(|i| folder(&format!("s{i:03}"), &format!("n{i}"), "o", i as u64))
            .collect();
        let gone = BTreeSet::from(["s000".to_owned()]);
        let (shown, left_out) = select(entries.clone(), &gone, &BTreeMap::new(), &BTreeMap::new());
        assert_eq!(shown.len(), MAX_SHARES);
        assert!(!shown.contains_key(&share_key(&entries[0])), "gone");
        // The newest is left out, with a reason.
        assert_eq!(left_out.len(), 1);
        assert_eq!(left_out[0].0, format!("n{}", MAX_SHARES + 1));
        assert!(left_out[0].1.contains("at most"));
    }

    /// D119: a share whose root cannot be decrypted is not gone. An open
    /// one stays exactly as shown (no removal, no rename, no role change);
    /// one not shown yet stays out.
    #[test]
    fn a_degraded_share_is_kept_as_it_was() {
        use pdmount_backend::shared::DegradedShare;
        let mut team = folder("s1", "Team", "alice", 1);
        team.role = ShareRole::Editor;
        let docs = folder("s2", "Docs", "bob", 2);
        let new = folder("s3", "New", "carol", 3);
        let before = shown(&[team.clone(), docs.clone()]);
        let degraded = |entry: &SharedEntry| DegradedShare {
            share: entry.share.clone(),
            volume: entry.volume.clone(),
            root: entry.root.clone(),
        };
        let listing = SharedListing {
            entries: vec![docs.clone()],
            degraded: vec![degraded(&team), degraded(&new)],
        };
        let entries = with_degraded(listing, &before);
        let (after, _) = select(entries, &BTreeSet::new(), &before, &kept_names(&before));
        assert_eq!(ListingDiff::of(&before, &after), ListingDiff::default());
        assert_eq!(after[&share_key(&team)].entry.role, ShareRole::Editor);
        assert!(!after.contains_key(&share_key(&new)));
    }

    /// An over-long name is cut, not a reason to leave a share out; an open
    /// share whose new name cannot be a file name keeps its old one, and a
    /// new share with such a name is left out.
    #[test]
    fn long_names_are_cut_and_open_shares_keep_a_usable_name() {
        let long = format!("{}é", "n".repeat(254));
        let entries = [
            folder("s1", &long, "alice", 1),
            folder("s2", "Team", "bob", 2),
        ];
        let shown = shown(&entries);
        let cut = &shown[&share_key(&entries[0])].name;
        assert_eq!(cut, &"n".repeat(254), "cut on a character boundary");
        assert_eq!(shown.len(), 2);

        // Renamed by its owner to names that cannot be file names.
        for bad in ["a/b", "", "..", "nul\0"] {
            let renamed = [folder("s1", &long, "alice", 1), folder("s2", bad, "bob", 2)];
            let (after, _) = select(
                renamed.to_vec(),
                &BTreeSet::new(),
                &shown,
                &kept_names(&shown),
            );
            assert_eq!(after[&share_key(&renamed[1])].name, "Team", "{bad:?}");
            assert_eq!(ListingDiff::of(&shown, &after), ListingDiff::default());
            // Not open yet: left out.
            let (fresh, _) = select(
                renamed.to_vec(),
                &BTreeSet::new(),
                &BTreeMap::new(),
                &BTreeMap::new(),
            );
            assert!(!fresh.contains_key(&share_key(&renamed[1])), "{bad:?}");
        }
    }
}
