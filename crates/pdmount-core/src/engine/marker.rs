//! The pending marker (DESIGN.md §14.4, §14.5; D076).
//!
//! When a stop's grace runs out with changes left, the engine leaves
//! `<root>/.pdmount/<device id>.pending` in its tree's root, written
//! straight through the backend, not the journal: a small JSON record of
//! how many changes are still on this device, with no file names. The next
//! drain (the queue reaching empty) trashes it. Writing is opt-in
//! (`sync.pending_marker`); reading is on in every tree the user can write
//! (its write gate is open: My Files, an editor's or admin's share; D126):
//! after each event pass the engine reads the markers of other devices in
//! `.pdmount/`, at most [`MAX_MARKERS_READ`] of them, and keeps them for
//! `status` and one notification per pass that found new ones.
//!
//! A marker comes from another device, so it is untrusted input (D070): its
//! size is capped before it is fetched and parsed, its fields are checked
//! (a `time` that no clock can hold is refused), and the device name loses
//! control and formatting characters before anything shows it.
//!
//! In the mount `.pdmount` is read-only (EROFS, D019): see
//! [`in_marker_dir`] and [`is_marker_name`], used by the engine's write
//! checks, so `rsync --delete` on the root cannot remove it and the journal
//! never has a local change there to upload.

use super::Engine;
use crate::errors::{CoreError, Result};
use crate::health::Connectivity;
use crate::store::meta_set;
use crate::vfs::{Ino, ROOT_INO, Tree};
use bytes::Bytes;
use pdmount_backend::{BackendError, FileMeta, LinkId, NodeKind, RevisionId, UploadSource};
use serde::{Deserialize, Serialize};
use sha1::Digest;
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The folder in a tree's root that holds the markers.
pub const MARKER_DIR: &str = ".pdmount";
/// A marker is `<device id>.pending`.
pub const MARKER_SUFFIX: &str = ".pending";
/// The marker is written in the last this much of the grace.
pub const MARKER_WINDOW: Duration = Duration::from_secs(10);
/// The largest marker that is read; ours are about 100 bytes.
pub const MAX_MARKER_BYTES: usize = 4096;
/// A marker older than this is shown as stale and not announced (D076).
pub const STALE_AFTER: Duration = Duration::from_secs(7 * 24 * 3600);
/// At most this many markers of other devices are read per pass; the rest
/// are counted, logged once and not fetched (D126).
pub const MAX_MARKERS_READ: usize = 16;
/// Device names are cut to this many characters.
const MAX_DEVICE_CHARS: usize = 64;
const MARKER_VERSION: u64 = 1;
/// `meta` key: the link of this device's marker while it is on the drive.
const META_OWN: &str = "pending_marker";
/// `meta` key: the marker revisions already announced, as a JSON list.
const META_NOTIFIED: &str = "incoming_notified";
/// Markers are read after each event pass, and at least this often.
const SCAN_FALLBACK: Duration = Duration::from_secs(120);
/// How often the drain watcher looks at the queue.
const DRAIN_POLL: Duration = Duration::from_secs(1);
/// Wait after a failed trash of our marker.
const TRASH_RETRY: Duration = Duration::from_secs(30);
/// How often the end of a flush looks whether the drain watcher's trash
/// has finished.
const TRASH_BUSY_POLL: Duration = Duration::from_millis(50);

/// The JSON on the drive.
#[derive(Debug, Serialize, Deserialize)]
struct MarkerJson {
    version: u64,
    /// Left out in a share (M5.14): a host name is no business of the other
    /// members.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    device: Option<String>,
    /// Unix seconds when it was written.
    time: u64,
    /// Queue entries left on the device.
    entries: u64,
    /// Bytes of content left to upload.
    bytes: u64,
}

/// A marker another device left: changes that are on their way.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncomingMarker {
    /// The device id from the file name.
    pub device_id: String,
    /// Its name, cleaned for display; `None` when the marker has none.
    pub device: Option<String>,
    /// Unix seconds when the device wrote it.
    pub time: u64,
    pub entries: u64,
    pub bytes: u64,
}

impl IncomingMarker {
    /// The device name, or a short form of its id.
    pub fn device_label(&self) -> String {
        match &self.device {
            Some(name) => name.clone(),
            None => format!(
                "device {}",
                self.device_id.chars().take(8).collect::<String>()
            ),
        }
    }

    /// How old the marker is at `now` (zero when it is from the future, or
    /// when no clock can hold its time: [`parse`] refuses those).
    pub fn age(&self, now: SystemTime) -> Duration {
        written_at(self.time).map_or(Duration::ZERO, |written| {
            now.duration_since(written).unwrap_or_default()
        })
    }

    pub fn is_stale(&self, now: SystemTime) -> bool {
        self.age(now) > STALE_AFTER
    }
}

/// What a pass that found new markers announces: one notice per tree and
/// pass, whatever the number of markers (D126).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncomingNotice {
    /// The markers new since the last pass and not stale, by device id; at
    /// most [`MAX_MARKERS_READ`].
    pub markers: Vec<IncomingMarker>,
    /// Markers in the folder beyond [`MAX_MARKERS_READ`], not read.
    pub not_read: u64,
}

/// Unix seconds as a time; `None` when no `SystemTime` can hold them
/// (`UNIX_EPOCH + Duration` would panic, V-8).
fn written_at(time: u64) -> Option<SystemTime> {
    i64::try_from(time).ok()?;
    UNIX_EPOCH.checked_add(Duration::from_secs(time))
}

/// Why a marker was not taken.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MarkerError {
    #[error("larger than {MAX_MARKER_BYTES} bytes")]
    TooLarge,
    #[error("not one block")]
    Blocks,
    #[error("not a marker: {0}")]
    Invalid(String),
    #[error("unknown version {0}")]
    Version(u64),
}

/// The marker's content.
pub fn encode(device: Option<&str>, time: u64, entries: u64, bytes: u64) -> Vec<u8> {
    let json = MarkerJson {
        version: MARKER_VERSION,
        device: device.and_then(clean_device_name),
        time,
        entries,
        bytes,
    };
    serde_json::to_vec(&json).expect("a marker serializes")
}

/// Parses and checks a marker of `device_id` (from its file name).
pub fn parse(device_id: &str, data: &[u8]) -> std::result::Result<IncomingMarker, MarkerError> {
    if data.len() > MAX_MARKER_BYTES {
        return Err(MarkerError::TooLarge);
    }
    let json: MarkerJson = serde_json::from_slice(data).map_err(|error| {
        // serde_json's message names a position and a type, never the input.
        MarkerError::Invalid(error.to_string())
    })?;
    if json.version != MARKER_VERSION {
        return Err(MarkerError::Version(json.version));
    }
    if written_at(json.time).is_none() {
        return Err(MarkerError::Invalid("time out of range".into()));
    }
    Ok(IncomingMarker {
        device_id: device_id.to_owned(),
        device: json.device.as_deref().and_then(clean_device_name),
        time: json.time,
        entries: json.entries,
        bytes: json.bytes,
    })
}

/// A device name as it may be shown: control characters and the invisible
/// formatting ones (bidirectional overrides, zero-width marks) become `?`,
/// surrounding white space goes, and at most [`MAX_DEVICE_CHARS`] remain.
/// `None` when nothing is left.
pub fn clean_device_name(raw: &str) -> Option<String> {
    let cleaned: String = raw
        .chars()
        .map(|c| if is_hidden(c) { '?' } else { c })
        .collect();
    let cleaned: String = cleaned.trim().chars().take(MAX_DEVICE_CHARS).collect();
    let cleaned = cleaned.trim_end().to_owned();
    (!cleaned.is_empty()).then_some(cleaned)
}

/// Characters that a terminal or a notification would act on or hide.
fn is_hidden(c: char) -> bool {
    c.is_control()
        || matches!(c,
            '\u{00AD}'
            | '\u{061C}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{2028}'..='\u{202E}'
            | '\u{2060}'..='\u{206F}'
            | '\u{FEFF}'
            | '\u{FFF9}'..='\u{FFFB}')
}

/// The device id a marker's file name stands for: `<id>.pending`, the id in
/// its canonical form (lower-case, hyphenated).
pub fn marker_device_id(name: &str) -> Option<uuid::Uuid> {
    let id = name.strip_suffix(MARKER_SUFFIX)?;
    let parsed = uuid::Uuid::parse_str(id).ok()?;
    (parsed.hyphenated().to_string() == id).then_some(parsed)
}

/// The file name of a device's marker.
pub fn marker_name(device: &uuid::Uuid) -> String {
    format!("{}{MARKER_SUFFIX}", device.hyphenated())
}

/// Whether `name` in `parent` is the marker folder: it cannot be created,
/// removed or renamed in the mount.
pub fn is_marker_name(parent: Ino, name: &str) -> bool {
    parent == ROOT_INO && name == MARKER_DIR
}

/// Whether `ino` is the marker folder or below it: read-only in the mount.
pub fn in_marker_dir(tree: &Tree, ino: Ino) -> bool {
    // Most trees have no marker folder: one lookup in the root.
    let Some(dir) = tree
        .get(ROOT_INO)
        .and_then(|root| root.children.get(MARKER_DIR).copied())
    else {
        return false;
    };
    tree.is_within(ino, dir)
}

/// One marker in the marker folder, as last read.
#[derive(Debug, Clone)]
struct Seen {
    link: LinkId,
    rev: Option<RevisionId>,
    /// `None`: it failed the checks, and is not read again until it changes.
    marker: Option<IncomingMarker>,
}

impl Seen {
    fn key(&self) -> String {
        seen_key(&self.link, self.rev.as_ref())
    }
}

/// How a marker revision is remembered in `meta.incoming_notified`.
fn seen_key(link: &LinkId, rev: Option<&RevisionId>) -> String {
    format!("{link}/{}", rev.map_or("", |rev| rev.as_str()))
}

/// The engine's marker state.
pub(crate) struct Markers {
    /// This device's marker while it is on the drive, and a generation that
    /// each write bumps, so a trash does not forget a newer one.
    own: parking_lot::Mutex<Option<LinkId>>,
    own_generation: AtomicU64,
    own_changed: tokio::sync::Notify,
    /// Set while our marker is being trashed, by the drain watcher or at the
    /// end of a stop's flush, so the two never trash it at once.
    trashing: AtomicBool,
    /// Our markers trashed in this run: a listing may still show them.
    trashed: parking_lot::Mutex<HashSet<LinkId>>,
    /// Other devices' markers, by device id.
    seen: parking_lot::Mutex<BTreeMap<String, Seen>>,
    /// Marker revisions already announced (link/revision).
    notified: parking_lot::Mutex<HashSet<String>>,
    /// Markers beyond [`MAX_MARKERS_READ`] at the last pass, not read.
    not_read: AtomicU64,
    /// The marker leaves out the device name (a share's tree, D076).
    anonymous: AtomicBool,
    /// An event pass ended: read the markers.
    pub(crate) scan: tokio::sync::Notify,
    notices: tokio::sync::broadcast::Sender<IncomingNotice>,
}

impl Markers {
    pub(crate) fn load(store: &crate::store::Store) -> Result<Self> {
        let own = store
            .meta_get(META_OWN)?
            .filter(|link| !link.is_empty())
            .map(LinkId::new);
        let notified: HashSet<String> = store
            .meta_get(META_NOTIFIED)?
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        Ok(Self {
            own: parking_lot::Mutex::new(own),
            own_generation: AtomicU64::new(0),
            own_changed: tokio::sync::Notify::new(),
            trashing: AtomicBool::new(false),
            trashed: parking_lot::Mutex::default(),
            seen: parking_lot::Mutex::default(),
            notified: parking_lot::Mutex::new(notified),
            not_read: AtomicU64::new(0),
            anonymous: AtomicBool::new(false),
            scan: tokio::sync::Notify::new(),
            notices: tokio::sync::broadcast::channel(16).0,
        })
    }
}

/// What [`Engine::leave_pending_marker`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkerOutcome {
    /// Written: the entries and bytes it names.
    Written { entries: u64, bytes: u64 },
    /// Nothing was left to announce.
    Empty,
    /// The write failed or did not finish in time; logged.
    Failed,
}

impl Engine {
    /// Whether this tree leaves a marker when a stop's grace runs out
    /// (`sync.pending_marker`).
    pub fn pending_marker_enabled(&self) -> bool {
        self.config.sync.pending_marker
    }

    /// Leaves the device name out of this tree's marker: in a share it is
    /// no business of the other members (D076, D114).
    pub fn set_marker_anonymous(&self, anonymous: bool) {
        self.markers.anonymous.store(anonymous, Ordering::SeqCst);
    }

    /// Writes this device's marker with what is left in the queue, unless it
    /// is empty; gives up at `deadline` (D076).
    pub async fn leave_pending_marker(&self, deadline: Instant) -> MarkerOutcome {
        let counts = self.journal.counts();
        let entries = (counts.pending + counts.parked) as u64;
        if entries == 0 {
            return MarkerOutcome::Empty;
        }
        let bytes = self.pending_upload_bytes();
        let time = self.clock()()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs());
        let device =
            (!self.markers.anonymous.load(Ordering::SeqCst)).then_some(self.device.name.as_str());
        let body = encode(device, time, entries, bytes);
        let limit = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(limit, self.write_marker(Bytes::from(body))).await {
            Ok(Ok(link)) => {
                self.markers.own_generation.fetch_add(1, Ordering::SeqCst);
                self.markers.trashed.lock().remove(&link);
                self.set_own_marker(Some(link));
                tracing::info!(
                    entries,
                    bytes,
                    "left a pending marker for other devices in {MARKER_DIR}/"
                );
                MarkerOutcome::Written { entries, bytes }
            }
            Ok(Err(error)) => {
                tracing::warn!(%error, "cannot leave the pending marker");
                MarkerOutcome::Failed
            }
            Err(_) => {
                tracing::warn!("the pending marker was not written before the grace ran out");
                MarkerOutcome::Failed
            }
        }
    }

    /// Creates or updates `.pdmount/<device id>.pending` in the tree's root,
    /// straight through the backend.
    async fn write_marker(&self, body: Bytes) -> Result<LinkId> {
        let folder = self.marker_folder().await?;
        let name = marker_name(&self.device.id);
        let meta = FileMeta {
            mtime: self.clock()(),
            mode: Some(0o444),
            media_type: "application/json".into(),
            sha1: Some(sha1::Sha1::digest(&body).into()),
        };
        // The folder holds one small file per device.
        let existing = self
            .backend
            .list_children(&folder)
            .await?
            .into_iter()
            .find(|node| node.name == name && node.kind == NodeKind::File);
        let (link, base) = match existing {
            Some(node) => (node.link_id, node.active_revision),
            None => {
                let source = UploadSource::Bytes(body.clone());
                match self
                    .backend
                    .upload_new(&folder, &name, source, meta.clone())
                    .await
                {
                    Ok((link, _)) => return Ok(link),
                    Err(BackendError::AlreadyExists {
                        existing: Some(link),
                    }) => {
                        let node = self.backend.get_nodes(std::slice::from_ref(&link)).await?;
                        let rev = node
                            .into_iter()
                            .flatten()
                            .next()
                            .and_then(|n| n.active_revision);
                        (link, rev)
                    }
                    Err(error) => return Err(error.into()),
                }
            }
        };
        let base = base.ok_or_else(|| CoreError::Internal("the marker has no revision".into()))?;
        let upload = |base: RevisionId| {
            let source = UploadSource::Bytes(body.clone());
            let meta = meta.clone();
            let link = link.clone();
            async move {
                self.backend
                    .upload_revision(&link, &base, source, meta)
                    .await
            }
        };
        match upload(base).await {
            Ok(_) => Ok(link),
            Err(BackendError::RevisionConflict {
                current: Some(current),
            }) => {
                upload(current).await?;
                Ok(link)
            }
            Err(error) => Err(error.into()),
        }
    }

    /// The link of the marker folder, created when the drive has none.
    async fn marker_folder(&self) -> Result<LinkId> {
        let (root, known) = {
            let tree = self.tree.read();
            let root = tree
                .link_of(ROOT_INO)
                .cloned()
                .ok_or_else(|| CoreError::Internal("the root has no link".into()))?;
            let known = tree
                .get(ROOT_INO)
                .and_then(|node| node.children.get(MARKER_DIR).copied())
                .and_then(|ino| tree.get(ino))
                .filter(|node| node.is_dir())
                .and_then(|node| node.link().cloned());
            (root, known)
        };
        if let Some(link) = known {
            // It may have been trashed since the tree saw it.
            match self
                .backend
                .get_nodes(std::slice::from_ref(&link))
                .await?
                .first()
            {
                Some(Some(node))
                    if !node.trashed
                        && node.kind == NodeKind::Folder
                        && node.parent_link_id.as_ref() == Some(&root) =>
                {
                    return Ok(link);
                }
                _ => {}
            }
        }
        match self
            .backend
            .create_folder(&root, MARKER_DIR, self.clock()())
            .await
        {
            Ok(link) => Ok(link),
            Err(BackendError::AlreadyExists {
                existing: Some(link),
            }) => Ok(link),
            Err(BackendError::AlreadyExists { existing: None }) => self
                .backend
                .list_children(&root)
                .await?
                .into_iter()
                .find(|node| node.name == MARKER_DIR && node.kind == NodeKind::Folder)
                .map(|node| node.link_id)
                .ok_or_else(|| {
                    CoreError::Internal(format!("{MARKER_DIR} exists, but is no folder"))
                }),
            Err(error) => Err(error.into()),
        }
    }

    fn set_own_marker(&self, link: Option<LinkId>) {
        let value = link.as_ref().map(|link| link.0.clone()).unwrap_or_default();
        *self.markers.own.lock() = link;
        self.store
            .write(move |conn| meta_set(conn, META_OWN, &value));
        self.markers.own_changed.notify_one();
    }

    /// This device's marker while it is on the drive.
    pub fn own_pending_marker(&self) -> Option<LinkId> {
        self.markers.own.lock().clone()
    }

    /// Markers of other devices, by device id.
    pub fn incoming_markers(&self) -> Vec<IncomingMarker> {
        self.markers
            .seen
            .lock()
            .values()
            .filter_map(|seen| seen.marker.clone())
            .collect()
    }

    /// Markers in the folder beyond [`MAX_MARKERS_READ`] at the last pass:
    /// not read, so not in [`Self::incoming_markers`].
    pub fn incoming_not_read(&self) -> u64 {
        self.markers.not_read.load(Ordering::SeqCst)
    }

    /// Receives one notice per pass that found markers of other devices
    /// not announced before (a new revision counts as new); stale ones are
    /// not sent (D126).
    pub fn subscribe_incoming(&self) -> tokio::sync::broadcast::Receiver<IncomingNotice> {
        self.markers.notices.subscribe()
    }

    /// The reader: after each event pass, and at least every
    /// [`SCAN_FALLBACK`], reads the marker folder. A pass that panics is
    /// logged and the next one runs: input from another device must never
    /// end the reading (V-8).
    pub(crate) async fn run_markers(self: Arc<Self>) {
        use futures::FutureExt;
        loop {
            tokio::select! {
                () = self.markers.scan.notified() => {}
                () = tokio::time::sleep(SCAN_FALLBACK) => {}
            }
            match std::panic::AssertUnwindSafe(self.scan_markers())
                .catch_unwind()
                .await
            {
                Ok(Ok(())) => {}
                Ok(Err(error)) => tracing::debug!(%error, "reading the pending markers failed"),
                Err(_) => tracing::error!("reading the pending markers panicked; trying again"),
            }
        }
    }

    /// Reads the marker folder: lists it once (events keep it current
    /// after that), fetches markers that are new or changed, at most
    /// [`MAX_MARKERS_READ`], forgets those that are gone, and announces new
    /// ones. A tree whose write gate is closed (a viewer's share) reads
    /// none (D126).
    pub(crate) async fn scan_markers(self: &Arc<Self>) -> Result<()> {
        if !self.write_gate_open() {
            self.forget_seen();
            return Ok(());
        }
        if self.health.state() != Connectivity::Online {
            return Ok(());
        }
        // A root never listed has not been used: the first use lists it,
        // and this pass waits for that rather than making the call.
        let root_listed = self.tree.read().get(ROOT_INO).is_some_and(|n| n.listed);
        if !root_listed {
            return Ok(());
        }
        let dir = {
            let tree = self.tree.read();
            tree.child(ROOT_INO, MARKER_DIR)
                .ok()
                .flatten()
                .filter(|&ino| {
                    tree.get(ino)
                        .is_some_and(|node| node.is_dir() && node.remote.is_some())
                })
                .map(|ino| (ino, tree.get(ino).is_some_and(|node| node.listed)))
        };
        let Some((dir, listed)) = dir else {
            self.replace_seen(BTreeMap::new(), &HashSet::new(), 0);
            return Ok(());
        };
        if !listed {
            let _turn = self.gate.background().await;
            self.ensure_listed_now(dir).await?;
        }
        let mut files: Vec<(uuid::Uuid, LinkId, Option<RevisionId>, u64)> = {
            let tree = self.tree.read();
            let Some(node) = tree.get(dir) else {
                return Ok(());
            };
            node.children
                .iter()
                .filter_map(|(name, &ino)| {
                    let id = marker_device_id(name)?;
                    let child = tree.get(ino)?;
                    let remote = child.remote.as_ref()?;
                    (child.kind == NodeKind::File)
                        .then(|| (id, remote.link.clone(), remote.rev.clone(), remote.size))
                })
                .collect()
        };
        if let Some(index) = files.iter().position(|(id, ..)| *id == self.device.id) {
            let (_, link, _, _) = files.swap_remove(index);
            self.adopt_own_marker(link);
        }
        let previous = self.markers.seen.lock().clone();
        // The cap (D126): devices already known first, so markers planted
        // later cannot push them out, then by id.
        files.sort_by_cached_key(|(id, ..)| {
            (!previous.contains_key(&id.hyphenated().to_string()), *id)
        });
        let not_read = files.len().saturating_sub(MAX_MARKERS_READ) as u64;
        files.truncate(MAX_MARKERS_READ);
        let mut seen = BTreeMap::new();
        // Markers that could not be fetched now: what was announced of them
        // is kept, so a transient error does not announce them again.
        let mut unread = HashSet::new();
        for (id, link, rev, size) in files {
            let device_id = id.hyphenated().to_string();
            if let Some(known) = previous.get(&device_id)
                && known.link == link
                && known.rev == rev
            {
                seen.insert(device_id, known.clone());
                continue;
            }
            let marker = match self
                .fetch_marker(&device_id, &link, rev.as_ref(), size)
                .await
            {
                Ok(marker) => Some(marker),
                Err(FetchError::Marker(error)) => {
                    tracing::warn!(device_id, %error, "ignoring a pending marker");
                    None
                }
                Err(FetchError::Backend(error)) => {
                    // Read again at the next pass.
                    tracing::debug!(device_id, %error, "cannot read a pending marker");
                    unread.insert(seen_key(&link, rev.as_ref()));
                    if let Some(known) = previous.get(&device_id) {
                        seen.insert(device_id, known.clone());
                    }
                    continue;
                }
            };
            seen.insert(device_id, Seen { link, rev, marker });
        }
        self.replace_seen(seen, &unread, not_read);
        Ok(())
    }

    /// The write gate is closed: no marker is shown. What was announced
    /// stays remembered, so a gate that opens again does not announce the
    /// same markers twice.
    fn forget_seen(&self) {
        self.markers.seen.lock().clear();
        self.markers.not_read.store(0, Ordering::SeqCst);
    }

    /// Our own marker in a listing: the drain watcher trashes it, unless it
    /// was trashed already in this run.
    fn adopt_own_marker(&self, link: LinkId) {
        if self.markers.trashed.lock().contains(&link) {
            return;
        }
        let known = self.markers.own.lock().is_some();
        if !known {
            self.set_own_marker(Some(link));
        }
    }

    /// Replaces what is known of other devices' markers; announces those
    /// not announced before, in one notice, and remembers them in the
    /// store. The revisions in `unread` are on the drive but could not be
    /// fetched: they stay remembered as announced if they were. The cap
    /// left `not_read` markers out.
    fn replace_seen(&self, seen: BTreeMap<String, Seen>, unread: &HashSet<String>, not_read: u64) {
        let now = self.clock()();
        let mut keys: HashSet<String> = seen.values().map(Seen::key).collect();
        keys.extend(unread.iter().cloned());
        let mut fresh = Vec::new();
        let changed = {
            let mut notified = self.markers.notified.lock();
            let before = notified.len();
            notified.retain(|key| keys.contains(key));
            let mut changed = notified.len() != before;
            for entry in seen.values() {
                let Some(marker) = &entry.marker else {
                    continue;
                };
                if notified.insert(entry.key()) {
                    changed = true;
                    if !marker.is_stale(now) {
                        fresh.push(marker.clone());
                    }
                }
            }
            changed.then(|| {
                let mut list: Vec<&String> = notified.iter().collect();
                list.sort();
                serde_json::to_string(&list).unwrap_or_default()
            })
        };
        *self.markers.seen.lock() = seen;
        if let Some(value) = changed {
            self.store
                .write(move |conn| meta_set(conn, META_NOTIFIED, &value));
        }
        let before = self.markers.not_read.swap(not_read, Ordering::SeqCst);
        if not_read > 0 && not_read != before {
            tracing::warn!(
                not_read,
                cap = MAX_MARKERS_READ,
                "more pending markers than are read; the rest are ignored"
            );
        }
        for marker in &fresh {
            tracing::info!(
                device_id = marker.device_id,
                device = marker.device_label(),
                entries = marker.entries,
                bytes = marker.bytes,
                "another device has changes on their way (pending marker)"
            );
        }
        if !fresh.is_empty() {
            // No subscriber is not an error.
            let _ = self.markers.notices.send(IncomingNotice {
                markers: fresh,
                not_read,
            });
        }
    }

    /// Fetches and checks one marker: its size is checked against the
    /// listing first, and the fetch cannot return more than the cap.
    async fn fetch_marker(
        &self,
        device_id: &str,
        link: &LinkId,
        rev: Option<&RevisionId>,
        size: u64,
    ) -> std::result::Result<IncomingMarker, FetchError> {
        if size > MAX_MARKER_BYTES as u64 {
            return Err(FetchError::Marker(MarkerError::TooLarge));
        }
        let rev = rev.ok_or(FetchError::Marker(MarkerError::Blocks))?;
        let _turn = self.gate.background().await;
        let blocks = self.backend.blocks(link, rev).await?;
        if blocks.len() != 1 {
            return Err(FetchError::Marker(MarkerError::Blocks));
        }
        let mut buf = vec![0; MAX_MARKER_BYTES];
        let len = match self
            .backend
            .fetch_block_into(link, rev, blocks[0].index, &mut buf)
            .await
        {
            Ok(len) => len,
            // A block longer than the buffer.
            Err(BackendError::Integrity(_)) => {
                return Err(FetchError::Marker(MarkerError::TooLarge));
            }
            Err(error) => return Err(error.into()),
        };
        parse(device_id, &buf[..len]).map_err(FetchError::Marker)
    }

    /// The drain watcher: once our marker is on the drive and the queue is
    /// empty (the next drain, D076), trashes it.
    pub(crate) async fn run_own_marker(self: Arc<Self>) {
        loop {
            let changed = self.markers.own_changed.notified();
            let Some(link) = self.own_pending_marker() else {
                changed.await;
                continue;
            };
            let counts = self.journal.counts();
            if counts.pending + counts.parked > 0 {
                tokio::time::sleep(DRAIN_POLL).await;
                continue;
            }
            self.health.wait_online().await;
            match self.trash_own_marker_once(&link).await {
                // Trashed, or the queue filled again meanwhile.
                Ok(Some(_)) => {}
                // The end of a flush is trashing it.
                Ok(None) => tokio::time::sleep(DRAIN_POLL).await,
                Err(error) => {
                    tracing::warn!(%error, "cannot trash this device's pending marker; trying again");
                    tokio::time::sleep(TRASH_RETRY).await;
                }
            }
        }
    }

    /// At the end of a stop's flush that left a marker and then reached its
    /// barrier: trashes the marker if the queue is empty, by `deadline`,
    /// since the drain watcher stops with the engine (D096). Returns
    /// whether no marker of this device is known any more.
    pub async fn trash_pending_marker_by(&self, deadline: Instant) -> bool {
        let trash = async {
            loop {
                let Some(link) = self.own_pending_marker() else {
                    return true;
                };
                let counts = self.journal.counts();
                if counts.pending + counts.parked > 0 || self.health.state() != Connectivity::Online
                {
                    return false;
                }
                match self.trash_own_marker_once(&link).await {
                    Ok(Some(true)) => {}
                    Ok(Some(false)) => return false,
                    Ok(None) => tokio::time::sleep(TRASH_BUSY_POLL).await,
                    Err(error) => {
                        tracing::warn!(%error, "cannot trash this device's pending marker; the next mount does");
                        return false;
                    }
                }
            }
        };
        let limit = deadline.saturating_duration_since(Instant::now());
        tokio::time::timeout(limit, trash).await.unwrap_or_else(|_| {
            tracing::info!(
                "the pending marker was not trashed before the grace ran out; the next mount does"
            );
            false
        })
    }

    /// One trash of our marker, unless one is running already (`None`);
    /// `false` when the queue filled again meanwhile. A trashed marker is
    /// forgotten unless a newer one was written meanwhile.
    async fn trash_own_marker_once(&self, link: &LinkId) -> Result<Option<bool>> {
        let Some(_trashing) = Trashing::take(&self.markers.trashing) else {
            return Ok(None);
        };
        let generation = self.markers.own_generation.load(Ordering::SeqCst);
        match self.trash_own_marker(link).await? {
            OwnTrash::Refilled => return Ok(Some(false)),
            OwnTrash::Trashed => {
                tracing::info!("the queue drained; trashed this device's pending marker");
                self.markers.trashed.lock().insert(link.clone());
            }
            OwnTrash::NotOurs => tracing::warn!(
                "the link known as this device's pending marker was renamed or moved; \
                 forgotten, not trashed"
            ),
        }
        if self.markers.own_generation.load(Ordering::SeqCst) == generation {
            self.set_own_marker(None);
        }
        Ok(Some(true))
    }

    /// Trashes our marker. A marker that is gone or trashed already (by
    /// another client, or by an earlier run whose trash the tree has not
    /// seen yet) counts as trashed. A link that is no longer a file named
    /// `<this device id>.pending` in the root's `.pdmount` is not trashed:
    /// a link adopted from a listing may have been renamed or moved since
    /// (V-4).
    async fn trash_own_marker(&self, link: &LinkId) -> Result<OwnTrash> {
        let _turn = self.gate.background().await;
        let counts = self.journal.counts();
        if counts.pending + counts.parked > 0 {
            return Ok(OwnTrash::Refilled);
        }
        let node = self
            .backend
            .get_nodes(std::slice::from_ref(link))
            .await?
            .into_iter()
            .flatten()
            .next();
        let Some(node) = node.filter(|node| !node.trashed) else {
            return Ok(OwnTrash::Trashed);
        };
        if !self.is_own_marker_node(&node).await? {
            return Ok(OwnTrash::NotOurs);
        }
        let results = self.backend.trash(std::slice::from_ref(link)).await?;
        match results.into_iter().next() {
            Some(Ok(())) | Some(Err(BackendError::NotFound)) => Ok(OwnTrash::Trashed),
            Some(Err(error)) => Err(error.into()),
            None => Err(CoreError::Internal("the trash returned no result".into())),
        }
    }

    /// Whether `node` is this device's marker: a file of its name whose
    /// parent is the marker folder in the tree's root.
    async fn is_own_marker_node(&self, node: &pdmount_backend::RemoteNode) -> Result<bool> {
        if node.kind != NodeKind::File || node.name != marker_name(&self.device.id) {
            return Ok(false);
        }
        let Some(parent) = &node.parent_link_id else {
            return Ok(false);
        };
        let root = self
            .tree
            .read()
            .link_of(ROOT_INO)
            .cloned()
            .ok_or_else(|| CoreError::Internal("the root has no link".into()))?;
        let folder = self
            .backend
            .get_nodes(std::slice::from_ref(parent))
            .await?
            .into_iter()
            .flatten()
            .next();
        Ok(folder.is_some_and(|folder| {
            !folder.trashed
                && folder.kind == NodeKind::Folder
                && folder.name == MARKER_DIR
                && folder.parent_link_id.as_ref() == Some(&root)
        }))
    }
}

/// What [`Engine::trash_own_marker`] did.
enum OwnTrash {
    /// Trashed, or gone already.
    Trashed,
    /// The queue filled again meanwhile: kept.
    Refilled,
    /// The link is no longer this device's marker: kept, and forgotten.
    NotOurs,
}

/// Holds [`Markers::trashing`]; clears it when dropped, also when the trash
/// is cancelled at the deadline.
struct Trashing<'a>(&'a AtomicBool);

impl<'a> Trashing<'a> {
    fn take(flag: &'a AtomicBool) -> Option<Self> {
        flag.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .ok()
            .map(|_| Self(flag))
    }
}

impl Drop for Trashing<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// Why [`Engine::fetch_marker`] failed.
#[derive(Debug)]
enum FetchError {
    /// The marker itself is wrong: not read again until it changes.
    Marker(MarkerError),
    /// The fetch failed: read again at the next pass.
    Backend(BackendError),
}

impl From<BackendError> for FetchError {
    fn from(error: BackendError) -> Self {
        Self::Backend(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "0b5f6c1e-6a7d-4c2b-9f1e-2a3b4c5d6e7f";

    #[test]
    fn a_marker_round_trips_and_names_no_files() {
        let data = encode(Some("laptop"), 1_792_663_200, 3, 25_165_824);
        let text = std::str::from_utf8(&data).expect("utf-8");
        assert_eq!(
            text,
            r#"{"version":1,"device":"laptop","time":1792663200,"entries":3,"bytes":25165824}"#
        );
        let marker = parse(ID, &data).expect("parse");
        assert_eq!(
            marker,
            IncomingMarker {
                device_id: ID.into(),
                device: Some("laptop".into()),
                time: 1_792_663_200,
                entries: 3,
                bytes: 25_165_824,
            }
        );
        // In a share the name is left out (M5.14).
        let data = encode(None, 1, 1, 0);
        assert!(
            !std::str::from_utf8(&data)
                .expect("utf-8")
                .contains("device")
        );
        assert_eq!(
            parse(ID, &data).expect("parse").device_label(),
            "device 0b5f6c1e"
        );
    }

    #[test]
    fn markers_over_the_cap_or_malformed_are_refused() {
        let mut big = br#"{"version":1,"time":1,"entries":1,"bytes":1,"device":""#.to_vec();
        big.extend(std::iter::repeat_n(b'a', MAX_MARKER_BYTES));
        big.extend(br#""}"#);
        assert_eq!(parse(ID, &big), Err(MarkerError::TooLarge));
        for bad in [
            &b""[..],
            b"not json",
            b"[]",
            br#"{"version":1}"#,
            br#"{"version":1,"time":-1,"entries":1,"bytes":1}"#,
            br#"{"version":1,"time":1,"entries":"many","bytes":1}"#,
            br#"{"version":1,"time":1,"entries":1,"bytes":1,"device":7}"#,
        ] {
            assert!(
                matches!(parse(ID, bad), Err(MarkerError::Invalid(_))),
                "{:?}",
                String::from_utf8_lossy(bad)
            );
        }
        assert_eq!(
            parse(ID, br#"{"version":2,"time":1,"entries":1,"bytes":1}"#),
            Err(MarkerError::Version(2))
        );
        // A time no clock can hold (V-8): refused, not a panic later.
        for time in [u64::MAX, i64::MAX as u64 + 1] {
            let data = format!(r#"{{"version":1,"time":{time},"entries":1,"bytes":1}}"#);
            assert_eq!(
                parse(ID, data.as_bytes()),
                Err(MarkerError::Invalid("time out of range".into())),
                "{time}"
            );
        }
        // Fields a later version adds are ignored.
        assert!(
            parse(
                ID,
                br#"{"version":1,"time":1,"entries":1,"bytes":1,"extra":[1]}"#
            )
            .is_ok()
        );
    }

    #[test]
    fn device_names_lose_control_and_hidden_characters() {
        let raw = "evil\u{1b}[2J\u{7}\u{202e}exe.txt\r\nname";
        let data = serde_json::to_vec(&serde_json::json!({
            "version": 1, "device": raw, "time": 1, "entries": 1, "bytes": 1
        }))
        .expect("json");
        let marker = parse(ID, &data).expect("parse");
        let name = marker.device.expect("a name");
        assert_eq!(name, "evil?[2J??exe.txt??name");
        assert!(!name.chars().any(is_hidden));
        assert_eq!(clean_device_name("  \u{200b} "), Some("?".into()));
        assert_eq!(clean_device_name(" \t "), Some("?".into()));
        assert_eq!(clean_device_name("   "), None);
        assert_eq!(
            clean_device_name(&"x".repeat(200)).map(|n| n.chars().count()),
            Some(MAX_DEVICE_CHARS)
        );
    }

    #[test]
    fn only_canonical_device_ids_name_a_marker() {
        let id = uuid::Uuid::parse_str(ID).expect("uuid");
        assert_eq!(marker_name(&id), format!("{ID}.pending"));
        assert_eq!(marker_device_id(&format!("{ID}.pending")), Some(id));
        for name in [
            ID.to_owned(),
            format!("{}.pending", ID.to_uppercase()),
            format!("{}.pending", ID.replace('-', "")),
            format!("{ID}.pending.tmp"),
            "notes.pending".to_owned(),
            ".pending".to_owned(),
        ] {
            assert_eq!(marker_device_id(&name), None, "{name}");
        }
    }

    #[test]
    fn age_and_staleness() {
        let marker = parse(ID, &encode(Some("a"), 1_000_000, 1, 1)).expect("parse");
        let at = |secs| UNIX_EPOCH + Duration::from_secs(secs);
        assert_eq!(marker.age(at(1_000_060)), Duration::from_secs(60));
        assert_eq!(marker.age(at(999_000)), Duration::ZERO, "from the future");
        assert!(!marker.is_stale(at(1_000_000 + STALE_AFTER.as_secs())));
        assert!(marker.is_stale(at(1_000_001 + STALE_AFTER.as_secs())));
        // A marker that came another way than `parse` (the control socket)
        // with a time out of range: age zero, never a panic (V-8).
        for time in [u64::MAX, i64::MAX as u64 + 1, i64::MAX as u64] {
            let far = IncomingMarker {
                time,
                ..marker.clone()
            };
            assert_eq!(far.age(at(1_000_000)), Duration::ZERO, "{time}");
            assert!(!far.is_stale(at(1_000_000)));
        }
    }
}
