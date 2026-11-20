//! In-memory model of Proton Drive "My Files" for tests (DESIGN.md §15.1).
//!
//! It enforces the server rules PdMount relies on: unique names per folder,
//! the base-revision precondition of new revisions, the name-hash
//! precondition of renames, trash semantics, quota, and a paged event
//! stream with `more` and `refresh`. Clones share one drive, so two clones
//! act as two devices. One-shot failures can be injected per operation.
//!
//! [`FakeBackend::open`] persists the drive to a directory, so two daemons
//! can share one fake (`pdmount mount --backend fake:<dir>`):
//! - `snapshot.json`: the whole state as of a generation;
//! - `journal.jsonl`: one line per change after it (the nodes it touched,
//!   new events and changes, settings), appended before the call returns,
//!   so a killed daemon loses nothing; compacted into a new snapshot when
//!   it grows;
//! - `blobs/<revision>`: content; `epoch`: bumped by every compaction;
//! - `lock`: every operation holds an exclusive `flock` and first applies
//!   what other processes appended.
//!
//! Clones share one drive but can act as different clients
//! ([`FakeBackend::with_client`]): a client has a UID, and may leave out the
//! SHA-1 and the mode the way the official apps do. Tests can stop the next
//! upload, folder creation, rename or trash of a client just before its
//! commit ([`FakeBackend::pause_at`]) and switch the server's commit-time
//! checks off. For a daemon running in another process, `faults.json` in
//! the fake's directory makes calls fail as if offline or signed out, or
//! makes node fetches fail as if a node could not be decrypted.
//!
//! A network profile ([`NetProfile`]; `profile.json` in the fake's
//! directory for running daemons, [`FakeBackend::set_profile`] in memory)
//! gives every call round trips and every transfer a share of one pipe per
//! direction. The cost is slept after the fault checks and before the call
//! changes or reads the drive, outside the state lock.
//!
//! Several volumes and shares (M5.10, D110): the drive's own volume
//! ([`MAIN_VOLUME`]) holds My Files; [`FakeBackend::add_root`] adds a root
//! folder without a parent, on the drive's own volume (as a device's) or on
//! another user's volume, optionally shared with the drive's user in a
//! role ([`FakeRole`]). A handle made by [`FakeBackend::tree`] serves one
//! such tree like `ProtonBackend` does: `account` and `root` name the tree's
//! root, and `latest_event_id` and `poll_events` follow that volume's events
//! only (event ids stay absolute over all volumes). A handle acts as the
//! drive's user unless [`FakeClient::user`] names another; nodes on another
//! user's volume can be read only through a share with that user as a
//! member, and changed only as an `Editor` or `Admin`
//! ([`BackendError::PermissionDenied`] otherwise; unreachable nodes read as
//! not found). Moves between volumes are [`BackendError::InvalidMove`].
//!
//! Photos (M5.13, D113): the drive user's Photos volume ([`PHOTOS_VOLUME`])
//! has a root folder holding the photos ([`FakeBackend::add_photo`]), each
//! with a capture time, tags and, for a related photo, its main photo. The
//! [`Photos`] timeline lists them newest first, 500 per page; a handle from
//! [`FakeBackend::photos_tree`] reads them and polls their events.
//!
//! Growth is bounded for long runs (fsx, the scenario matrix): files keep
//! their last revisions (`PDMOUNT_FAKE_KEEP_REVISIONS`, default 16), the
//! most recent trashed nodes are kept, and the event and change logs are
//! trimmed, keeping absolute sequence numbers. Injected failures,
//! latency and the network's pipes stay per process.

use crate::block_lists::{BlockLists, Listed, Page, PageSource, Revision as ListedRevision};
use crate::photos::{
    MAX_THUMBNAIL_BYTES, Photos, PhotosVolume, RelatedPhoto, THUMBNAIL_BATCH, TIMELINE_PAGE,
    ThumbnailRef, TimelineItem,
};
use crate::shared::{CorePage, DegradedShare, ShareRole, SharedEntry, SharedListing, SharedWithMe};
use crate::{
    AccountInfo, Backend, BackendError, BlockInfo, EventId, EventKind, EventPage, FileMeta, LinkId,
    Listing, NameHash, NodeKind, NodeLookup, RemoteEvent, RemoteNode, Result, RevisionId,
    RevisionMeta, SignaturePart, StorageInfo, Unverified, UploadSource,
};
use bytes::Bytes;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sha1::Digest;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::watch;

/// Revisions kept per file unless configured otherwise.
const DEFAULT_KEEP_REVISIONS: usize = 16;
/// Trashed nodes kept (their content stays readable, as in Proton's trash).
const KEEP_TRASHED: usize = 1024;
/// Events and changes kept; older ones are trimmed in batches.
const KEEP_EVENTS: usize = 10_000;
const KEEP_CHANGES: usize = 50_000;
/// Journal size that triggers a compaction.
const COMPACT_BYTES: u64 = 8 << 20;
/// Children per listing page, and ids per `get_nodes` batch: each costs a
/// round trip under a network profile.
const NET_PAGE: usize = 150;
/// Round trips of an upload: draft, blocks, commit.
const UPLOAD_RTTS: u32 = 3;

/// Operations of the [`Backend`] trait, for fault injection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FakeOp {
    Account,
    Root,
    ListChildren,
    GetNodes,
    Blocks,
    FetchBlock,
    UploadNew,
    UploadRevision,
    CreateFolder,
    Rename,
    Trash,
    Revisions,
    LatestEventId,
    PollEvents,
    StorageInfo,
    /// The shared-with-me listing ([`SharedWithMe`]).
    SharedWithMe,
    /// The core event stream ([`SharedWithMe`]).
    CoreEvents,
    /// The Photos volume and its timeline ([`Photos`]).
    Timeline,
    /// A photo's thumbnail: its reference or a batch of downloads
    /// ([`Photos::thumbnail_ref`], [`Photos::fetch_thumbnails`]).
    Thumbnails,
}

/// Who is calling: handles made by [`FakeBackend::with_client`] differ in
/// what their uploads record, like the daemon and the official apps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeClient {
    /// The account and root of an earlier online run: `account` then
    /// answers from it without a request, like `ProtonBackend::with_root`,
    /// so a daemon can start while the drive is unreachable (§9.6).
    pub known_account: Option<AccountInfo>,
    /// The SDK client UID (`pdmount-<device id>`); `None` for tests that
    /// do not care.
    pub uid: Option<String>,
    /// Uploads carry a SHA-1 that reads report.
    pub claims_sha1: bool,
    /// Uploads carry `pdmount.mode` (the official apps leave it out).
    pub claims_mode: bool,
    /// The user the handle acts as; `None`: the drive's user.
    pub user: Option<String>,
    /// The tree the handle serves ([`FakeBackend::tree`]); `None`: My Files.
    pub tree: Option<FakeTree>,
}

impl Default for FakeClient {
    fn default() -> Self {
        Self {
            known_account: None,
            uid: None,
            claims_sha1: true,
            claims_mode: true,
            user: None,
            tree: None,
        }
    }
}

/// The volume of My Files on every fake drive (`proton-root.json` records
/// it as the volume of a fake profile).
pub const MAIN_VOLUME: &str = "fake";

/// The volume of the drive user's Photos ([`FakeBackend::add_photo`]).
pub const PHOTOS_VOLUME: &str = "photos";

/// A member's role in a share, as the server reports it on the share's root.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FakeRole {
    Viewer,
    Editor,
    Admin,
}

impl FakeRole {
    /// Whether the role may change the share's content.
    pub fn may_write(self) -> bool {
        matches!(self, Self::Editor | Self::Admin)
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "viewer" => Some(Self::Viewer),
            "editor" => Some(Self::Editor),
            "admin" => Some(Self::Admin),
            _ => None,
        }
    }
}

/// A tree a handle serves: a volume and a root folder on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FakeTree {
    pub volume: String,
    pub root: LinkId,
}

/// A volume other than [`MAIN_VOLUME`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct FakeVolume {
    owner: String,
}

/// A share: a folder or a file on a volume, with its members' roles.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct FakeShare {
    volume: String,
    root: LinkId,
    members: BTreeMap<String, FakeRole>,
    /// Creation order (the id counter at creation); orders the listing.
    #[serde(default)]
    created: u64,
}

/// What [`FakeBackend::add_root`] made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddedRoot {
    pub volume: String,
    pub root: LinkId,
    /// The share with the drive's user, when the root is another user's.
    pub share: Option<String>,
}

/// An event as kept: the volume it belongs to, `None` for [`MAIN_VOLUME`]
/// (the only one events of older fakes have).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct FakeEvent {
    #[serde(flatten)]
    event: RemoteEvent,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    volume: Option<String>,
}

impl FakeEvent {
    fn volume(&self) -> &str {
        self.volume.as_deref().unwrap_or(MAIN_VOLUME)
    }
}

/// Where a call can be stopped, just before the server applies it (or,
/// for a listing, reads the folder).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FakePoint {
    NewBeforeCommit,
    RevisionBeforeCommit,
    CreateFolderBeforeCommit,
    RenameBeforeCommit,
    TrashBeforeCommit,
    ListBeforeRead,
}

#[derive(Debug)]
struct PauseChannels {
    reached: watch::Sender<bool>,
    release: watch::Sender<bool>,
}

/// A stopped call, as seen by the test: wait for `reached`, change the
/// drive meanwhile, then `release`.
#[derive(Debug, Clone)]
pub struct FakePause(Arc<PauseChannels>);

impl FakePause {
    /// Returns once the call is stopped at its point.
    pub async fn reached(&self) {
        let mut reached = self.0.reached.subscribe();
        let _ = reached.wait_for(|reached| *reached).await;
    }

    /// Lets the call go on (also if it has not arrived yet).
    pub fn release(&self) {
        self.0.release.send_replace(true);
    }
}

struct PendingPause {
    point: FakePoint,
    client: Option<String>,
    channels: Arc<PauseChannels>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FaultKind {
    /// Every call fails with `Offline("injected")`.
    Offline,
    /// Every call fails with `Unauthorized`.
    Unauthorized,
    /// Every node fetch (`get_nodes`) fails with an error that is not
    /// transient, as the SDK's batch did for a node under a folder that
    /// cannot be decrypted (M6.0.3 V-3); other calls go through.
    BrokenNodes,
}

/// One rule of `faults.json`, for daemons in other processes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FaultRule {
    pub kind: FaultKind,
    /// Unix milliseconds; `None` = until cleared.
    #[serde(default)]
    pub until_ms: Option<u64>,
    /// Client UID the rule applies to; `None` = all clients.
    #[serde(default)]
    pub client: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct FaultFile {
    #[serde(default)]
    rules: Vec<FaultRule>,
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

fn read_fault_file(path: &Path) -> Vec<FaultRule> {
    match std::fs::read(path) {
        Ok(json) => serde_json::from_slice::<FaultFile>(&json)
            .map(|file| file.rules)
            .unwrap_or_else(|error| {
                tracing::warn!(%error, "fake backend: ignoring a malformed faults.json");
                Vec::new()
            }),
        Err(_) => Vec::new(),
    }
}

/// Adds a rule to `dir/faults.json`; daemons on this fake pick it up on
/// their next call. Rules that have expired are dropped.
pub fn add_fault(dir: &Path, rule: FaultRule) -> Result<()> {
    edit_faults(dir, |rules| rules.push(rule))
}

/// Removes the rules for `client`, or all rules.
pub fn clear_faults(dir: &Path, client: Option<&str>) -> Result<()> {
    edit_faults(dir, |rules| {
        rules.retain(|rule| client.is_some_and(|uid| rule.client.as_deref() != Some(uid)));
    })
}

fn edit_faults(dir: &Path, edit: impl FnOnce(&mut Vec<FaultRule>)) -> Result<()> {
    std::fs::create_dir_all(dir).map_err(|e| io_error("create dir", e))?;
    let _lock = lock_dir(dir)?;
    let path = dir.join("faults.json");
    let mut rules = read_fault_file(&path);
    let now = unix_ms();
    rules.retain(|rule| rule.until_ms.is_none_or(|until| until > now));
    edit(&mut rules);
    let json = serde_json::to_vec(&FaultFile { rules })
        .map_err(|e| BackendError::Other(format!("fake backend: encode faults: {e}")))?;
    write_replace(&path, &json).map_err(|e| io_error("save faults", e))
}

/// A simulated network (DESIGN.md §15.1). Every call costs `rtt`, listings
/// and node batches one per 150 entries, uploads three; block transfers
/// also take `len / bandwidth` on a pipe the process's calls share.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NetProfile {
    pub rtt: Duration,
    /// Upload bandwidth; `None` is unlimited.
    pub up_bytes_per_sec: Option<u64>,
    /// Download bandwidth; `None` is unlimited.
    pub down_bytes_per_sec: Option<u64>,
}

/// `profile.json`: `{"rtt_ms":80,"up_bytes_per_sec":20000000,"down_bytes_per_sec":20000000}`.
#[derive(Debug, Default, Serialize, Deserialize)]
struct ProfileFile {
    #[serde(default)]
    rtt_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    up_bytes_per_sec: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    down_bytes_per_sec: Option<u64>,
}

fn read_profile_file(path: &Path) -> NetProfile {
    let Ok(json) = std::fs::read(path) else {
        return NetProfile::default();
    };
    match serde_json::from_slice::<ProfileFile>(&json) {
        Ok(file) => NetProfile {
            rtt: Duration::from_millis(file.rtt_ms),
            up_bytes_per_sec: file.up_bytes_per_sec.filter(|rate| *rate > 0),
            down_bytes_per_sec: file.down_bytes_per_sec.filter(|rate| *rate > 0),
        },
        Err(error) => {
            tracing::warn!(%error, "fake backend: ignoring a malformed profile.json");
            NetProfile::default()
        }
    }
}

/// The profile in `dir/profile.json`; none (no cost) without the file.
pub fn read_profile(dir: &Path) -> NetProfile {
    read_profile_file(&dir.join("profile.json"))
}

/// Writes `dir/profile.json`; daemons on this fake use it from their next
/// call.
pub fn write_profile(dir: &Path, profile: NetProfile) -> Result<()> {
    std::fs::create_dir_all(dir).map_err(|e| io_error("create dir", e))?;
    let file = ProfileFile {
        rtt_ms: profile.rtt.as_millis() as u64,
        up_bytes_per_sec: profile.up_bytes_per_sec,
        down_bytes_per_sec: profile.down_bytes_per_sec,
    };
    let json = serde_json::to_vec(&file)
        .map_err(|e| BackendError::Other(format!("fake backend: encode profile: {e}")))?;
    write_replace(&dir.join("profile.json"), &json).map_err(|e| io_error("save profile", e))
}

/// Removes `dir/profile.json`: calls cost nothing again.
pub fn clear_profile(dir: &Path) -> Result<()> {
    match std::fs::remove_file(dir.join("profile.json")) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            Err(io_error("remove profile", error))
        }
        _ => Ok(()),
    }
}

/// The pipes of a [`NetProfile`], one per direction, shared by every handle
/// of a process: each holds the time it is busy until. Two daemons on one
/// fake each have their own.
struct Links {
    up: Mutex<tokio::time::Instant>,
    down: Mutex<tokio::time::Instant>,
    /// Block downloads and file uploads in flight over the process's
    /// handles, for tests of the session's limits (D033, D110).
    fetches: InFlight,
    /// The revisions' block lists, read by range as the adapter reads them
    /// (D135): each page listed costs a round trip.
    block_lists: BlockLists<(), ()>,
    uploads: InFlight,
}

impl Links {
    fn new() -> Self {
        let now = tokio::time::Instant::now();
        Self {
            up: Mutex::new(now),
            down: Mutex::new(now),
            fetches: InFlight::default(),
            block_lists: BlockLists::default(),
            uploads: InFlight::default(),
        }
    }
}

/// Calls of one kind in flight, and the most seen at once.
#[derive(Default)]
struct InFlight {
    now: AtomicUsize,
    peak: AtomicUsize,
}

impl InFlight {
    /// Counts a call until the guard drops.
    fn enter(&self) -> InFlightGuard<'_> {
        let now = self.now.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(now, Ordering::SeqCst);
        InFlightGuard(self)
    }

    /// The most calls in flight at once since the last take; starts over.
    fn take_peak(&self) -> usize {
        self.peak
            .swap(self.now.load(Ordering::SeqCst), Ordering::SeqCst)
    }
}

struct InFlightGuard<'a>(&'a InFlight);

impl Drop for InFlightGuard<'_> {
    fn drop(&mut self) {
        self.0.now.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Sleeps for `len` bytes at `rate` on `link`, after the transfers that
/// reserved it before.
async fn transfer(link: &Mutex<tokio::time::Instant>, len: u64, rate: Option<u64>) {
    let Some(rate) = rate.filter(|rate| *rate > 0) else {
        return;
    };
    let took = Duration::from_secs_f64(len as f64 / rate as f64);
    let end = {
        let mut busy = link.lock();
        *busy = (*busy).max(tokio::time::Instant::now()) + took;
        *busy
    };
    tokio::time::sleep_until(end).await;
}

/// Sleeps for `count` round trips.
async fn round_trips(profile: &NetProfile, count: u32) {
    if !profile.rtt.is_zero() && count > 0 {
        tokio::time::sleep(profile.rtt * count).await;
    }
}

/// Round trips for `entries` listed or fetched in pages.
fn pages(entries: usize) -> u32 {
    entries.div_ceil(NET_PAGE).max(1) as u32
}

/// The block pages of one revision of the fake (D135): a round trip per
/// page, the blocks' hashes made up, nothing to verify.
struct FakePages<'a> {
    fake: &'a FakeBackend,
    file: &'a LinkId,
    revision: &'a RevisionId,
}

#[async_trait::async_trait]
impl PageSource for FakePages<'_> {
    type Target = ();
    type Meta = ();

    async fn page(&self, from: u32, size: u32) -> Result<Page<(), ()>> {
        self.fake.delay().await;
        self.fake.enter(FakeOp::Blocks, 1).await?;
        let count = self.fake.with_state(|s| {
            if !self.fake.may_read(s, self.file) {
                return Err(BackendError::NotFound);
            }
            let node = s.nodes.get(self.file).ok_or(BackendError::NotFound)?;
            let rev = node.revision(self.revision)?;
            Ok(rev.size.div_ceil(s.block_size as u64) as u32)
        })?;
        let end = from.saturating_add(size).min(count + 1);
        Ok(Page {
            blocks: (from..end.max(from))
                .map(|index| Listed {
                    index,
                    hash: index.to_le_bytes().to_vec(),
                    target: (),
                })
                .collect(),
            meta: (),
        })
    }

    async fn verify(&self, _: &(), _: &[&[u8]]) -> Result<()> {
        Ok(())
    }
}

/// A remote change applied by the fake, in order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FakeChange {
    CreatedFile { link: LinkId, name: String },
    CreatedFolder { link: LinkId, name: String },
    NewRevision { link: LinkId, revision: RevisionId },
    Renamed { link: LinkId, name: String },
    Trashed { link: LinkId },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Revision {
    id: RevisionId,
    size: u64,
    sha1: [u8; 20],
    mtime: SystemTime,
    mode: Option<u32>,
    /// The uploading client's UID, when it had one.
    #[serde(default)]
    client_uid: Option<String>,
    /// Whether the uploader claimed the SHA-1 (the official apps may not).
    #[serde(default = "yes")]
    sha1_claimed: bool,
    /// Creation order (the id counter at creation).
    #[serde(default)]
    seq: u64,
    /// Held in memory by in-memory fakes; persisted fakes read
    /// `blobs/<id>` instead.
    #[serde(skip)]
    content: Option<Bytes>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Node {
    link: LinkId,
    parent: Option<LinkId>,
    name: String,
    kind: NodeKind,
    trashed: bool,
    folder_mtime: Option<SystemTime>,
    revisions: Vec<Revision>,
    /// Generation of the trashing, for retention.
    #[serde(default)]
    trashed_at: u64,
    /// The volume of a root folder other than My Files'
    /// ([`FakeBackend::add_root`]); `None` everywhere else.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    volume: Option<String>,
    /// A photo's timeline fields ([`FakeBackend::add_photo`]); `None`
    /// everywhere else.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    photo: Option<FakePhoto>,
    /// Signatures that "do not verify" ([`FakeBackend::set_unverified`]):
    /// the part (`name`, `key`, `content key`) and its claimed signer.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    unverified: BTreeMap<String, Option<String>>,
}

/// What the timeline knows of a photo (M5.13).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct FakePhoto {
    capture_time: i64,
    #[serde(default)]
    tags: Vec<u8>,
    /// The main photo of a related one (a Live Photo's video).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    main: Option<LinkId>,
    /// Its 512 px thumbnail (M5.13b); `None`: it has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    thumbnail: Option<FakeThumbnail>,
}

/// A photo's thumbnail on the fake (M5.13b, D115): a lossless WebP of one
/// colour, made from these fields when it is asked for, so the drive's
/// files stay small. Its digest is that of the bytes as the uploader made
/// them; the faults change what the server sends or what was signed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FakeThumbnail {
    pub width: u32,
    pub height: u32,
    /// The server sends other bytes than those the manifest signs.
    #[serde(default)]
    pub tampered: bool,
    /// What was signed is no WebP.
    #[serde(default)]
    pub not_webp: bool,
}

impl FakeThumbnail {
    /// What `pdmount fake photo add|seed` give a photo: 512 x 384, the long
    /// side of Proton's thumbnail.
    pub const DEFAULT: Self = Self {
        width: 512,
        height: 384,
        tampered: false,
        not_webp: false,
    };

    /// The bytes as the uploader made and signed them; the colour comes
    /// from `seed` (the photo's link), so photos tell apart.
    pub fn signed_bytes(&self, seed: &str) -> Vec<u8> {
        if self.not_webp {
            return format!("not a WebP: {seed}").into_bytes();
        }
        let colour = sha2::Sha256::digest(seed.as_bytes());
        let pixels = (self.width as usize) * (self.height as usize);
        let rgb: Vec<u8> = std::iter::repeat_n([colour[0], colour[1], colour[2]], pixels)
            .flatten()
            .collect();
        let mut out = Vec::new();
        image_webp::WebPEncoder::new(&mut out)
            .encode(&rgb, self.width, self.height, image_webp::ColorType::Rgb8)
            .expect("a WebP of one colour encodes");
        out
    }

    /// What the server sends.
    fn served_bytes(&self, seed: &str) -> Vec<u8> {
        let mut bytes = self.signed_bytes(seed);
        if self.tampered {
            match bytes.last_mut() {
                Some(last) => *last ^= 0xff,
                None => bytes.push(0),
            }
        }
        bytes
    }
}

impl Node {
    fn active(&self) -> Option<&Revision> {
        self.revisions.last()
    }

    fn revision(&self, id: &RevisionId) -> Result<&Revision> {
        self.revisions
            .iter()
            .find(|r| &r.id == id)
            .ok_or(BackendError::NotFound)
    }

    fn to_remote(&self) -> RemoteNode {
        let active = self.active();
        RemoteNode {
            link_id: self.link.clone(),
            parent_link_id: self.parent.clone(),
            name: self.name.clone(),
            kind: self.kind,
            active_revision: active.map(|r| r.id.clone()),
            size: active.map(|r| r.size),
            mtime: active.map(|r| r.mtime).or(self.folder_mtime),
            mode: active.and_then(|r| r.mode),
            sha1: active.filter(|r| r.sha1_claimed).map(|r| r.sha1),
            name_hash: Some(name_hash(&self.name)),
            trashed: self.trashed,
            unverified: self.unverified(),
        }
    }

    /// The failed signatures as the adapter reports them (D132).
    fn unverified(&self) -> Option<Unverified> {
        let failed: Vec<(SignaturePart, Option<&str>)> = self
            .unverified
            .iter()
            .filter_map(|(part, signer)| Some((SignaturePart::parse(part)?, signer.as_deref())))
            .collect();
        Unverified::of(&self.link, &self.name, &failed)
    }
}

fn yes() -> bool {
    true
}

fn name_hash(name: &str) -> NameHash {
    NameHash::new(format!("fake:{name}"))
}

fn default_page_size() -> usize {
    50
}

fn default_keep_revisions() -> usize {
    DEFAULT_KEEP_REVISIONS
}

fn io_error(context: &str, error: std::io::Error) -> BackendError {
    BackendError::local_io(format_args!("fake backend: {context}"), error)
}

/// Settings and counters, journaled as one record.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Meta {
    next_id: u64,
    quota_max: u64,
    block_size: usize,
    events_base: u64,
    changes_base: u64,
    keep_revisions: usize,
    /// The core event stream's position ([`FakeBackend::core_signal`]).
    #[serde(default)]
    core_events: u64,
}

/// One journaled delta.
#[derive(Debug, Clone, Serialize, Deserialize)]
enum Record {
    Node(Node),
    Removed(LinkId),
    Event(FakeEvent),
    Change(FakeChange),
    Meta(Meta),
    Volume(String, FakeVolume),
    Share(String, FakeShare),
}

/// One journal line: everything one operation changed.
#[derive(Debug, Serialize, Deserialize)]
struct Line {
    generation: u64,
    records: Vec<Record>,
}

/// What the current operation changed, for the journal.
#[derive(Debug, Default)]
struct Delta {
    nodes: BTreeSet<LinkId>,
    events: usize,
    changes: usize,
    meta: bool,
    trashed: bool,
    volumes: BTreeSet<String>,
    shares: BTreeSet<String>,
}

impl Delta {
    fn is_empty(&self) -> bool {
        self.nodes.is_empty()
            && self.events == 0
            && self.changes == 0
            && !self.meta
            && self.volumes.is_empty()
            && self.shares.is_empty()
    }
}

#[derive(Serialize, Deserialize)]
struct State {
    /// Persistence directory; `None` for an in-memory fake.
    #[serde(skip)]
    dir: Option<PathBuf>,
    /// Bumped by every journaled change; the snapshot records the last one
    /// it contains.
    #[serde(default)]
    generation: u64,
    /// Compaction count this process has loaded.
    #[serde(skip)]
    epoch: u64,
    /// Bytes of the journal this process has applied.
    #[serde(skip)]
    journal_offset: u64,
    username: String,
    root: LinkId,
    nodes: BTreeMap<LinkId, Node>,
    events: Vec<FakeEvent>,
    changes: Vec<FakeChange>,
    /// Volumes other than [`MAIN_VOLUME`], by id.
    #[serde(default)]
    volumes: BTreeMap<String, FakeVolume>,
    /// Shares by id.
    #[serde(default)]
    shares: BTreeMap<String, FakeShare>,
    /// Absolute index of `events[0]` (event ids are absolute).
    #[serde(default)]
    events_base: u64,
    /// Absolute index of `changes[0]`.
    #[serde(default)]
    changes_base: u64,
    next_id: u64,
    quota_max: u64,
    block_size: usize,
    #[serde(default = "default_keep_revisions")]
    keep_revisions: usize,
    /// `DriveShareRefresh` events sent on the core stream so far: each
    /// [`FakeBackend::core_signal`] adds one (M5.12).
    #[serde(default)]
    core_events: u64,
    #[serde(skip, default = "default_page_size")]
    event_page_size: usize,
    /// Volumes whose next poll answers `refresh`.
    #[serde(skip)]
    refresh_pending: BTreeSet<String>,
    #[serde(skip)]
    faults: VecDeque<(FakeOp, BackendError)>,
    /// Renames that take effect and then fail ([`FakeBackend::lose_rename_reply`]).
    #[serde(skip)]
    lost_rename_replies: VecDeque<BackendError>,
    /// Nodes whose metadata "cannot be decrypted" ([`FakeBackend::set_degraded`]).
    #[serde(skip)]
    degraded: HashSet<LinkId>,
    #[serde(skip)]
    latency: Duration,
    #[serde(skip)]
    pauses: Vec<PendingPause>,
    /// The server refuses a commit whose base revision is no longer active.
    #[serde(skip, default = "yes")]
    commit_checks_revision: bool,
    /// The server refuses a rename whose name hash is stale.
    #[serde(skip, default = "yes")]
    name_hash_enforced: bool,
    /// `faults.json` as last read: (modified, length) and its rules.
    #[serde(skip)]
    fault_cache: Option<((SystemTime, u64), Vec<FaultRule>)>,
    /// The network profile of this process's calls.
    #[serde(skip)]
    profile: NetProfile,
    /// `profile.json` as last read: (modified, length).
    #[serde(skip)]
    profile_stamp: Option<(SystemTime, u64)>,
    /// Children that are not trashed, by parent and name; kept in step by
    /// [`State::put_node`] and [`State::remove_node`], rebuilt on load.
    #[serde(skip)]
    children: HashMap<LinkId, BTreeMap<String, LinkId>>,
    /// Bytes of every kept revision (the quota's use), kept in step like
    /// `children`.
    #[serde(skip)]
    used: u64,
    #[serde(skip)]
    delta: Delta,
}

impl State {
    fn empty() -> Self {
        let root = LinkId::new("root");
        let mut nodes = BTreeMap::new();
        nodes.insert(
            root.clone(),
            Node {
                link: root.clone(),
                parent: None,
                name: String::new(),
                kind: NodeKind::Folder,
                trashed: false,
                folder_mtime: None,
                revisions: Vec::new(),
                trashed_at: 0,
                volume: None,
                photo: None,
                unverified: BTreeMap::new(),
            },
        );
        Self {
            dir: None,
            generation: 0,
            epoch: 0,
            journal_offset: 0,
            username: "fake@example.test".into(),
            root,
            nodes,
            events: Vec::new(),
            changes: Vec::new(),
            volumes: BTreeMap::new(),
            shares: BTreeMap::new(),
            events_base: 0,
            changes_base: 0,
            next_id: 0,
            quota_max: 5 << 30,
            block_size: FakeBackend::DEFAULT_BLOCK_SIZE,
            keep_revisions: DEFAULT_KEEP_REVISIONS,
            core_events: 0,
            event_page_size: default_page_size(),
            refresh_pending: BTreeSet::new(),
            faults: VecDeque::new(),
            lost_rename_replies: VecDeque::new(),
            degraded: HashSet::new(),
            latency: Duration::ZERO,
            pauses: Vec::new(),
            commit_checks_revision: true,
            name_hash_enforced: true,
            fault_cache: None,
            profile: NetProfile::default(),
            profile_stamp: None,
            children: HashMap::new(),
            used: 0,
            delta: Delta::default(),
        }
    }

    fn new_id(&mut self, prefix: &str) -> String {
        self.next_id += 1;
        self.delta.meta = true;
        format!("{prefix}{}", self.next_id)
    }

    fn take_fault(&mut self, op: FakeOp) -> Result<()> {
        let position = self.faults.iter().position(|(o, _)| *o == op);
        match position.and_then(|i| self.faults.remove(i)) {
            Some((_, error)) => Err(error),
            None => Ok(()),
        }
    }

    /// Fails like the rules of `faults.json` say, for the calling client
    /// and a call of `op`.
    fn check_fault_file(&mut self, client: Option<&str>, op: FakeOp) -> Result<()> {
        let Some(dir) = &self.dir else {
            return Ok(());
        };
        let path = dir.join("faults.json");
        let stamp = std::fs::metadata(&path)
            .and_then(|m| Ok((m.modified()?, m.len())))
            .ok();
        match (stamp, &self.fault_cache) {
            (None, _) => {
                self.fault_cache = None;
                return Ok(());
            }
            (Some(stamp), Some((cached, _))) if *cached == stamp => {}
            (Some(stamp), _) => self.fault_cache = Some((stamp, read_fault_file(&path))),
        }
        let now = unix_ms();
        let rules = self.fault_cache.iter().flat_map(|(_, rules)| rules);
        for rule in rules {
            let applies = rule.until_ms.is_none_or(|until| until > now)
                && rule.client.as_deref().is_none_or(|uid| Some(uid) == client)
                && (rule.kind != FaultKind::BrokenNodes || op == FakeOp::GetNodes);
            if applies {
                return Err(match rule.kind {
                    FaultKind::Offline => BackendError::Offline("injected".into()),
                    FaultKind::Unauthorized => BackendError::Unauthorized,
                    FaultKind::BrokenNodes => {
                        BackendError::Other("injected: a node cannot be decrypted".into())
                    }
                });
            }
        }
        Ok(())
    }

    /// Re-reads `profile.json` when it changed; a missing file is no cost.
    fn check_profile_file(&mut self) {
        let Some(dir) = &self.dir else {
            return;
        };
        let path = dir.join("profile.json");
        let stamp = std::fs::metadata(&path)
            .and_then(|m| Ok((m.modified()?, m.len())))
            .ok();
        if stamp != self.profile_stamp {
            self.profile = if stamp.is_some() {
                read_profile_file(&path)
            } else {
                NetProfile::default()
            };
            self.profile_stamp = stamp;
        }
    }

    fn node(&self, link: &LinkId) -> Result<&Node> {
        self.nodes
            .get(link)
            .filter(|n| !n.trashed)
            .ok_or(BackendError::NotFound)
    }

    fn folder(&self, link: &LinkId) -> Result<&Node> {
        let node = self.node(link)?;
        match node.kind {
            NodeKind::Folder => Ok(node),
            NodeKind::File => Err(BackendError::Other(format!("{link} is not a folder"))),
        }
    }

    fn child_named(&self, parent: &LinkId, name: &str) -> Option<&Node> {
        let link = self.children.get(parent)?.get(name)?;
        self.nodes.get(link)
    }

    /// Inserts or replaces a node, keeping the child index and the used
    /// total in step. Every change of a node goes through here.
    fn put_node(&mut self, node: Node) {
        self.remove_node(&node.link);
        self.used += node.revisions.iter().map(|r| r.size).sum::<u64>();
        if !node.trashed
            && let Some(parent) = &node.parent
        {
            self.children
                .entry(parent.clone())
                .or_default()
                .insert(node.name.clone(), node.link.clone());
        }
        self.nodes.insert(node.link.clone(), node);
    }

    fn remove_node(&mut self, link: &LinkId) -> Option<Node> {
        let node = self.nodes.remove(link)?;
        self.used -= node.revisions.iter().map(|r| r.size).sum::<u64>();
        if let Some(parent) = &node.parent
            && let Some(children) = self.children.get_mut(parent)
            && children.get(&node.name) == Some(link)
        {
            children.remove(&node.name);
            if children.is_empty() {
                self.children.remove(parent);
            }
        }
        Some(node)
    }

    /// Changes a node through [`State::put_node`].
    fn update_node(&mut self, link: &LinkId, change: impl FnOnce(&mut Node)) {
        if let Some(mut node) = self.remove_node(link) {
            change(&mut node);
            self.put_node(node);
        }
    }

    /// Rebuilds the child index and the used total from the nodes.
    fn reindex(&mut self) {
        let nodes = std::mem::take(&mut self.nodes);
        self.children.clear();
        self.used = 0;
        for node in nodes.into_values() {
            self.put_node(node);
        }
    }

    fn ensure_name_free(&self, parent: &LinkId, name: &str, except: Option<&LinkId>) -> Result<()> {
        match self.child_named(parent, name) {
            Some(existing) if Some(&existing.link) != except => Err(BackendError::AlreadyExists {
                existing: Some(existing.link.clone()),
            }),
            _ => Ok(()),
        }
    }

    fn ensure_quota(&self, extra: u64) -> Result<()> {
        if self.used + extra > self.quota_max {
            Err(BackendError::QuotaExceeded)
        } else {
            Ok(())
        }
    }

    /// Marks a node as changed by the current operation.
    fn touch(&mut self, link: &LinkId) {
        self.delta.nodes.insert(link.clone());
    }

    fn push_event(&mut self, kind: EventKind, link: &LinkId) {
        let volume = self.volume_of(link);
        let node = &self.nodes[link];
        self.events.push(FakeEvent {
            event: RemoteEvent {
                kind,
                link_id: link.clone(),
                parent_link_id: node.parent.clone(),
                trashed: node.trashed,
            },
            volume: (volume != MAIN_VOLUME).then_some(volume),
        });
        self.delta.events += 1;
    }

    /// The top folder above `link` (itself for a root).
    fn top_of(&self, link: &LinkId) -> LinkId {
        let mut current = link.clone();
        // Bounded: a cycle cannot form (moves into a descendant fail).
        for _ in 0..4096 {
            match self
                .nodes
                .get(&current)
                .and_then(|node| node.parent.clone())
            {
                Some(parent) => current = parent,
                None => break,
            }
        }
        current
    }

    /// The volume `link` is on: that of its top folder. My Files' root, and
    /// a root no volume claims, are on [`MAIN_VOLUME`].
    fn volume_of(&self, link: &LinkId) -> String {
        let top = self.top_of(link);
        self.nodes
            .get(&top)
            .and_then(|node| node.volume.clone())
            .unwrap_or_else(|| MAIN_VOLUME.to_owned())
    }

    /// Shares `link` on `volume` with the drive's user as `role`; returns
    /// the share's id.
    fn add_share(&mut self, volume: &str, link: &LinkId, role: FakeRole) -> String {
        let id = self.new_id("share-");
        let members = BTreeMap::from([(self.username.clone(), role)]);
        let created = self.next_id;
        self.shares.insert(
            id.clone(),
            FakeShare {
                volume: volume.to_owned(),
                root: link.clone(),
                members,
                created,
            },
        );
        self.delta.shares.insert(id.clone());
        id
    }

    fn owner_of(&self, volume: &str) -> &str {
        match self.volumes.get(volume) {
            Some(volume) => &volume.owner,
            None => &self.username,
        }
    }

    /// The role `user` has on `link`: `None` for the owner of its volume
    /// (who may do anything), the best role of the shares above it
    /// otherwise; [`BackendError::NotFound`] when no share reaches it.
    fn role_of(&self, user: &str, link: &LinkId) -> Result<Option<FakeRole>> {
        let volume = self.volume_of(link);
        if self.owner_of(&volume) == user {
            return Ok(None);
        }
        self.shares
            .values()
            .filter(|share| share.volume == volume && self.is_descendant(link, &share.root))
            .filter_map(|share| share.members.get(user).copied())
            .max_by_key(|role| *role as u8)
            .map(Some)
            .ok_or(BackendError::NotFound)
    }

    /// Whether `user` may read `link`.
    fn may_read(&self, user: &str, link: &LinkId) -> bool {
        self.role_of(user, link).is_ok()
    }

    /// `PermissionDenied` unless `user` may change `link`.
    fn check_write(&self, user: &str, link: &LinkId) -> Result<()> {
        match self.role_of(user, link)? {
            None => Ok(()),
            Some(role) if role.may_write() => Ok(()),
            Some(_) => Err(BackendError::PermissionDenied),
        }
    }

    fn push_change(&mut self, change: FakeChange) {
        self.changes.push(change);
        self.delta.changes += 1;
    }

    fn latest_event(&self) -> u64 {
        self.events_base + self.events.len() as u64
    }

    /// The Photos root, if the drive has one.
    fn photos_root_if_any(&self) -> Option<LinkId> {
        self.nodes
            .values()
            .find(|node| node.parent.is_none() && node.volume.as_deref() == Some(PHOTOS_VOLUME))
            .map(|node| node.link.clone())
    }

    /// The thumbnail of revision `revision` of photo `link`, when that
    /// revision is the photo's active one.
    fn photo_thumbnail(
        &self,
        link: &LinkId,
        revision: &RevisionId,
    ) -> Result<Option<FakeThumbnail>> {
        let node = self.node(link)?;
        let photo = node.photo.as_ref().ok_or(BackendError::NotFound)?;
        if node.active().map(|rev| &rev.id) != Some(revision) {
            return Err(BackendError::NotFound);
        }
        Ok(photo.thumbnail)
    }

    /// The Photos root, made at the first use (named "root", as Proton's).
    fn photos_root(&mut self) -> LinkId {
        if let Some(root) = self.photos_root_if_any() {
            return root;
        }
        let link = LinkId::new(self.new_id("photos-root-"));
        self.put_node(Node {
            link: link.clone(),
            parent: None,
            name: "root".into(),
            kind: NodeKind::Folder,
            trashed: false,
            folder_mtime: Some(SystemTime::now()),
            revisions: Vec::new(),
            trashed_at: 0,
            volume: Some(PHOTOS_VOLUME.to_owned()),
            photo: None,
            unverified: BTreeMap::new(),
        });
        self.touch(&link);
        link
    }

    /// The timeline: the photos that are no related photo and not trashed,
    /// newest first (ties by link, so pages are stable), each with its
    /// related photos.
    fn timeline(&self) -> Vec<TimelineItem> {
        let Some(root) = self.photos_root_if_any() else {
            return Vec::new();
        };
        let in_root = |node: &&Node| {
            node.parent.as_ref() == Some(&root) && !node.trashed && node.photo.is_some()
        };
        let mut related: HashMap<&LinkId, Vec<RelatedPhoto>> = HashMap::new();
        for node in self.nodes.values().filter(in_root) {
            let photo = node.photo.as_ref().expect("filtered");
            if let Some(main) = &photo.main {
                related.entry(main).or_default().push(RelatedPhoto {
                    link: node.link.clone(),
                    capture_time: photo.capture_time,
                });
            }
        }
        let mut items: Vec<TimelineItem> = self
            .nodes
            .values()
            .filter(in_root)
            .filter(|node| {
                node.photo
                    .as_ref()
                    .is_some_and(|photo| photo.main.is_none())
            })
            .map(|node| {
                let photo = node.photo.as_ref().expect("filtered");
                TimelineItem {
                    link: node.link.clone(),
                    capture_time: photo.capture_time,
                    tags: photo.tags.clone(),
                    related: related.remove(&node.link).unwrap_or_default(),
                }
            })
            .collect();
        items.sort_by(|a, b| {
            b.capture_time
                .cmp(&a.capture_time)
                .then_with(|| b.link.cmp(&a.link))
        });
        items
    }

    fn is_descendant(&self, node: &LinkId, ancestor: &LinkId) -> bool {
        let mut current = Some(node.clone());
        while let Some(link) = current {
            if &link == ancestor {
                return true;
            }
            current = self.nodes.get(&link).and_then(|n| n.parent.clone());
        }
        false
    }

    fn blob(&self, revision: &RevisionId) -> Option<PathBuf> {
        self.dir
            .as_ref()
            .map(|dir| dir.join("blobs").join(&revision.0))
    }

    /// Where `len` bytes of a revision from `start` are.
    fn locate(&self, revision: &Revision, start: u64, len: u64) -> Result<BlockRead> {
        if let Some(content) = &revision.content {
            let end = (start + len).min(content.len() as u64);
            return Ok(BlockRead::Memory(
                content.slice(start as usize..end as usize),
            ));
        }
        let path = self
            .blob(&revision.id)
            .ok_or_else(|| BackendError::Other(format!("no content for {}", revision.id)))?;
        Ok(BlockRead::Blob { path, start, len })
    }

    /// Block `index` (from 1) of a file's revision.
    fn block(&self, file: &LinkId, revision: &RevisionId, index: u32) -> Result<BlockRead> {
        let node = self.nodes.get(file).ok_or(BackendError::NotFound)?;
        let rev = node.revision(revision)?;
        let block = self.block_size as u64;
        let start = u64::from(index)
            .checked_sub(1)
            .map(|i| i * block)
            .filter(|&start| start < rev.size)
            .ok_or(BackendError::NotFound)?;
        self.locate(rev, start, block.min(rev.size - start))
    }

    /// `len` bytes of a revision from `start`, read under the lock (test
    /// helpers only; calls read after releasing it).
    fn read_range(&self, revision: &Revision, start: u64, len: u64) -> Result<Bytes> {
        match self.locate(revision, start, len)? {
            BlockRead::Memory(bytes) => Ok(bytes),
            BlockRead::Blob { path, start, len } => read_blob(&path, start, len),
        }
    }

    fn content(&self, revision: &Revision) -> Result<Bytes> {
        self.read_range(revision, 0, revision.size)
    }

    fn new_revision(
        &mut self,
        mut staged: Staged,
        meta: &FileMeta,
        client: &FakeClient,
    ) -> Result<(RevisionId, Revision)> {
        if meta.sha1.is_some_and(|sha1| sha1 != staged.sha1) {
            return Err(BackendError::Integrity("content SHA-1 mismatch".into()));
        }
        let id = RevisionId::new(self.new_id("rev-"));
        let content = match staged.content.take() {
            Some(StagedContent::Memory(bytes)) => Some(bytes),
            Some(StagedContent::Blob(tmp)) => {
                let path = self
                    .blob(&id)
                    .expect("blobs are staged only when persisted");
                std::fs::rename(&tmp, &path).map_err(|e| io_error("store blob", e))?;
                None
            }
            None => None,
        };
        let revision = Revision {
            id: id.clone(),
            size: staged.size,
            sha1: staged.sha1,
            mtime: meta.mtime,
            mode: meta.mode.filter(|_| client.claims_mode),
            client_uid: client.uid.clone(),
            sha1_claimed: client.claims_sha1,
            seq: self.next_id,
            content,
        };
        Ok((id, revision))
    }

    fn meta(&self) -> Meta {
        Meta {
            next_id: self.next_id,
            quota_max: self.quota_max,
            block_size: self.block_size,
            events_base: self.events_base,
            changes_base: self.changes_base,
            keep_revisions: self.keep_revisions,
            core_events: self.core_events,
        }
    }

    /// Bounds growth after an operation. Returns the revisions whose blobs
    /// are no longer referenced.
    fn retain(&mut self) -> Vec<RevisionId> {
        let mut dropped = Vec::new();
        let keep = self.keep_revisions.max(1);
        for link in self.delta.nodes.clone() {
            if self
                .nodes
                .get(&link)
                .is_some_and(|node| node.revisions.len() > keep)
            {
                self.update_node(&link, |node| {
                    let excess = node.revisions.len() - keep;
                    dropped.extend(node.revisions.drain(..excess).map(|r| r.id));
                });
            }
        }
        if self.delta.trashed {
            let mut trashed: Vec<(u64, LinkId)> = self
                .nodes
                .values()
                .filter(|n| n.trashed)
                .map(|n| (n.trashed_at, n.link.clone()))
                .collect();
            if trashed.len() > KEEP_TRASHED * 2 {
                trashed.sort();
                for (_, link) in &trashed[..trashed.len() - KEEP_TRASHED] {
                    if let Some(node) = self.remove_node(link) {
                        dropped.extend(node.revisions.into_iter().map(|r| r.id));
                    }
                    self.delta.nodes.insert(link.clone());
                }
            }
        }
        if self.events.len() > KEEP_EVENTS * 3 / 2 {
            let cut = self.events.len() - KEEP_EVENTS;
            self.events.drain(..cut);
            self.events_base += cut as u64;
            self.delta.events = self.delta.events.min(self.events.len());
            self.delta.meta = true;
        }
        if self.changes.len() > KEEP_CHANGES * 3 / 2 {
            let cut = self.changes.len() - KEEP_CHANGES;
            self.changes.drain(..cut);
            self.changes_base += cut as u64;
            self.delta.changes = self.delta.changes.min(self.changes.len());
            self.delta.meta = true;
        }
        dropped
    }

    /// Ends an operation: retention, then the journal line (persisted
    /// fakes), then the blobs nothing refers to any more.
    fn finish(&mut self) -> Result<()> {
        if self.delta.is_empty() {
            return Ok(());
        }
        let dropped = self.retain();
        let delta = std::mem::take(&mut self.delta);
        let Some(dir) = self.dir.clone() else {
            return Ok(());
        };
        let mut records: Vec<Record> = delta
            .nodes
            .iter()
            .map(|link| match self.nodes.get(link) {
                Some(node) => Record::Node(node.clone()),
                None => Record::Removed(link.clone()),
            })
            .collect();
        records.extend(
            self.events[self.events.len() - delta.events..]
                .iter()
                .cloned()
                .map(Record::Event),
        );
        records.extend(delta.volumes.iter().filter_map(|id| {
            self.volumes
                .get(id)
                .map(|volume| Record::Volume(id.clone(), volume.clone()))
        }));
        records.extend(delta.shares.iter().filter_map(|id| {
            self.shares
                .get(id)
                .map(|share| Record::Share(id.clone(), share.clone()))
        }));
        records.extend(
            self.changes[self.changes.len() - delta.changes..]
                .iter()
                .cloned()
                .map(Record::Change),
        );
        records.push(Record::Meta(self.meta()));
        self.generation += 1;
        let mut line = serde_json::to_vec(&Line {
            generation: self.generation,
            records,
        })
        .map_err(|e| BackendError::Other(format!("fake backend: encode journal: {e}")))?;
        line.push(b'\n');
        let mut journal = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("journal.jsonl"))
            .map_err(|e| io_error("open journal", e))?;
        let length = journal
            .metadata()
            .map_err(|e| io_error("stat journal", e))?
            .len();
        if length > self.journal_offset {
            // A line torn by a killed writer: end it, so ours stays whole.
            line.insert(0, b'\n');
        }
        journal
            .write_all(&line)
            .map_err(|e| io_error("append journal", e))?;
        self.journal_offset = length + line.len() as u64;
        for revision in dropped {
            if let Some(path) = self.blob(&revision) {
                let _ = std::fs::remove_file(path);
            }
        }
        if self.journal_offset > COMPACT_BYTES {
            self.compact()?;
        }
        Ok(())
    }

    /// Writes the whole state as a new snapshot and starts an empty journal.
    /// A crash in between is harmless: journal lines at or below the
    /// snapshot's generation are skipped on load.
    fn compact(&mut self) -> Result<()> {
        let Some(dir) = self.dir.clone() else {
            return Ok(());
        };
        let json = serde_json::to_vec(self)
            .map_err(|e| BackendError::Other(format!("fake backend: encode state: {e}")))?;
        write_replace(&dir.join("snapshot.json"), &json)
            .map_err(|e| io_error("save snapshot", e))?;
        write_replace(&dir.join("journal.jsonl"), b"").map_err(|e| io_error("reset journal", e))?;
        self.epoch += 1;
        write_replace(&dir.join("epoch"), self.epoch.to_string().as_bytes())
            .map_err(|e| io_error("save epoch", e))?;
        self.journal_offset = 0;
        Ok(())
    }

    fn apply(&mut self, record: Record) {
        match record {
            Record::Node(node) => self.put_node(node),
            Record::Removed(link) => {
                self.remove_node(&link);
            }
            Record::Event(event) => self.events.push(event),
            Record::Change(change) => self.changes.push(change),
            Record::Volume(id, volume) => {
                self.volumes.insert(id, volume);
            }
            Record::Share(id, share) => {
                self.shares.insert(id, share);
            }
            Record::Meta(meta) => {
                self.next_id = meta.next_id;
                self.quota_max = meta.quota_max;
                self.block_size = meta.block_size;
                self.keep_revisions = meta.keep_revisions;
                self.core_events = self.core_events.max(meta.core_events);
                if meta.events_base > self.events_base {
                    let cut =
                        ((meta.events_base - self.events_base) as usize).min(self.events.len());
                    self.events.drain(..cut);
                    self.events_base = meta.events_base;
                }
                if meta.changes_base > self.changes_base {
                    let cut =
                        ((meta.changes_base - self.changes_base) as usize).min(self.changes.len());
                    self.changes.drain(..cut);
                    self.changes_base = meta.changes_base;
                }
            }
        }
    }

    /// Applies journal lines from byte `from` on; returns the offset after
    /// the last complete line. A line torn by a killed writer is skipped.
    fn replay(&mut self, journal: &[u8], from: u64) -> u64 {
        let mut offset = from as usize;
        while let Some(end) = journal[offset..].iter().position(|b| *b == b'\n') {
            let line = &journal[offset..offset + end];
            offset += end + 1;
            match serde_json::from_slice::<Line>(line) {
                Ok(line) if line.generation > self.generation => {
                    for record in line.records {
                        self.apply(record);
                    }
                    self.generation = line.generation;
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(%error, "fake backend: skipping a torn journal line");
                }
            }
        }
        offset as u64
    }

    /// Takes over another process's changes before an operation.
    fn refresh(&mut self) -> Result<()> {
        let Some(dir) = self.dir.clone() else {
            return Ok(());
        };
        let epoch = read_epoch(&dir)?;
        let journal_len = std::fs::metadata(dir.join("journal.jsonl"))
            .map(|m| m.len())
            .unwrap_or(0);
        if epoch != self.epoch || journal_len < self.journal_offset {
            let loaded = load_state(&dir)?;
            self.take_persisted(loaded);
            return Ok(());
        }
        if journal_len > self.journal_offset {
            // Only the lines after ours: rereading the whole journal makes
            // buffers of growing size, which glibc keeps (D049).
            let mut tail = Vec::new();
            std::fs::File::open(dir.join("journal.jsonl"))
                .and_then(|mut file| {
                    file.seek(SeekFrom::Start(self.journal_offset))?;
                    file.read_to_end(&mut tail)
                })
                .map_err(|e| io_error("read journal", e))?;
            self.journal_offset += self.replay(&tail, 0);
        }
        Ok(())
    }

    /// Replaces the persisted part of the state, keeping what is per
    /// process (faults, latency, page size).
    fn take_persisted(&mut self, loaded: State) {
        let State {
            generation,
            epoch,
            journal_offset,
            username,
            root,
            nodes,
            events,
            changes,
            volumes,
            shares,
            events_base,
            changes_base,
            next_id,
            quota_max,
            block_size,
            keep_revisions,
            children,
            used,
            ..
        } = loaded;
        self.volumes = volumes;
        self.shares = shares;
        self.generation = generation;
        self.epoch = epoch;
        self.journal_offset = journal_offset;
        self.username = username;
        self.root = root;
        self.nodes = nodes;
        self.events = events;
        self.changes = changes;
        self.events_base = events_base;
        self.changes_base = changes_base;
        self.next_id = next_id;
        self.quota_max = quota_max;
        self.block_size = block_size;
        self.keep_revisions = keep_revisions;
        self.children = children;
        self.used = used;
    }
}

fn read_epoch(dir: &Path) -> Result<u64> {
    match std::fs::read_to_string(dir.join("epoch")) {
        Ok(text) => text
            .trim()
            .parse()
            .map_err(|_| BackendError::Other("fake backend: corrupt epoch file".into())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(io_error("read epoch", error)),
    }
}

/// The snapshot plus the journal.
fn load_state(dir: &Path) -> Result<State> {
    let parse = |path: PathBuf| -> Result<State> {
        let json = std::fs::read(&path).map_err(|e| io_error("read state", e))?;
        serde_json::from_slice(&json).map_err(|e| {
            BackendError::Other(format!("fake backend: corrupt {}: {e}", path.display()))
        })
    };
    let mut state = if dir.join("snapshot.json").exists() {
        parse(dir.join("snapshot.json"))?
    } else {
        State::empty()
    };
    state.dir = Some(dir.to_owned());
    state.epoch = read_epoch(dir)?;
    state.reindex();
    match std::fs::read(dir.join("journal.jsonl")) {
        Ok(journal) => state.journal_offset = state.replay(&journal, 0),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(io_error("read journal", error)),
    }
    Ok(state)
}

/// A block's bytes as located under the lock, read after it is released.
enum BlockRead {
    Memory(Bytes),
    Blob { path: PathBuf, start: u64, len: u64 },
}

impl BlockRead {
    fn len(&self) -> u64 {
        match self {
            BlockRead::Memory(bytes) => bytes.len() as u64,
            BlockRead::Blob { len, .. } => *len,
        }
    }
}

fn read_blob(path: &Path, start: u64, len: u64) -> Result<Bytes> {
    let mut file = std::fs::File::open(path).map_err(|e| io_error("open blob", e))?;
    file.seek(SeekFrom::Start(start))
        .map_err(|e| io_error("seek blob", e))?;
    let mut out = Vec::with_capacity(len as usize);
    file.take(len)
        .read_to_end(&mut out)
        .map_err(|e| io_error("read blob", e))?;
    Ok(Bytes::from(out))
}

/// Replaces `path` atomically (a killed process leaves the old content).
fn write_replace(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    let mut file = std::fs::File::create(&tmp)?;
    file.write_all(data)?;
    drop(file);
    std::fs::rename(&tmp, path)
}

/// Exclusive lock on a persisted fake, held for one operation.
fn lock_dir(dir: &Path) -> Result<nix::fcntl::Flock<std::fs::File>> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("lock"))
        .map_err(|e| io_error("open lock", e))?;
    nix::fcntl::Flock::lock(file, nix::fcntl::FlockArg::LockExclusive)
        .map_err(|(_, errno)| io_error("lock", errno.into()))
}

enum StagedContent {
    Memory(Bytes),
    /// A temporary file in `blobs/`, renamed to the revision id on commit.
    Blob(PathBuf),
}

/// Upload content, read before the drive is locked.
struct Staged {
    size: u64,
    sha1: [u8; 20],
    content: Option<StagedContent>,
}

impl Drop for Staged {
    /// Removes a temporary blob that was never committed.
    fn drop(&mut self) {
        if let Some(StagedContent::Blob(tmp)) = &self.content {
            let _ = std::fs::remove_file(tmp);
        }
    }
}

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// In-memory Proton Drive. `Clone` shares the drive.
#[derive(Clone)]
pub struct FakeBackend {
    state: Arc<Mutex<State>>,
    client: Arc<FakeClient>,
    links: Arc<Links>,
}

impl Default for FakeBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeBackend {
    const DEFAULT_BLOCK_SIZE: usize = 4 << 20;

    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(State::empty())),
            client: Arc::default(),
            links: Arc::new(Links::new()),
        }
    }

    /// A handle on the same drive that acts as another client.
    pub fn with_client(&self, client: FakeClient) -> Self {
        Self {
            state: Arc::clone(&self.state),
            client: Arc::new(client),
            links: Arc::clone(&self.links),
        }
    }

    /// Opens the fake persisted in `dir`, creating an empty drive there if
    /// needed, with the network profile of `dir/profile.json`.
    /// `PDMOUNT_FAKE_KEEP_REVISIONS` sets how many revisions files keep.
    pub fn open(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir.join("blobs")).map_err(|e| io_error("create dir", e))?;
        let _lock = lock_dir(dir)?;
        let mut state = load_state(dir)?;
        if !dir.join("snapshot.json").exists() {
            // New: start the snapshot and journal.
            state.compact()?;
        }
        if let Some(keep) = std::env::var("PDMOUNT_FAKE_KEEP_REVISIONS")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|keep| *keep > 0 && *keep != state.keep_revisions)
        {
            state.keep_revisions = keep;
            state.delta.meta = true;
            state.finish()?;
        }
        state.check_profile_file();
        Ok(Self {
            state: Arc::new(Mutex::new(state)),
            client: Arc::default(),
            links: Arc::new(Links::new()),
        })
    }

    /// Makes the next call of `op` fail with `error` (one-shot, FIFO).
    pub fn inject_failure(&self, op: FakeOp, error: BackendError) {
        self.state.lock().faults.push_back((op, error));
    }

    /// Makes the next rename that succeeds fail afterwards with `error`, as
    /// a request whose reply was lost: the drive has the new name, the
    /// caller sees the error (one-shot, FIFO). In this process only.
    pub fn lose_rename_reply(&self, error: BackendError) {
        self.state.lock().lost_rename_replies.push_back(error);
    }

    /// Makes `link`'s metadata unreadable, as a node whose name or keys the
    /// SDK cannot decrypt (D119): `get_nodes` answers `None` for it, as for
    /// a node that does not exist, `get_nodes_checked` tells it apart
    /// (`Degraded`), a folder listing leaves it out and
    /// `list_children_checked` names it degraded (D132), and the
    /// shared-with-me listing names a share whose root it is as degraded.
    /// In this process only.
    pub fn set_degraded(&self, link: &LinkId, degraded: bool) {
        let mut state = self.state.lock();
        if degraded {
            state.degraded.insert(link.clone());
        } else {
            state.degraded.remove(link);
        }
    }

    /// Marks one signature of `link` as one that does not verify (fork
    /// P27, D132), claimed by `signer` (`None`: anonymous), or with
    /// `failed = false` as one that does; persisted, and announced with an
    /// event as a change of the node's metadata. A rename by a client signs
    /// the new name, which then verifies.
    pub fn set_unverified(
        &self,
        link: &LinkId,
        part: SignaturePart,
        failed: bool,
        signer: Option<&str>,
    ) -> Result<()> {
        self.with_state(|s| {
            s.node(link)?;
            s.update_node(link, |entry| {
                if failed {
                    entry
                        .unverified
                        .insert(part.as_str().to_owned(), signer.map(str::to_owned));
                } else {
                    entry.unverified.remove(part.as_str());
                }
            });
            s.touch(link);
            s.push_event(EventKind::UpdateMetadata, link);
            Ok(())
        })
    }

    /// Makes the next event poll of My Files' volume answer `refresh = true`.
    pub fn request_refresh(&self) {
        self.request_volume_refresh(MAIN_VOLUME);
    }

    /// Makes the next event poll of `volume` answer `refresh = true`.
    pub fn request_volume_refresh(&self, volume: &str) {
        self.state.lock().refresh_pending.insert(volume.to_owned());
    }

    /// A handle on the same drive that serves the tree with root `root` on
    /// `volume` (see the module doc).
    pub fn tree(&self, volume: &str, root: LinkId) -> Self {
        let mut client = (*self.client).clone();
        client.tree = Some(FakeTree {
            volume: volume.to_owned(),
            root,
        });
        self.with_client(client)
    }

    /// The volume this handle's events come from.
    fn volume(&self) -> &str {
        self.client
            .tree
            .as_ref()
            .map_or(MAIN_VOLUME, |tree| tree.volume.as_str())
    }

    /// Adds a root folder named `name` without a parent: with `owner`
    /// `None` (or the drive's user) on the drive's own volume, like a
    /// device's root; otherwise on `owner`'s volume, made at its first use,
    /// and shared with the drive's user as `role` when one is given.
    pub fn add_root(
        &self,
        name: &str,
        owner: Option<&str>,
        role: Option<FakeRole>,
    ) -> Result<AddedRoot> {
        self.with_state(|s| {
            let owner = owner
                .filter(|owner| *owner != s.username)
                .map(str::to_owned);
            let volume = match &owner {
                None => MAIN_VOLUME.to_owned(),
                Some(owner) => match s.volumes.iter().find(|(_, v)| &v.owner == owner) {
                    Some((id, _)) => id.clone(),
                    None => {
                        let id = s.new_id("vol-");
                        s.volumes.insert(
                            id.clone(),
                            FakeVolume {
                                owner: owner.clone(),
                            },
                        );
                        s.delta.volumes.insert(id.clone());
                        id
                    }
                },
            };
            let link = LinkId::new(s.new_id("link-"));
            s.put_node(Node {
                link: link.clone(),
                parent: None,
                name: name.to_owned(),
                kind: NodeKind::Folder,
                trashed: false,
                folder_mtime: Some(SystemTime::now()),
                revisions: Vec::new(),
                trashed_at: 0,
                volume: (volume != MAIN_VOLUME).then(|| volume.clone()),
                photo: None,
                unverified: BTreeMap::new(),
            });
            s.touch(&link);
            s.push_event(EventKind::Create, &link);
            let share = role.map(|role| s.add_share(&volume, &link, role));
            Ok(AddedRoot {
                volume,
                root: link,
                share,
            })
        })
    }

    /// Adds a file named `name` with `content` on `owner`'s volume, in a
    /// folder of its own that is not shared, and shares the file alone with
    /// the drive's user as `role` (a shared single file, M5.12).
    pub async fn add_shared_file(
        &self,
        name: &str,
        content: &[u8],
        owner: &str,
        role: FakeRole,
    ) -> Result<AddedRoot> {
        let holder = self.add_root(&format!("holder of {name}"), Some(owner), None)?;
        let as_owner = self.with_client(FakeClient {
            user: Some(owner.to_owned()),
            ..FakeClient::default()
        });
        let meta = FileMeta {
            mtime: SystemTime::now(),
            mode: Some(0o644),
            media_type: "application/octet-stream".into(),
            sha1: None,
        };
        let source = UploadSource::Bytes(Bytes::copy_from_slice(content));
        let (link, _) = as_owner
            .upload_new(&holder.root, name, source, meta)
            .await?;
        let volume = holder.volume;
        let share = self.with_state(|s| Ok(s.add_share(&volume, &link, role)))?;
        Ok(AddedRoot {
            volume,
            root: link,
            share: Some(share),
        })
    }

    /// Sends one `DriveShareRefresh` on the core event stream: a Shared
    /// mount polling it reads the listing again (M5.12). Changes to shares
    /// send none by themselves, so a test decides when the signal comes.
    pub fn core_signal(&self) -> Result<()> {
        self.with_state(|s| {
            s.core_events += 1;
            s.delta.meta = true;
            Ok(())
        })
    }

    /// Gives `user` the role `role` in `share`, or with `None` removes the
    /// membership.
    pub fn set_member_role(&self, share: &str, user: &str, role: Option<FakeRole>) -> Result<()> {
        self.with_state(|s| {
            let entry = s.shares.get_mut(share).ok_or(BackendError::NotFound)?;
            match role {
                Some(role) => entry.members.insert(user.to_owned(), role),
                None => entry.members.remove(user),
            };
            s.delta.shares.insert(share.to_owned());
            Ok(())
        })
    }

    /// Gives the drive's user the role `role` in `share`; `None` revokes
    /// the share for them.
    pub fn set_own_role(&self, share: &str, role: Option<FakeRole>) -> Result<()> {
        let user = self.with_state(|s| Ok(s.username.clone()))?;
        self.set_member_role(share, &user, role)
    }

    /// Adds a photo named `name` to the drive user's Photos (M5.13): a file
    /// in the Photos root, on [`PHOTOS_VOLUME`] (made at the first photo),
    /// with its timeline fields; with `main`, a related photo of that one
    /// (a Live Photo's video). Sends a `Create` event on the Photos volume.
    /// Returns the photo's link.
    pub async fn add_photo(
        &self,
        name: &str,
        source: UploadSource,
        capture_time: i64,
        tags: Vec<u8>,
        main: Option<LinkId>,
    ) -> Result<LinkId> {
        let staged = self.stage(source).await?;
        let meta = FileMeta {
            mtime: UNIX_EPOCH + Duration::from_secs(capture_time.max(0) as u64),
            mode: None,
            media_type: "image/jpeg".into(),
            sha1: None,
        };
        self.with_state(|s| {
            let root = s.photos_root();
            s.ensure_name_free(&root, name, None)?;
            if let Some(main) = &main {
                let node = s.node(main)?;
                if node.photo.is_none() || node.parent.as_ref() != Some(&root) {
                    return Err(BackendError::Other(format!("{main} is not a photo")));
                }
            }
            s.ensure_quota(staged.size)?;
            let link = LinkId::new(s.new_id("photo-"));
            let (_, rev) = s.new_revision(staged, &meta, &self.client)?;
            s.put_node(Node {
                link: link.clone(),
                parent: Some(root),
                name: name.to_owned(),
                kind: NodeKind::File,
                trashed: false,
                folder_mtime: None,
                revisions: vec![rev],
                trashed_at: 0,
                volume: None,
                photo: Some(FakePhoto {
                    capture_time,
                    tags,
                    main,
                    thumbnail: Some(FakeThumbnail::DEFAULT),
                }),
                unverified: BTreeMap::new(),
            });
            s.touch(&link);
            s.push_event(EventKind::Create, &link);
            s.push_change(FakeChange::CreatedFile {
                link: link.clone(),
                name: name.to_owned(),
            });
            Ok(link)
        })
    }

    /// Sets or removes the thumbnail of photo `link` (M5.13b);
    /// [`FakeBackend::add_photo`] gives every photo
    /// [`FakeThumbnail::DEFAULT`].
    pub fn set_photo_thumbnail(
        &self,
        link: &LinkId,
        thumbnail: Option<FakeThumbnail>,
    ) -> Result<()> {
        self.with_state(|s| {
            let node = s.nodes.get_mut(link).ok_or(BackendError::NotFound)?;
            let photo = node
                .photo
                .as_mut()
                .ok_or_else(|| BackendError::Other(format!("{link} is not a photo")))?;
            photo.thumbnail = thumbnail;
            s.touch(link);
            Ok(())
        })
    }

    /// Makes the file `link` in the Photos root a photo of the timeline,
    /// as a draft whose upload finishes: the timeline lists it from now on,
    /// and an `UpdateMetadata` event of the Photos volume says so (D119).
    pub fn commit_photo(&self, link: &LinkId, capture_time: i64, tags: Vec<u8>) -> Result<()> {
        self.with_state(|s| {
            let root = s.photos_root();
            let node = s.nodes.get_mut(link).ok_or(BackendError::NotFound)?;
            if node.parent.as_ref() != Some(&root) || node.kind != NodeKind::File {
                return Err(BackendError::Other(format!(
                    "{link} is not a file in the Photos root"
                )));
            }
            node.photo = Some(FakePhoto {
                capture_time,
                tags,
                main: None,
                thumbnail: Some(FakeThumbnail::DEFAULT),
            });
            s.touch(link);
            s.push_event(EventKind::UpdateMetadata, link);
            Ok(())
        })
    }

    /// The photo named `name` in the Photos root, if any.
    pub fn photo_named(&self, name: &str) -> Option<LinkId> {
        self.with_state(|s| {
            let root = s.photos_root_if_any();
            Ok(root.and_then(|root| s.child_named(&root, name).map(|node| node.link.clone())))
        })
        .ok()
        .flatten()
    }

    /// The Photos volume and root, if the drive has photos (no request).
    pub fn photos_volume_now(&self) -> Option<PhotosVolume> {
        let root = self.with_state(|s| Ok(s.photos_root_if_any())).ok()??;
        Some(PhotosVolume {
            volume: PHOTOS_VOLUME.to_owned(),
            root,
        })
    }

    /// A handle on the Photos tree (made if missing): its volume and root.
    pub fn photos_tree(&self) -> Result<Self> {
        let root = self.with_state(|s| Ok(s.photos_root()))?;
        Ok(self.tree(PHOTOS_VOLUME, root))
    }

    /// The user calls of this handle act as.
    fn user(&self, s: &State) -> String {
        self.client
            .user
            .clone()
            .unwrap_or_else(|| s.username.clone())
    }

    /// Whether this handle's user may read `link`.
    fn may_read(&self, s: &State, link: &LinkId) -> bool {
        s.may_read(&self.user(s), link)
    }

    /// The root this handle serves.
    fn tree_root(&self, s: &State) -> LinkId {
        self.client
            .tree
            .as_ref()
            .map_or_else(|| s.root.clone(), |tree| tree.root.clone())
    }

    pub fn set_quota(&self, max: u64) {
        let _ = self.with_state(|s| {
            s.quota_max = max;
            s.delta.meta = true;
            Ok(())
        });
    }

    pub fn set_block_size(&self, block_size: usize) {
        assert!(block_size > 0, "block size must be positive");
        let _ = self.with_state(|s| {
            s.block_size = block_size;
            s.delta.meta = true;
            Ok(())
        });
    }

    /// How many revisions each file keeps (older ones are deleted).
    pub fn set_revision_limit(&self, keep: usize) {
        assert!(keep > 0, "files keep at least one revision");
        let _ = self.with_state(|s| {
            s.keep_revisions = keep;
            s.delta.meta = true;
            Ok(())
        });
    }

    /// Stops the next call at `point` made through a handle with this
    /// handle's client UID, until the returned pause is released.
    pub fn pause_at(&self, point: FakePoint) -> FakePause {
        let channels = Arc::new(PauseChannels {
            reached: watch::channel(false).0,
            release: watch::channel(false).0,
        });
        self.state.lock().pauses.push(PendingPause {
            point,
            client: self.client.uid.clone(),
            channels: Arc::clone(&channels),
        });
        FakePause(channels)
    }

    async fn checkpoint(&self, point: FakePoint) {
        let pause = {
            let mut state = self.state.lock();
            let position = state
                .pauses
                .iter()
                .position(|p| p.point == point && p.client == self.client.uid);
            position.map(|i| state.pauses.remove(i))
        };
        if let Some(pause) = pause {
            pause.channels.reached.send_replace(true);
            let mut release = pause.channels.release.subscribe();
            let _ = release.wait_for(|released| *released).await;
        }
    }

    /// Whether commits check that the base revision is still active (the
    /// real server checks it only when the draft is created, M3.1).
    pub fn set_commit_checks(&self, enforced: bool) {
        self.state.lock().commit_checks_revision = enforced;
    }

    /// Whether renames check the name hash (the real server does, M3.1).
    pub fn set_name_hash_enforced(&self, enforced: bool) {
        self.state.lock().name_hash_enforced = enforced;
    }

    /// SHA-1 of every kept revision of every node, trashed ones included:
    /// what is still reachable on the server.
    pub fn all_revision_sha1s(&self) -> HashSet<[u8; 20]> {
        self.with_state(|s| {
            Ok(s.nodes
                .values()
                .flat_map(|n| &n.revisions)
                .map(|r| r.sha1)
                .collect())
        })
        .unwrap_or_default()
    }

    /// The network profile of an in-memory fake (a persisted one reads
    /// `profile.json`, which replaces this when it changes).
    pub fn set_profile(&self, profile: NetProfile) {
        self.state.lock().profile = profile;
    }

    /// Delays every read call (root, listings, blocks) by `latency`.
    pub fn set_latency(&self, latency: Duration) {
        self.state.lock().latency = latency;
    }

    /// The most block downloads in flight at once over this process's
    /// handles since the last call (or the start); then starts over.
    pub fn take_peak_block_fetches(&self) -> usize {
        self.links.fetches.take_peak()
    }

    /// [`FakeBackend::take_peak_block_fetches`] for file uploads.
    pub fn take_peak_uploads(&self) -> usize {
        self.links.uploads.take_peak()
    }

    async fn delay(&self) {
        let latency = self.state.lock().latency;
        if !latency.is_zero() {
            tokio::time::sleep(latency).await;
        }
    }

    pub fn set_event_page_size(&self, page_size: usize) {
        assert!(page_size > 0, "event page size must be positive");
        self.state.lock().event_page_size = page_size;
    }

    /// Content of the active revision of a file.
    pub fn content(&self, file: &LinkId) -> Option<Bytes> {
        self.with_state(|s| {
            let Some(revision) = s.nodes.get(file).and_then(Node::active) else {
                return Ok(None);
            };
            s.content(revision).map(Some)
        })
        .ok()
        .flatten()
    }

    /// Revisions a file still keeps.
    pub fn revision_count(&self, file: &LinkId) -> usize {
        self.with_state(|s| Ok(s.nodes.get(file).map_or(0, |n| n.revisions.len())))
            .unwrap_or(0)
    }

    /// The changes still kept, in order.
    pub fn changes(&self) -> Vec<FakeChange> {
        self.with_state(|s| Ok(s.changes.clone()))
            .unwrap_or_default()
    }

    /// Kept changes with sequence numbers above `after`. Numbers start at 1
    /// and survive trimming, so callers can remember where they were.
    pub fn changes_since(&self, after: u64) -> Vec<(u64, FakeChange)> {
        self.with_state(|s| {
            Ok(s.changes
                .iter()
                .enumerate()
                .map(|(i, change)| (s.changes_base + i as u64 + 1, change.clone()))
                .filter(|(seq, _)| *seq > after)
                .collect())
        })
        .unwrap_or_default()
    }

    /// Uploads a local directory tree into `parent`, keeping file mtimes and
    /// permission bits (test setup).
    pub async fn import_dir(&self, local: &Path, parent: &LinkId) -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let mut entries: Vec<_> = std::fs::read_dir(local)
            .map_err(|e| io_error("read source", e))?
            .collect::<std::io::Result<_>>()
            .map_err(|e| io_error("read source", e))?;
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let meta = entry.metadata().map_err(|e| io_error("stat source", e))?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|name| BackendError::Other(format!("not UTF-8: {name:?}")))?;
            let mtime = meta.modified().map_err(|e| io_error("source mtime", e))?;
            if meta.is_dir() {
                let link = self.create_folder(parent, &name, mtime).await?;
                Box::pin(self.import_dir(&entry.path(), &link)).await?;
            } else if meta.is_file() {
                let file_meta = FileMeta {
                    mtime,
                    mode: Some(meta.permissions().mode() & 0o7777),
                    media_type: "application/octet-stream".into(),
                    sha1: None,
                };
                self.upload_new(parent, &name, UploadSource::File(entry.path()), file_meta)
                    .await?;
            }
        }
        Ok(())
    }

    /// Runs `f` on the locked, current state and journals what it changed.
    fn with_state<T>(&self, f: impl FnOnce(&mut State) -> Result<T>) -> Result<T> {
        let mut state = self.state.lock();
        let _lock = state.dir.as_deref().map(lock_dir).transpose()?;
        state.refresh()?;
        let result = f(&mut state);
        state.finish()?;
        result
    }

    /// The start of a call of `op`: its failures (`faults.json`, then an
    /// injected one), then `rtts` round trips of the network profile, slept
    /// without the lock. Returns the profile for the call's other costs.
    async fn enter(&self, op: FakeOp, rtts: u32) -> Result<NetProfile> {
        let profile = {
            let mut state = self.state.lock();
            state.check_fault_file(self.client.uid.as_deref(), op)?;
            state.take_fault(op)?;
            state.check_profile_file();
            state.profile
        };
        round_trips(&profile, rtts).await;
        Ok(profile)
    }

    /// The rest of an upload's cost after the draft: the blocks' round trip
    /// and transfer, then the commit's round trip.
    async fn send_blocks(&self, profile: &NetProfile, size: u64) {
        round_trips(profile, 1).await;
        transfer(&self.links.up, size, profile.up_bytes_per_sec).await;
        round_trips(profile, UPLOAD_RTTS - 2).await;
    }

    /// Reads a block located under the lock, after it was released.
    async fn read_block(&self, block: BlockRead, profile: &NetProfile) -> Result<Bytes> {
        transfer(&self.links.down, block.len(), profile.down_bytes_per_sec).await;
        match block {
            BlockRead::Memory(bytes) => Ok(bytes),
            BlockRead::Blob { path, start, len } => {
                tokio::task::spawn_blocking(move || read_blob(&path, start, len))
                    .await
                    .map_err(|e| BackendError::Other(format!("fake backend: read task: {e}")))?
            }
        }
    }

    /// Reads upload content: into memory, or into a temporary blob for a
    /// persisted fake, hashing it on the way.
    async fn stage(&self, source: UploadSource) -> Result<Staged> {
        let blobs = self.state.lock().dir.as_ref().map(|dir| dir.join("blobs"));
        tokio::task::spawn_blocking(move || stage_blocking(source, blobs))
            .await
            .map_err(|e| BackendError::Other(format!("fake backend: staging task: {e}")))?
    }
}

fn stage_blocking(source: UploadSource, blobs: Option<PathBuf>) -> Result<Staged> {
    let Some(blobs) = blobs else {
        let bytes = match source {
            UploadSource::Bytes(bytes) => bytes,
            UploadSource::File(path) => std::fs::read(&path)
                .map(Bytes::from)
                .map_err(|e| BackendError::Other(format!("cannot read {}: {e}", path.display())))?,
        };
        return Ok(Staged {
            size: bytes.len() as u64,
            sha1: sha1::Sha1::digest(&bytes).into(),
            content: Some(StagedContent::Memory(bytes)),
        });
    };
    let tmp = blobs.join(format!(
        "tmp-{}-{}",
        std::process::id(),
        TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    // From here on, dropping `staged` removes the temporary blob.
    let mut staged = Staged {
        size: 0,
        sha1: [0; 20],
        content: Some(StagedContent::Blob(tmp.clone())),
    };
    let mut out = std::fs::File::create(&tmp).map_err(|e| io_error("create blob", e))?;
    let mut hasher = sha1::Sha1::new();
    let mut write = |chunk: &[u8]| -> std::io::Result<()> {
        hasher.update(chunk);
        staged.size += chunk.len() as u64;
        out.write_all(chunk)
    };
    match source {
        UploadSource::Bytes(bytes) => write(&bytes).map_err(|e| io_error("write blob", e))?,
        UploadSource::File(path) => {
            let mut input = std::fs::File::open(&path)
                .map_err(|e| BackendError::Other(format!("cannot read {}: {e}", path.display())))?;
            // On the stack, so the soak's RSS check measures the daemon, not
            // the fake (D049).
            let mut buffer = [0u8; 64 << 10];
            loop {
                let n = input
                    .read(&mut buffer)
                    .map_err(|e| io_error("read source", e))?;
                if n == 0 {
                    break;
                }
                write(&buffer[..n]).map_err(|e| io_error("write blob", e))?;
            }
        }
    }
    staged.sha1 = hasher.finalize().into();
    Ok(staged)
}

impl FakeBackend {
    /// A revision's block list and its pages on this drive.
    fn block_pages<'a>(
        &'a self,
        file: &'a LinkId,
        revision: &'a RevisionId,
    ) -> (Arc<ListedRevision<(), ()>>, FakePages<'a>) {
        let kept = self.links.block_lists.revision(file, revision);
        (
            kept,
            FakePages {
                fake: self,
                file,
                revision,
            },
        )
    }

    /// One listed block into `buf`: a round trip and its transfer.
    async fn fetch_listed_into(
        &self,
        file: &LinkId,
        revision: &RevisionId,
        index: u32,
        buf: &mut [u8],
    ) -> Result<usize> {
        let _in_flight = self.links.fetches.enter();
        self.delay().await;
        let profile = self.enter(FakeOp::FetchBlock, 1).await?;
        let block = self.with_state(|s| {
            if !self.may_read(s, file) {
                return Err(BackendError::NotFound);
            }
            s.block(file, revision, index)
        })?;
        let (len, capacity) = (block.len(), buf.len());
        let target = buf.get_mut(..len as usize).ok_or_else(|| {
            BackendError::Integrity(format!(
                "block has {len} bytes, more than the {capacity} expected"
            ))
        })?;
        transfer(&self.links.down, len, profile.down_bytes_per_sec).await;
        match block {
            BlockRead::Memory(bytes) => target.copy_from_slice(&bytes),
            BlockRead::Blob { path, start, .. } => {
                // `buf` is borrowed, so not `spawn_blocking`: the read
                // moves this worker's other tasks away instead.
                let mut read = || -> Result<()> {
                    let file = std::fs::File::open(&path).map_err(|e| io_error("open blob", e))?;
                    std::os::unix::fs::FileExt::read_exact_at(&file, target, start)
                        .map_err(|e| io_error("read blob", e))
                };
                match tokio::runtime::Handle::current().runtime_flavor() {
                    tokio::runtime::RuntimeFlavor::MultiThread => {
                        tokio::task::block_in_place(read)?
                    }
                    _ => read()?,
                }
            }
        }
        Ok(len as usize)
    }
}

#[async_trait::async_trait]
impl Backend for FakeBackend {
    fn upload_block_size(&self) -> u64 {
        self.state.lock().block_size as u64
    }

    async fn account(&self) -> Result<AccountInfo> {
        if let Some(known) = &self.client.known_account {
            return Ok(known.clone());
        }
        self.enter(FakeOp::Account, 1).await?;
        self.with_state(|s| {
            let root = self.tree_root(s);
            if !self.may_read(s, &root) {
                return Err(BackendError::NotFound);
            }
            Ok(AccountInfo {
                username: self.user(s),
                root,
            })
        })
    }

    async fn root(&self) -> Result<RemoteNode> {
        self.delay().await;
        self.enter(FakeOp::Root, 1).await?;
        self.with_state(|s| {
            let root = self.tree_root(s);
            if !self.may_read(s, &root) {
                return Err(BackendError::NotFound);
            }
            Ok(s.node(&root)?.to_remote())
        })
    }

    async fn list_children(&self, folder: &LinkId) -> Result<Vec<RemoteNode>> {
        Ok(self.list_children_checked(folder).await?.children)
    }

    /// A degraded child ([`FakeBackend::set_degraded`]) is named in
    /// [`Listing::degraded`] instead of listed, as Proton's (fork P26).
    async fn list_children_checked(&self, folder: &LinkId) -> Result<Listing> {
        self.delay().await;
        self.checkpoint(FakePoint::ListBeforeRead).await;
        let profile = self.enter(FakeOp::ListChildren, 1).await?;
        let listing = self.with_state(|s| {
            s.folder(folder)?;
            if !self.may_read(s, folder) {
                return Err(BackendError::NotFound);
            }
            // By name, as the index keeps them.
            let mut listing = Listing::default();
            for link in s.children.get(folder).into_iter().flat_map(|c| c.values()) {
                if s.degraded.contains(link) {
                    listing.degraded.push(link.clone());
                } else {
                    listing.children.push(s.nodes[link].to_remote());
                }
            }
            Ok(listing)
        })?;
        // The pages after the first.
        let count = listing.children.len() + listing.degraded.len();
        round_trips(&profile, pages(count) - 1).await;
        Ok(listing)
    }

    async fn get_nodes(&self, ids: &[LinkId]) -> Result<Vec<Option<RemoteNode>>> {
        Ok(self
            .get_nodes_checked(ids)
            .await?
            .into_iter()
            .map(NodeLookup::found)
            .collect())
    }

    async fn get_nodes_checked(&self, ids: &[LinkId]) -> Result<Vec<NodeLookup>> {
        self.delay().await;
        self.enter(FakeOp::GetNodes, pages(ids.len())).await?;
        self.with_state(|s| {
            Ok(ids
                .iter()
                .map(
                    |id| match s.nodes.get(id).filter(|_| self.may_read(s, id)) {
                        None => NodeLookup::Absent,
                        Some(_) if s.degraded.contains(id) => NodeLookup::Degraded,
                        Some(node) => NodeLookup::Found(node.to_remote()),
                    },
                )
                .collect())
        })
    }

    /// The layout from the revision's claimed block sizes, as the adapter
    /// takes it from the node (D135): no listing, no round trip.
    async fn blocks(&self, file: &LinkId, revision: &RevisionId) -> Result<Vec<BlockInfo>> {
        self.delay().await;
        self.enter(FakeOp::Blocks, 0).await?;
        let blocks: Vec<BlockInfo> = self.with_state(|s| {
            let node = s
                .nodes
                .get(file)
                .filter(|_| self.may_read(s, file))
                .ok_or(BackendError::NotFound)?;
            let rev = node.revision(revision)?;
            let block = s.block_size as u64;
            Ok((0..rev.size.div_ceil(block))
                .map(|i| BlockInfo {
                    index: i as u32 + 1,
                    plain_size: Some((rev.size - i * block).min(block) as u32),
                })
                .collect())
        })?;
        self.links
            .block_lists
            .revision(file, revision)
            .set_count(blocks.len() as u32);
        Ok(blocks)
    }

    async fn fetch_block(&self, file: &LinkId, revision: &RevisionId, index: u32) -> Result<Bytes> {
        let (kept, pages) = self.block_pages(file, revision);
        let download = |_| async move {
            let _in_flight = self.links.fetches.enter();
            self.delay().await;
            let profile = self.enter(FakeOp::FetchBlock, 1).await?;
            let block = self.with_state(|s| {
                if !self.may_read(s, file) {
                    return Err(BackendError::NotFound);
                }
                s.block(file, revision, index)
            })?;
            self.read_block(block, &profile).await
        };
        self.links
            .block_lists
            .fetch(&kept, &pages, index, download)
            .await
    }

    /// Through the block lists, as the adapter (D135): the block's page
    /// when it is not listed, and the rest of the listing beside the first
    /// download of a revision, each page a round trip.
    async fn fetch_block_into(
        &self,
        file: &LinkId,
        revision: &RevisionId,
        index: u32,
        buf: &mut [u8],
    ) -> Result<usize> {
        let (kept, pages) = self.block_pages(file, revision);
        self.links
            .block_lists
            .fetch(&kept, &pages, index, |_| {
                self.fetch_listed_into(file, revision, index, buf)
            })
            .await
    }

    async fn upload_new(
        &self,
        parent: &LinkId,
        name: &str,
        source: UploadSource,
        meta: FileMeta,
    ) -> Result<(LinkId, RevisionId)> {
        let _in_flight = self.links.uploads.enter();
        let staged = self.stage(source).await?;
        let profile = self.enter(FakeOp::UploadNew, 1).await?;
        // Draft: the server checks the name when the draft is created ...
        self.with_state(|s| {
            s.folder(parent)?;
            s.check_write(&self.user(s), parent)?;
            s.ensure_name_free(parent, name, None)
        })?;
        self.send_blocks(&profile, staged.size).await;
        self.checkpoint(FakePoint::NewBeforeCommit).await;
        // ... and again when the file is committed.
        self.with_state(|s| {
            s.folder(parent)?;
            s.ensure_name_free(parent, name, None)?;
            s.ensure_quota(staged.size)?;
            let link = LinkId::new(s.new_id("link-"));
            let (revision, rev) = s.new_revision(staged, &meta, &self.client)?;
            s.put_node(Node {
                link: link.clone(),
                parent: Some(parent.clone()),
                name: name.to_owned(),
                kind: NodeKind::File,
                trashed: false,
                folder_mtime: None,
                revisions: vec![rev],
                trashed_at: 0,
                volume: None,
                photo: None,
                unverified: BTreeMap::new(),
            });
            s.touch(&link);
            s.push_event(EventKind::Create, &link);
            s.push_change(FakeChange::CreatedFile {
                link: link.clone(),
                name: name.to_owned(),
            });
            Ok((link, revision))
        })
    }

    async fn upload_revision(
        &self,
        file: &LinkId,
        base: &RevisionId,
        source: UploadSource,
        meta: FileMeta,
    ) -> Result<RevisionId> {
        let _in_flight = self.links.uploads.enter();
        let staged = self.stage(source).await?;
        let profile = self.enter(FakeOp::UploadRevision, 1).await?;
        // Draft: the base must be the active revision.
        self.with_state(|s| {
            let node = s.node(file)?;
            if node.kind != NodeKind::File {
                return Err(BackendError::Other(format!("{file} is not a file")));
            }
            s.check_write(&self.user(s), file)?;
            let current = node.active().map(|r| r.id.clone());
            if current.as_ref() != Some(base) {
                return Err(BackendError::RevisionConflict { current });
            }
            Ok(())
        })?;
        self.send_blocks(&profile, staged.size).await;
        self.checkpoint(FakePoint::RevisionBeforeCommit).await;
        self.with_state(|s| {
            let current = s.node(file)?.active().map(|r| r.id.clone());
            if s.commit_checks_revision && current.as_ref() != Some(base) {
                return Err(BackendError::RevisionConflict { current });
            }
            s.ensure_quota(staged.size)?;
            let (revision, rev) = s.new_revision(staged, &meta, &self.client)?;
            s.update_node(file, |node| node.revisions.push(rev));
            s.touch(file);
            s.push_event(EventKind::UpdateContent, file);
            s.push_change(FakeChange::NewRevision {
                link: file.clone(),
                revision: revision.clone(),
            });
            Ok(revision)
        })
    }

    async fn create_folder(
        &self,
        parent: &LinkId,
        name: &str,
        mtime: SystemTime,
    ) -> Result<LinkId> {
        self.checkpoint(FakePoint::CreateFolderBeforeCommit).await;
        self.enter(FakeOp::CreateFolder, 1).await?;
        self.with_state(|s| {
            s.folder(parent)?;
            s.check_write(&self.user(s), parent)?;
            s.ensure_name_free(parent, name, None)?;
            let link = LinkId::new(s.new_id("link-"));
            s.put_node(Node {
                link: link.clone(),
                parent: Some(parent.clone()),
                name: name.to_owned(),
                kind: NodeKind::Folder,
                trashed: false,
                folder_mtime: Some(mtime),
                revisions: Vec::new(),
                trashed_at: 0,
                volume: None,
                photo: None,
                unverified: BTreeMap::new(),
            });
            s.touch(&link);
            s.push_event(EventKind::Create, &link);
            s.push_change(FakeChange::CreatedFolder {
                link: link.clone(),
                name: name.to_owned(),
            });
            Ok(link)
        })
    }

    async fn rename(
        &self,
        node: &LinkId,
        new_parent: Option<&LinkId>,
        new_name: &str,
        expected: Option<&NameHash>,
    ) -> Result<()> {
        self.checkpoint(FakePoint::RenameBeforeCommit).await;
        self.enter(FakeOp::Rename, 1).await?;
        self.with_state(|s| {
            if node == &s.root {
                return Err(BackendError::PermissionDenied);
            }
            let current = s.node(node)?;
            if s.name_hash_enforced
                && expected.is_some_and(|hash| *hash != name_hash(&current.name))
            {
                return Err(BackendError::NameConflict);
            }
            let parent = match new_parent {
                Some(parent) => parent.clone(),
                None => current
                    .parent
                    .clone()
                    .ok_or(BackendError::PermissionDenied)?,
            };
            s.folder(&parent)?;
            let user = self.user(s);
            s.check_write(&user, node)?;
            s.check_write(&user, &parent)?;
            if s.is_descendant(&parent, node) || s.volume_of(&parent) != s.volume_of(node) {
                return Err(BackendError::InvalidMove);
            }
            s.ensure_name_free(&parent, new_name, Some(node))?;
            s.update_node(node, |entry| {
                entry.parent = Some(parent);
                if entry.name != new_name {
                    // The client signs the new name (D132).
                    entry.unverified.remove(SignaturePart::Name.as_str());
                }
                entry.name = new_name.to_owned();
            });
            s.touch(node);
            s.push_event(EventKind::UpdateMetadata, node);
            s.push_change(FakeChange::Renamed {
                link: node.clone(),
                name: new_name.to_owned(),
            });
            Ok(())
        })?;
        match self.state.lock().lost_rename_replies.pop_front() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    async fn trash(&self, nodes: &[LinkId]) -> Result<Vec<Result<()>>> {
        self.checkpoint(FakePoint::TrashBeforeCommit).await;
        self.enter(FakeOp::Trash, 1).await?;
        self.with_state(|s| {
            Ok(nodes
                .iter()
                .map(|link| {
                    if link == &s.root || s.node(link)?.parent.is_none() {
                        return Err(BackendError::PermissionDenied);
                    }
                    s.check_write(&self.user(s), link)?;
                    let at = s.generation + 1;
                    s.update_node(link, |entry| {
                        entry.trashed = true;
                        entry.trashed_at = at;
                    });
                    s.touch(link);
                    s.delta.trashed = true;
                    s.push_event(EventKind::Delete, link);
                    s.push_change(FakeChange::Trashed { link: link.clone() });
                    Ok(())
                })
                .collect())
        })
    }

    async fn revisions(&self, file: &LinkId) -> Result<Vec<RevisionMeta>> {
        self.enter(FakeOp::Revisions, 1).await?;
        self.with_state(|s| {
            let node = s
                .nodes
                .get(file)
                .filter(|_| self.may_read(s, file))
                .ok_or(BackendError::NotFound)?;
            if node.kind != NodeKind::File {
                return Err(BackendError::Other(format!("{file} is not a file")));
            }
            let last = node.revisions.len().saturating_sub(1);
            Ok(node
                .revisions
                .iter()
                .enumerate()
                .map(|(i, r)| RevisionMeta {
                    id: r.id.clone(),
                    created: UNIX_EPOCH + Duration::from_secs(r.seq),
                    size: Some(r.size),
                    sha1: r.sha1_claimed.then_some(r.sha1),
                    client_uid: r.client_uid.clone(),
                    active: i == last,
                })
                .collect())
        })
    }

    async fn latest_event_id(&self) -> Result<EventId> {
        self.enter(FakeOp::LatestEventId, 1).await?;
        self.with_state(|s| Ok(EventId::new(s.latest_event().to_string())))
    }

    async fn poll_events(&self, since: &EventId) -> Result<EventPage> {
        self.enter(FakeOp::PollEvents, 1).await?;
        self.with_state(|s| {
            let latest = s.latest_event();
            let since: u64 = since
                .as_str()
                .parse()
                .ok()
                .filter(|&i| i <= latest)
                .ok_or_else(|| BackendError::Other(format!("unknown event id {since}")))?;
            let volume = self.volume();
            // A cursor older than the kept events: the client must refresh.
            if s.refresh_pending.remove(volume) || since < s.events_base {
                return Ok(EventPage {
                    next: EventId::new(latest.to_string()),
                    more: false,
                    refresh: true,
                    events: Vec::new(),
                });
            }
            // Only this volume's events; the ids count every volume's.
            let mut end = (since - s.events_base) as usize;
            let mut events = Vec::new();
            while end < s.events.len() && events.len() < s.event_page_size {
                if s.events[end].volume() == volume {
                    events.push(s.events[end].event.clone());
                }
                end += 1;
            }
            Ok(EventPage {
                next: EventId::new((s.events_base + end as u64).to_string()),
                more: s.events[end..].iter().any(|event| event.volume() == volume),
                refresh: false,
                events,
            })
        })
    }

    async fn storage_info(&self) -> Result<StorageInfo> {
        self.enter(FakeOp::StorageInfo, 1).await?;
        self.with_state(|s| {
            Ok(StorageInfo {
                used: s.used,
                max: s.quota_max,
            })
        })
    }
}

/// The fake's shared-with-me (M5.12): the shares of other users' volumes
/// with this handle's user as a member, oldest first; the core stream
/// carries one `DriveShareRefresh` per [`FakeBackend::core_signal`].
#[async_trait::async_trait]
impl SharedWithMe for FakeBackend {
    async fn shared_with_me(&self) -> Result<Vec<SharedEntry>> {
        Ok(self.shared_listing().await?.entries)
    }

    /// A share whose root is degraded ([`FakeBackend::set_degraded`]) is
    /// listed as degraded, as Proton's (D119).
    async fn shared_listing(&self) -> Result<SharedListing> {
        self.enter(FakeOp::SharedWithMe, 1).await?;
        self.with_state(|s| {
            let user = self.user(s);
            let mut shares: Vec<(&String, &FakeShare)> = s
                .shares
                .iter()
                .filter(|(_, share)| s.owner_of(&share.volume) != user)
                .collect();
            shares.sort_by_key(|(id, share)| (share.created, (*id).clone()));
            let mut degraded = Vec::new();
            let entries = shares
                .into_iter()
                .filter_map(|(id, share)| {
                    let role = share.members.get(&user)?;
                    let node = s.node(&share.root).ok()?;
                    if s.degraded.contains(&share.root) {
                        degraded.push(DegradedShare {
                            share: id.clone(),
                            volume: share.volume.clone(),
                            root: share.root.clone(),
                        });
                        return None;
                    }
                    Some(SharedEntry {
                        share: id.clone(),
                        volume: share.volume.clone(),
                        root: share.root.clone(),
                        kind: node.kind,
                        name: node.name.clone(),
                        owner: s.owner_of(&share.volume).to_owned(),
                        role: match role {
                            FakeRole::Viewer => ShareRole::Viewer,
                            FakeRole::Editor => ShareRole::Editor,
                            FakeRole::Admin => ShareRole::Admin,
                        },
                        created: Some(UNIX_EPOCH + Duration::from_secs(share.created)),
                    })
                })
                .collect();
            Ok(SharedListing { entries, degraded })
        })
    }

    async fn latest_core_event_id(&self) -> Result<EventId> {
        self.enter(FakeOp::CoreEvents, 1).await?;
        self.with_state(|s| Ok(EventId::new(format!("core-{}", s.core_events))))
    }

    async fn poll_core_events(&self, since: &EventId) -> Result<CorePage> {
        self.enter(FakeOp::CoreEvents, 1).await?;
        self.with_state(|s| {
            let since: u64 = since
                .as_str()
                .strip_prefix("core-")
                .and_then(|n| n.parse().ok())
                .filter(|&n| n <= s.core_events)
                .ok_or_else(|| BackendError::Other(format!("unknown core event id {since}")))?;
            Ok(CorePage {
                next: EventId::new(format!("core-{}", s.core_events)),
                more: false,
                refresh: false,
                shares_changed: since < s.core_events,
            })
        })
    }
}

/// Photos as the server keeps them (M5.13): [`PHOTOS_VOLUME`] and its
/// timeline, a page of [`TIMELINE_PAGE`] per call.
#[async_trait::async_trait]
impl Photos for FakeBackend {
    async fn photos_volume(&self) -> Result<Option<PhotosVolume>> {
        self.enter(FakeOp::Timeline, 1).await?;
        self.with_state(|s| {
            Ok(s.photos_root_if_any().map(|root| PhotosVolume {
                volume: PHOTOS_VOLUME.to_owned(),
                root,
            }))
        })
    }

    async fn timeline_page(
        &self,
        volume: &str,
        after: Option<&LinkId>,
    ) -> Result<Vec<TimelineItem>> {
        self.delay().await;
        self.enter(FakeOp::Timeline, 1).await?;
        if volume != PHOTOS_VOLUME {
            return Err(BackendError::NotFound);
        }
        self.with_state(|s| {
            let items = s.timeline();
            let start = match after {
                None => 0,
                Some(after) => match items.iter().position(|item| &item.link == after) {
                    Some(at) => at + 1,
                    // The anchor is gone: as the server, an empty page.
                    None => return Ok(Vec::new()),
                },
            };
            Ok(items.into_iter().skip(start).take(TIMELINE_PAGE).collect())
        })
    }

    async fn thumbnail_ref(
        &self,
        volume: &str,
        link: &LinkId,
        revision: &RevisionId,
    ) -> Result<Option<ThumbnailRef>> {
        self.delay().await;
        // One request: the first page of the revision (D135).
        self.enter(FakeOp::Thumbnails, 1).await?;
        if volume != PHOTOS_VOLUME {
            return Err(BackendError::NotFound);
        }
        self.with_state(|s| {
            let thumbnail = s.photo_thumbnail(link, revision)?;
            Ok(thumbnail.map(|thumbnail| {
                let signed = thumbnail.signed_bytes(link.as_str());
                ThumbnailRef {
                    link: link.clone(),
                    revision: revision.clone(),
                    id: format!("thumb-{revision}"),
                    digest: sha2::Sha256::digest(&signed).to_vec(),
                    size: signed.len() as u64,
                }
            }))
        })
    }

    async fn fetch_thumbnails(
        &self,
        volume: &str,
        thumbnails: &[ThumbnailRef],
    ) -> Result<Vec<Result<Bytes>>> {
        self.delay().await;
        self.enter(FakeOp::Thumbnails, 1).await?;
        if volume != PHOTOS_VOLUME {
            return Err(BackendError::NotFound);
        }
        if thumbnails.len() > THUMBNAIL_BATCH {
            return Err(BackendError::Other(format!(
                "{} thumbnails in one request, more than {THUMBNAIL_BATCH}",
                thumbnails.len()
            )));
        }
        self.with_state(|s| {
            Ok(thumbnails
                .iter()
                .map(|wanted| {
                    let thumbnail = s
                        .photo_thumbnail(&wanted.link, &wanted.revision)?
                        .ok_or(BackendError::NotFound)?;
                    let served = thumbnail.served_bytes(wanted.link.as_str());
                    // As `ProtonBackend`: the size, then the digest that
                    // the verified manifest gave.
                    if served.len() > MAX_THUMBNAIL_BYTES {
                        return Err(BackendError::Integrity(format!(
                            "the thumbnail of {} has {} bytes",
                            wanted.link,
                            served.len()
                        )));
                    }
                    if sha2::Sha256::digest(&served).as_slice() != wanted.digest.as_slice() {
                        return Err(BackendError::Integrity(format!(
                            "the thumbnail of {} is not the one its revision's manifest signs",
                            wanted.link
                        )));
                    }
                    Ok(Bytes::from(served))
                })
                .collect())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn the_timeline_pages_newest_first_with_related_photos() {
        let fake = FakeBackend::new();
        assert_eq!(fake.photos_volume().await.expect("volume"), None);
        let photo = |n: usize| UploadSource::Bytes(Bytes::from(format!("photo {n}")));
        let count = TIMELINE_PAGE * 2 + 10;
        for n in 0..count {
            fake.add_photo(
                &format!("P{n}.jpg"),
                photo(n),
                n as i64 * 60,
                Vec::new(),
                None,
            )
            .await
            .expect("add");
        }
        let still = fake
            .add_photo("L.HEIC", photo(9999), 1_000_000, vec![3], None)
            .await
            .expect("still");
        let video = fake
            .add_photo(
                "L.MOV",
                photo(9998),
                1_000_001,
                vec![2],
                Some(still.clone()),
            )
            .await
            .expect("video");
        let volume = fake.photos_volume().await.expect("volume").expect("some");
        assert_eq!(volume.volume, PHOTOS_VOLUME);
        let mut pages = Vec::new();
        let mut after = None;
        loop {
            let page = fake
                .timeline_page(&volume.volume, after.as_ref())
                .await
                .expect("page");
            let last = page.last().map(|item| item.link.clone());
            let full = page.len() == TIMELINE_PAGE;
            pages.push(page);
            if !full {
                break;
            }
            after = last;
        }
        assert_eq!(
            pages.iter().map(Vec::len).collect::<Vec<_>>(),
            [TIMELINE_PAGE, TIMELINE_PAGE, 11],
            "the video is no item of its own"
        );
        let first = &pages[0][0];
        assert_eq!(first.link, still);
        assert_eq!(first.tags, [3]);
        assert_eq!(first.related.len(), 1);
        assert_eq!(first.related[0].link, video);
        let times: Vec<i64> = pages
            .iter()
            .flatten()
            .map(|item| item.capture_time)
            .collect();
        assert!(
            times.windows(2).all(|pair| pair[0] >= pair[1]),
            "newest first"
        );
        // Photos live on their own volume: the tree handle sees their
        // events and nodes, My Files' does not.
        let tree = fake.photos_tree().expect("tree");
        let nodes = tree
            .get_nodes(std::slice::from_ref(&video))
            .await
            .expect("nodes");
        assert_eq!(
            nodes[0].as_ref().map(|node| node.name.as_str()),
            Some("L.MOV")
        );
        let page = tree.poll_events(&EventId::new("0")).await.expect("events");
        assert!(page.more && !page.events.is_empty());
        assert!(
            page.events
                .iter()
                .all(|event| event.kind == EventKind::Create)
        );
        let mine = fake.poll_events(&EventId::new("0")).await.expect("events");
        assert!(mine.events.is_empty());
    }

    fn meta() -> FileMeta {
        FileMeta {
            mtime: SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            mode: Some(0o644),
            media_type: "application/octet-stream".into(),
            sha1: None,
        }
    }

    fn bytes(data: &'static [u8]) -> UploadSource {
        UploadSource::Bytes(Bytes::from_static(data))
    }

    async fn root(fake: &FakeBackend) -> LinkId {
        fake.root().await.expect("root").link_id
    }

    #[tokio::test]
    async fn files_and_folders_are_listed_with_metadata() {
        let fake = FakeBackend::new();
        let root = root(&fake).await;
        let docs = fake
            .create_folder(&root, "docs", SystemTime::UNIX_EPOCH)
            .await
            .expect("mkdir");
        let (file, rev) = fake
            .upload_new(&docs, "a.txt", bytes(b"hello"), meta())
            .await
            .expect("upload");

        let children = fake.list_children(&docs).await.expect("list");
        assert_eq!(children.len(), 1);
        let node = &children[0];
        assert_eq!(node.name, "a.txt");
        assert_eq!(node.kind, NodeKind::File);
        assert_eq!(node.active_revision.as_ref(), Some(&rev));
        assert_eq!(node.size, Some(5));
        assert_eq!(node.mtime, Some(meta().mtime));
        assert_eq!(node.mode, Some(0o644));
        assert_eq!(node.sha1, Some(sha1::Sha1::digest(b"hello").into()));
        assert_eq!(node.parent_link_id.as_ref(), Some(&docs));

        let root_children = fake.list_children(&root).await.expect("list root");
        assert_eq!(root_children[0].kind, NodeKind::Folder);
        let fetched = fake
            .get_nodes(&[file, LinkId::new("missing")])
            .await
            .expect("get");
        assert!(fetched[0].is_some() && fetched[1].is_none());
    }

    #[tokio::test]
    async fn names_are_unique_per_folder() {
        let fake = FakeBackend::new();
        let root = root(&fake).await;
        let (first, _) = fake
            .upload_new(&root, "x", bytes(b"1"), meta())
            .await
            .expect("upload");
        let error = fake
            .upload_new(&root, "x", bytes(b"2"), meta())
            .await
            .unwrap_err();
        assert_eq!(
            error,
            BackendError::AlreadyExists {
                existing: Some(first)
            }
        );
        assert!(matches!(
            fake.create_folder(&root, "x", SystemTime::UNIX_EPOCH).await,
            Err(BackendError::AlreadyExists { .. })
        ));
    }

    #[tokio::test]
    async fn new_revisions_require_the_active_base() {
        let fake = FakeBackend::new();
        let root = root(&fake).await;
        let (file, first) = fake
            .upload_new(&root, "f", bytes(b"v1"), meta())
            .await
            .expect("upload");
        let second = fake
            .upload_revision(&file, &first, bytes(b"v2"), meta())
            .await
            .expect("revision");
        let stale = fake
            .upload_revision(&file, &first, bytes(b"v3"), meta())
            .await
            .unwrap_err();
        assert_eq!(
            stale,
            BackendError::RevisionConflict {
                current: Some(second)
            }
        );
        assert_eq!(fake.content(&file).as_deref(), Some(&b"v2"[..]));
        assert_eq!(fake.revision_count(&file), 2);
    }

    #[tokio::test]
    async fn rename_checks_name_hash_target_and_cycles() {
        let fake = FakeBackend::new();
        let root = root(&fake).await;
        let a = fake
            .create_folder(&root, "a", SystemTime::UNIX_EPOCH)
            .await
            .expect("a");
        let b = fake
            .create_folder(&a, "b", SystemTime::UNIX_EPOCH)
            .await
            .expect("b");
        let (file, _) = fake
            .upload_new(&root, "f", bytes(b"x"), meta())
            .await
            .expect("f");

        let hash = fake
            .get_nodes(std::slice::from_ref(&file))
            .await
            .expect("get")[0]
            .as_ref()
            .and_then(|n| n.name_hash.clone())
            .expect("hash");
        fake.rename(&file, Some(&b), "g", Some(&hash))
            .await
            .expect("move and rename");
        assert_eq!(fake.list_children(&b).await.expect("list")[0].name, "g");
        assert_eq!(
            fake.rename(&file, None, "h", Some(&hash)).await,
            Err(BackendError::NameConflict)
        );
        assert!(fake.rename(&a, Some(&b), "a", None).await.is_err());
        assert_eq!(
            fake.rename(&root, None, "x", None).await,
            Err(BackendError::PermissionDenied)
        );
    }

    #[tokio::test]
    async fn trash_reports_per_node_results_and_hides_nodes() {
        let fake = FakeBackend::new();
        let root = root(&fake).await;
        let (file, _) = fake
            .upload_new(&root, "f", bytes(b"x"), meta())
            .await
            .expect("f");
        let results = fake
            .trash(&[file.clone(), LinkId::new("missing")])
            .await
            .expect("trash");
        assert_eq!(results, vec![Ok(()), Err(BackendError::NotFound)]);
        assert!(fake.list_children(&root).await.expect("list").is_empty());
        let trashed = fake.get_nodes(&[file]).await.expect("get");
        assert!(trashed[0].as_ref().is_some_and(|n| n.trashed));
        // The name is free again once the old node is trashed.
        fake.upload_new(&root, "f", bytes(b"y"), meta())
            .await
            .expect("reuse name");
    }

    #[tokio::test]
    async fn events_are_paged_and_refresh_can_be_requested() {
        let fake = FakeBackend::new();
        fake.set_event_page_size(2);
        let root = root(&fake).await;
        let cursor = fake.latest_event_id().await.expect("cursor");
        let (file, rev) = fake
            .upload_new(&root, "f", bytes(b"1"), meta())
            .await
            .expect("f");
        fake.upload_revision(&file, &rev, bytes(b"2"), meta())
            .await
            .expect("rev");
        fake.rename(&file, None, "g", None).await.expect("rename");

        let first = fake.poll_events(&cursor).await.expect("poll");
        assert!(first.more && !first.refresh);
        let kinds: Vec<_> = first.events.iter().map(|e| e.kind).collect();
        assert_eq!(kinds, [EventKind::Create, EventKind::UpdateContent]);
        let second = fake.poll_events(&first.next).await.expect("poll");
        assert!(!second.more);
        assert_eq!(second.events[0].kind, EventKind::UpdateMetadata);
        assert_eq!(second.events[0].parent_link_id.as_ref(), Some(&root));
        assert!(
            fake.poll_events(&second.next)
                .await
                .expect("poll")
                .events
                .is_empty()
        );

        fake.request_refresh();
        assert!(fake.poll_events(&second.next).await.expect("poll").refresh);
        assert!(!fake.poll_events(&second.next).await.expect("poll").refresh);
        assert!(fake.poll_events(&EventId::new("999")).await.is_err());
    }

    #[tokio::test]
    async fn blocks_split_content_at_the_block_size() {
        let fake = FakeBackend::new();
        fake.set_block_size(4);
        let root = root(&fake).await;
        let (file, rev) = fake
            .upload_new(&root, "f", bytes(b"0123456789"), meta())
            .await
            .expect("f");
        let blocks = fake.blocks(&file, &rev).await.expect("blocks");
        let sizes: Vec<_> = blocks.iter().map(|b| (b.index, b.plain_size)).collect();
        assert_eq!(sizes, [(1, Some(4)), (2, Some(4)), (3, Some(2))]);
        assert_eq!(
            fake.fetch_block(&file, &rev, 3).await.expect("block"),
            &b"89"[..]
        );
        assert_eq!(
            fake.fetch_block(&file, &rev, 4).await,
            Err(BackendError::NotFound)
        );
        assert_eq!(
            fake.fetch_block(&file, &rev, 0).await,
            Err(BackendError::NotFound)
        );
        let mut buf = [0u8; 4];
        assert_eq!(fake.fetch_block_into(&file, &rev, 2, &mut buf).await, Ok(4));
        assert_eq!(&buf, b"4567");
        assert_eq!(fake.fetch_block_into(&file, &rev, 3, &mut buf).await, Ok(2));
        assert!(matches!(
            fake.fetch_block_into(&file, &rev, 1, &mut buf[..3]).await,
            Err(BackendError::Integrity(_))
        ));
    }

    #[tokio::test]
    async fn quota_counts_every_revision() {
        let fake = FakeBackend::new();
        fake.set_quota(5);
        let root = root(&fake).await;
        let (file, rev) = fake
            .upload_new(&root, "f", bytes(b"123"), meta())
            .await
            .expect("f");
        assert_eq!(
            fake.upload_revision(&file, &rev, bytes(b"456"), meta())
                .await,
            Err(BackendError::QuotaExceeded)
        );
        assert_eq!(
            fake.storage_info().await.expect("info"),
            StorageInfo { used: 3, max: 5 }
        );
    }

    #[tokio::test]
    async fn injected_failures_fire_once_for_their_operation() {
        let fake = FakeBackend::new();
        fake.inject_failure(FakeOp::ListChildren, BackendError::Transient("503".into()));
        let root = root(&fake).await;
        assert_eq!(
            fake.list_children(&root).await,
            Err(BackendError::Transient("503".into()))
        );
        assert!(fake.list_children(&root).await.is_ok());
    }

    #[tokio::test]
    async fn a_lost_rename_reply_fails_a_rename_that_took_effect() {
        let fake = FakeBackend::new();
        let root = root(&fake).await;
        let (file, _) = fake
            .upload_new(&root, "a", bytes(b"x"), meta())
            .await
            .expect("upload");
        fake.lose_rename_reply(BackendError::Transient("timeout".into()));
        assert_eq!(
            fake.rename(&file, None, "b", None).await,
            Err(BackendError::Transient("timeout".into()))
        );
        let node = fake
            .get_nodes(std::slice::from_ref(&file))
            .await
            .expect("get")[0]
            .clone()
            .expect("node");
        assert_eq!(node.name, "b");
        fake.rename(&file, None, "c", None)
            .await
            .expect("once only");
    }

    #[tokio::test]
    async fn clones_share_one_drive_like_two_devices() {
        let device_a = FakeBackend::new();
        let device_b = device_a.clone();
        let root = root(&device_a).await;
        let cursor = device_b.latest_event_id().await.expect("cursor");
        let (file, _) = device_a
            .upload_new(&root, "shared", bytes(b"x"), meta())
            .await
            .expect("upload");
        let page = device_b.poll_events(&cursor).await.expect("poll");
        assert_eq!(page.events[0].link_id, file);
        assert_eq!(
            device_a.changes(),
            vec![FakeChange::CreatedFile {
                link: file,
                name: "shared".into()
            }]
        );
    }

    #[tokio::test]
    async fn a_persisted_fake_survives_reopening() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (file, rev) = {
            let fake = FakeBackend::open(dir.path()).expect("open");
            fake.set_block_size(4);
            let root = root(&fake).await;
            let docs = fake
                .create_folder(&root, "docs", SystemTime::UNIX_EPOCH)
                .await
                .expect("mkdir");
            fake.upload_new(&docs, "a", bytes(b"0123456789"), meta())
                .await
                .expect("upload")
        };
        let fake = FakeBackend::open(dir.path()).expect("reopen");
        assert_eq!(fake.content(&file).as_deref(), Some(&b"0123456789"[..]));
        assert_eq!(
            fake.fetch_block(&file, &rev, 3).await.expect("block"),
            &b"89"[..]
        );
        // From the blob file.
        let mut buf = [0u8; 4];
        assert_eq!(fake.fetch_block_into(&file, &rev, 2, &mut buf).await, Ok(4));
        assert_eq!(&buf, b"4567");
        let listed = fake
            .list_children(&LinkId::new("link-1"))
            .await
            .expect("list");
        assert_eq!(
            listed[0].sha1,
            Some(sha1::Sha1::digest(b"0123456789").into())
        );
        assert_eq!(fake.changes().len(), 2);
        // The next ids continue after the stored ones.
        let (other, _) = fake
            .upload_new(&root(&fake).await, "b", bytes(b"x"), meta())
            .await
            .expect("upload");
        assert_ne!(other, file);
    }

    #[tokio::test]
    async fn two_handles_on_one_directory_see_each_others_changes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first = FakeBackend::open(dir.path()).expect("open");
        let second = FakeBackend::open(dir.path()).expect("open");
        let root = root(&first).await;
        let (file, rev) = first
            .upload_new(&root, "f", bytes(b"v1"), meta())
            .await
            .expect("upload");
        second
            .upload_revision(&file, &rev, bytes(b"v2"), meta())
            .await
            .expect("revision from the other handle");
        assert_eq!(first.content(&file).as_deref(), Some(&b"v2"[..]));
        assert!(matches!(
            first
                .upload_revision(&file, &rev, bytes(b"v3"), meta())
                .await,
            Err(BackendError::RevisionConflict { .. })
        ));
    }

    #[tokio::test]
    async fn failed_uploads_leave_no_temporary_blobs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fake = FakeBackend::open(dir.path()).expect("open");
        let root = root(&fake).await;
        fake.upload_new(&root, "f", bytes(b"1"), meta())
            .await
            .expect("upload");
        assert!(
            fake.upload_new(&root, "f", bytes(b"2"), meta())
                .await
                .is_err()
        );
        let blobs: Vec<_> = std::fs::read_dir(dir.path().join("blobs"))
            .expect("blobs")
            .map(|e| e.expect("entry").file_name())
            .collect();
        assert_eq!(blobs.len(), 1, "{blobs:?}");
    }

    #[tokio::test]
    async fn import_copies_a_local_tree_with_modes_and_mtimes() {
        use std::os::unix::fs::PermissionsExt;
        let source = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(source.path().join("sub")).expect("mkdir");
        std::fs::write(source.path().join("sub/x"), b"xyz").expect("write");
        std::fs::set_permissions(
            source.path().join("sub/x"),
            std::fs::Permissions::from_mode(0o751),
        )
        .expect("chmod");
        let fake = FakeBackend::new();
        let root = root(&fake).await;
        fake.import_dir(source.path(), &root).await.expect("import");
        let sub = &fake.list_children(&root).await.expect("list")[0];
        assert_eq!((sub.name.as_str(), sub.kind), ("sub", NodeKind::Folder));
        let x = &fake.list_children(&sub.link_id).await.expect("list")[0];
        assert_eq!((x.size, x.mode), (Some(3), Some(0o751)));
        let mtime = std::fs::metadata(source.path().join("sub/x"))
            .and_then(|m| m.modified())
            .expect("mtime");
        assert_eq!(x.mtime, Some(mtime));
    }

    fn blob_count(dir: &std::path::Path) -> usize {
        std::fs::read_dir(dir.join("blobs")).expect("blobs").count()
    }

    #[tokio::test]
    async fn the_journal_and_compactions_reload_the_same_drive() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = {
            let fake = FakeBackend::open(dir.path()).expect("open");
            let root = root(&fake).await;
            let (file, mut rev) = fake
                .upload_new(&root, "f", bytes(b"v0"), meta())
                .await
                .expect("upload");
            for i in 1..5 {
                let data = Bytes::from(format!("v{i}"));
                rev = fake
                    .upload_revision(&file, &rev, UploadSource::Bytes(data), meta())
                    .await
                    .expect("revision");
            }
            // Only one line per change is written; nothing rewrites a whole file.
            let journal =
                std::fs::read_to_string(dir.path().join("journal.jsonl")).expect("journal");
            assert_eq!(journal.lines().count(), 5);
            file
        };
        let fake = FakeBackend::open(dir.path()).expect("reopen from the journal");
        assert_eq!(fake.content(&file).as_deref(), Some(&b"v4"[..]));
        fake.state.lock().compact().expect("compact");
        assert_eq!(
            std::fs::metadata(dir.path().join("journal.jsonl"))
                .expect("journal")
                .len(),
            0
        );
        let other = FakeBackend::open(dir.path()).expect("reopen from the snapshot");
        assert_eq!(other.content(&file).as_deref(), Some(&b"v4"[..]));
        assert_eq!(other.changes().len(), 5);
        // A process that loaded before the compaction catches up too.
        let root_link = root(&other).await;
        other
            .upload_new(&root_link, "g", bytes(b"g"), meta())
            .await
            .expect("upload");
        assert_eq!(fake.list_children(&root_link).await.expect("list").len(), 2);
    }

    #[tokio::test]
    async fn files_keep_only_their_last_revisions() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fake = FakeBackend::open(dir.path()).expect("open");
        fake.set_revision_limit(3);
        let root = root(&fake).await;
        let (file, mut rev) = fake
            .upload_new(&root, "f", bytes(b"0"), meta())
            .await
            .expect("upload");
        for i in 1..10 {
            let data = Bytes::from(i.to_string());
            rev = fake
                .upload_revision(&file, &rev, UploadSource::Bytes(data), meta())
                .await
                .expect("revision");
        }
        assert_eq!(fake.revision_count(&file), 3);
        assert_eq!(blob_count(dir.path()), 3, "old blobs are deleted");
        assert_eq!(fake.content(&file).as_deref(), Some(&b"9"[..]));
        assert_eq!(
            fake.storage_info().await.expect("info").used,
            3,
            "quota counts kept revisions"
        );
    }

    #[tokio::test]
    async fn change_numbers_survive_trimming() {
        let fake = FakeBackend::new();
        let root = root(&fake).await;
        let mut rev = fake
            .upload_new(&root, "f", bytes(b"x"), meta())
            .await
            .expect("upload")
            .1;
        let file = fake.list_children(&root).await.expect("list")[0]
            .link_id
            .clone();
        let total = KEEP_CHANGES * 3 / 2 + 10;
        for _ in 1..total {
            rev = fake
                .upload_revision(&file, &rev, bytes(b"x"), meta())
                .await
                .expect("revision");
        }
        let kept = fake.changes_since(0);
        assert!(kept.len() <= KEEP_CHANGES * 3 / 2);
        assert_eq!(kept.last().map(|(seq, _)| *seq), Some(total as u64));
        assert_eq!(fake.changes_since(total as u64 - 2).len(), 2);
        // Events are trimmed the same way; an old cursor asks for a refresh.
        assert!(
            fake.poll_events(&EventId::new("0"))
                .await
                .expect("poll")
                .refresh
        );
    }

    #[tokio::test]
    async fn a_torn_journal_line_is_skipped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = {
            let fake = FakeBackend::open(dir.path()).expect("open");
            let root = root(&fake).await;
            fake.upload_new(&root, "f", bytes(b"kept"), meta())
                .await
                .expect("upload")
                .0
        };
        let mut journal = std::fs::OpenOptions::new()
            .append(true)
            .open(dir.path().join("journal.jsonl"))
            .expect("journal");
        journal
            .write_all(b"{\"generation\": 99, \"rec")
            .expect("tear");
        drop(journal);
        let fake = FakeBackend::open(dir.path()).expect("reopen");
        assert_eq!(fake.content(&file).as_deref(), Some(&b"kept"[..]));
        let root = root(&fake).await;
        fake.upload_new(&root, "g", bytes(b"after"), meta())
            .await
            .expect("upload after the torn line");
        let again = FakeBackend::open(dir.path()).expect("reopen");
        assert_eq!(again.list_children(&root).await.expect("list").len(), 2);
    }

    fn client(uid: &str) -> FakeClient {
        FakeClient {
            uid: Some(uid.into()),
            ..FakeClient::default()
        }
    }

    async fn revision_ids(fake: &FakeBackend, file: &LinkId) -> Vec<RevisionId> {
        let revisions = fake.revisions(file).await.expect("revisions");
        revisions.into_iter().map(|r| r.id).collect()
    }

    #[tokio::test]
    async fn a_paused_revision_lets_another_client_commit_first() {
        let fake = FakeBackend::new();
        let a = fake.with_client(client("a"));
        let b = fake.with_client(client("b"));
        fake.set_commit_checks(false);
        let root = root(&fake).await;
        let (file, r1) = a
            .upload_new(&root, "f", bytes(b"v1"), meta())
            .await
            .expect("upload");
        let pause = a.pause_at(FakePoint::RevisionBeforeCommit);
        let upload = tokio::spawn({
            let (a, file, r1) = (a.clone(), file.clone(), r1.clone());
            async move {
                a.upload_revision(&file, &r1, bytes(b"from a"), meta())
                    .await
            }
        });
        pause.reached().await;
        let rb = b
            .upload_revision(&file, &r1, bytes(b"from b"), meta())
            .await
            .expect("b commits first");
        pause.release();
        let ra = upload.await.expect("task").expect("a commits last");
        assert_eq!(revision_ids(&fake, &file).await, [r1, rb, ra.clone()]);
        let revisions = fake.revisions(&file).await.expect("revisions");
        assert_eq!(
            revisions.iter().map(|r| r.active).collect::<Vec<_>>(),
            [false, false, true]
        );
        assert_eq!(revisions[1].client_uid.as_deref(), Some("b"));
        assert_eq!(revisions[2].id, ra);
        assert_eq!(fake.content(&file).as_deref(), Some(&b"from a"[..]));
    }

    #[tokio::test]
    async fn commit_checks_reject_a_stale_base_by_default() {
        let fake = FakeBackend::new();
        let a = fake.with_client(client("a"));
        let b = fake.with_client(client("b"));
        let root = root(&fake).await;
        let (file, r1) = a
            .upload_new(&root, "f", bytes(b"v1"), meta())
            .await
            .expect("upload");
        let pause = a.pause_at(FakePoint::RevisionBeforeCommit);
        let upload = tokio::spawn({
            let (a, file, r1) = (a.clone(), file.clone(), r1.clone());
            async move {
                a.upload_revision(&file, &r1, bytes(b"from a"), meta())
                    .await
            }
        });
        pause.reached().await;
        let rb = b
            .upload_revision(&file, &r1, bytes(b"from b"), meta())
            .await
            .expect("b commits first");
        pause.release();
        assert_eq!(
            upload.await.expect("task").unwrap_err(),
            BackendError::RevisionConflict { current: Some(rb) }
        );
        assert_eq!(fake.revision_count(&file), 2);
    }

    #[tokio::test]
    async fn pauses_apply_only_to_their_client() {
        let fake = FakeBackend::new();
        let a = fake.with_client(client("a"));
        let b = fake.with_client(client("b"));
        let root = root(&fake).await;
        let pause = a.pause_at(FakePoint::CreateFolderBeforeCommit);
        let other = tokio::time::timeout(
            Duration::from_secs(5),
            b.create_folder(&root, "b", SystemTime::UNIX_EPOCH),
        )
        .await
        .expect("b is not paused")
        .expect("mkdir");
        assert_eq!(fake.list_children(&root).await.expect("list").len(), 1);
        let paused = tokio::spawn({
            let (a, root) = (a.clone(), root.clone());
            async move { a.create_folder(&root, "a", SystemTime::UNIX_EPOCH).await }
        });
        pause.reached().await;
        assert!(!paused.is_finished());
        assert_eq!(
            fake.list_children(&root).await.expect("list").len(),
            1,
            "nothing is applied while paused"
        );
        pause.release();
        let created = paused.await.expect("task").expect("mkdir");
        assert_ne!(created, other);
        assert_eq!(fake.list_children(&root).await.expect("list").len(), 2);
    }

    #[tokio::test]
    async fn rename_hash_check_can_be_disabled() {
        let fake = FakeBackend::new();
        let root = root(&fake).await;
        let (file, _) = fake
            .upload_new(&root, "f", bytes(b"x"), meta())
            .await
            .expect("upload");
        let stale = NameHash::new("fake:other");
        assert_eq!(
            fake.rename(&file, None, "g", Some(&stale)).await,
            Err(BackendError::NameConflict)
        );
        fake.set_name_hash_enforced(false);
        fake.rename(&file, None, "g", Some(&stale))
            .await
            .expect("not enforced");
        let renamed = fake.get_nodes(&[file]).await.expect("get").remove(0);
        assert_eq!(renamed.expect("node").name, "g");
    }

    #[tokio::test]
    async fn moving_a_folder_into_itself_is_invalid_move() {
        let fake = FakeBackend::new();
        let root = root(&fake).await;
        let top = fake
            .create_folder(&root, "top", SystemTime::UNIX_EPOCH)
            .await
            .expect("mkdir");
        let inner = fake
            .create_folder(&top, "inner", SystemTime::UNIX_EPOCH)
            .await
            .expect("mkdir");
        assert_eq!(
            fake.rename(&top, Some(&inner), "top", None).await,
            Err(BackendError::InvalidMove)
        );
        assert_eq!(
            fake.rename(&top, Some(&top), "top", None).await,
            Err(BackendError::InvalidMove)
        );
        assert!(!BackendError::InvalidMove.is_retryable());
    }

    #[tokio::test]
    async fn official_app_revisions_carry_no_mode_and_optionally_no_sha1() {
        let fake = FakeBackend::new();
        let app = fake.with_client(FakeClient {
            uid: Some("web".into()),
            claims_sha1: false,
            claims_mode: false,
            ..FakeClient::default()
        });
        let half = fake.with_client(FakeClient {
            claims_sha1: true,
            claims_mode: false,
            ..FakeClient::default()
        });
        let root = root(&fake).await;
        let (plain, _) = fake
            .upload_new(&root, "plain", bytes(b"x"), meta())
            .await
            .expect("upload");
        let (web, _) = app
            .upload_new(&root, "web", bytes(b"y"), meta())
            .await
            .expect("upload");
        let (partial, _) = half
            .upload_new(&root, "half", bytes(b"z"), meta())
            .await
            .expect("upload");
        let node = |link: LinkId| {
            let fake = fake.clone();
            async move {
                fake.get_nodes(&[link])
                    .await
                    .expect("get")
                    .remove(0)
                    .expect("node")
            }
        };
        let plain = node(plain).await;
        assert_eq!((plain.mode, plain.sha1.is_some()), (Some(0o644), true));
        let web_node = node(web.clone()).await;
        assert_eq!((web_node.mode, web_node.sha1), (None, None));
        let partial = node(partial).await;
        assert_eq!((partial.mode, partial.sha1.is_some()), (None, true));
        let revisions = fake.revisions(&web).await.expect("revisions");
        assert_eq!(revisions[0].sha1, None);
        assert_eq!(revisions[0].client_uid.as_deref(), Some("web"));
        assert!(
            fake.all_revision_sha1s()
                .contains(&<[u8; 20]>::from(sha1::Sha1::digest(b"y"))),
            "the server still has the content hash"
        );
    }

    /// Removes the fields M3.3 added from every revision in `value`.
    fn strip_new_revision_fields(value: &mut serde_json::Value) {
        match value {
            serde_json::Value::Object(map) => {
                if map.contains_key("sha1") && map.contains_key("size") {
                    for key in ["client_uid", "sha1_claimed", "seq"] {
                        map.remove(key);
                    }
                }
                map.values_mut().for_each(strip_new_revision_fields);
            }
            serde_json::Value::Array(items) => items.iter_mut().for_each(strip_new_revision_fields),
            _ => {}
        }
    }

    /// D132: a signature marked as failing is reported on the node (each
    /// part), announced by an event, kept across a reopen, and a rename
    /// signs the new name.
    #[tokio::test]
    async fn an_unverified_signature_is_reported_until_the_name_is_signed_again() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = {
            let fake = FakeBackend::open(dir.path()).expect("open");
            let root = root(&fake).await;
            let (file, _) = fake
                .upload_new(&root, "f", bytes(b"v1"), meta())
                .await
                .expect("upload");
            let cursor = fake.latest_event_id().await.expect("cursor");
            for part in SignaturePart::ALL {
                fake.set_unverified(&file, part, true, Some("x@example.test"))
                    .expect("mark");
            }
            let page = fake.poll_events(&cursor).await.expect("events");
            assert!(
                page.events
                    .iter()
                    .any(|e| e.link_id == file && e.kind == EventKind::UpdateMetadata)
            );
            file
        };
        let fake = FakeBackend::open(dir.path()).expect("reopen");
        let root = root(&fake).await;
        let listed = fake.list_children(&root).await.expect("list");
        let found = listed[0].unverified.clone().expect("unverified");
        assert_eq!(found.parts(), SignaturePart::ALL);
        assert_eq!(found.signer, "x@example.test");
        fake.rename(&file, None, "g", None).await.expect("rename");
        let node = fake
            .get_nodes(std::slice::from_ref(&file))
            .await
            .expect("get")
            .remove(0);
        let found = node.expect("node").unverified.expect("still unverified");
        assert_eq!(
            found.parts(),
            [SignaturePart::Key, SignaturePart::ContentKey]
        );
        for part in [SignaturePart::Key, SignaturePart::ContentKey] {
            fake.set_unverified(&file, part, false, None)
                .expect("clear");
        }
        let node = fake.get_nodes(&[file]).await.expect("get").remove(0);
        assert_eq!(node.expect("node").unverified, None);
    }

    #[tokio::test]
    async fn persisted_revisions_without_new_fields_still_load() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = {
            let fake = FakeBackend::open(dir.path()).expect("open");
            let root = root(&fake).await;
            let (file, r1) = fake
                .upload_new(&root, "f", bytes(b"v1"), meta())
                .await
                .expect("upload");
            fake.upload_revision(&file, &r1, bytes(b"v2"), meta())
                .await
                .expect("revision");
            file
        };
        let path = dir.path().join("journal.jsonl");
        let journal = std::fs::read_to_string(&path).expect("journal");
        assert!(
            journal.contains("sha1_claimed"),
            "the new format is written"
        );
        let old: Vec<String> = journal
            .lines()
            .map(|line| {
                let mut value: serde_json::Value = serde_json::from_str(line).expect("line");
                strip_new_revision_fields(&mut value);
                value.to_string()
            })
            .collect();
        assert!(!old.join("\n").contains("sha1_claimed"));
        std::fs::write(&path, old.join("\n") + "\n").expect("rewrite");
        let fake = FakeBackend::open(dir.path()).expect("reopen");
        assert_eq!(fake.content(&file).as_deref(), Some(&b"v2"[..]));
        let revisions = fake.revisions(&file).await.expect("revisions");
        assert_eq!(revisions.len(), 2);
        assert!(
            revisions.iter().all(|r| r.sha1.is_some()),
            "claimed by default"
        );
        assert!(revisions.iter().all(|r| r.client_uid.is_none()));
        let node = fake.get_nodes(&[file]).await.expect("get").remove(0);
        assert_eq!(node.expect("node").mode, Some(0o644));
    }

    #[tokio::test]
    async fn fault_rules_apply_per_client_and_expire() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fake = FakeBackend::open(dir.path()).expect("open");
        let a = fake.with_client(client("a"));
        let b = fake.with_client(client("b"));
        let now = unix_ms();
        add_fault(
            dir.path(),
            FaultRule {
                kind: FaultKind::Offline,
                until_ms: None,
                client: Some("a".into()),
            },
        )
        .expect("rule");
        assert_eq!(
            a.root().await.unwrap_err(),
            BackendError::Offline("injected".into())
        );
        b.root().await.expect("b is not affected");
        fake.root()
            .await
            .expect("a client without a uid is not affected");
        add_fault(
            dir.path(),
            FaultRule {
                kind: FaultKind::Unauthorized,
                until_ms: Some(now + 300),
                client: Some("b".into()),
            },
        )
        .expect("rule");
        assert_eq!(b.root().await.unwrap_err(), BackendError::Unauthorized);
        tokio::time::sleep(Duration::from_millis(400)).await;
        b.root().await.expect("the rule expired");
        assert!(a.storage_info().await.is_err(), "every call is covered");
        clear_faults(dir.path(), Some("a")).expect("clear");
        a.root().await.expect("cleared");
    }

    #[tokio::test]
    async fn a_changed_fault_file_is_picked_up() {
        let dir = tempfile::tempdir().expect("tempdir");
        let daemon = FakeBackend::open(dir.path()).expect("open");
        daemon.root().await.expect("no file, no faults");
        let rule = |kind| FaultRule {
            kind,
            until_ms: None,
            client: None,
        };
        add_fault(dir.path(), rule(FaultKind::Offline)).expect("rule");
        assert!(matches!(daemon.root().await, Err(BackendError::Offline(_))));
        clear_faults(dir.path(), None).expect("clear");
        daemon.root().await.expect("cleared");
        add_fault(dir.path(), rule(FaultKind::Unauthorized)).expect("rule");
        assert_eq!(daemon.root().await.unwrap_err(), BackendError::Unauthorized);
        clear_faults(dir.path(), None).expect("clear");
        // Broken nodes: only node fetches fail, with an error that is not
        // transient.
        add_fault(dir.path(), rule(FaultKind::BrokenNodes)).expect("rule");
        let root = daemon.root().await.expect("other calls go through").link_id;
        let error = daemon.get_nodes(&[root]).await.unwrap_err();
        assert!(matches!(error, BackendError::Other(_)) && error.is_permanent());
        std::fs::remove_file(dir.path().join("faults.json")).expect("remove");
        daemon.root().await.expect("a missing file is no faults");
        std::fs::write(dir.path().join("faults.json"), b"not json").expect("write");
        daemon.root().await.expect("a malformed file is ignored");
    }

    fn rtt(ms: u64) -> NetProfile {
        NetProfile {
            rtt: Duration::from_millis(ms),
            ..NetProfile::default()
        }
    }

    #[tokio::test]
    async fn round_trips_cost_time_but_overlap() {
        let fake = FakeBackend::new();
        let root = root(&fake).await;
        fake.set_profile(rtt(50));
        let start = std::time::Instant::now();
        fake.list_children(&root).await.expect("list");
        assert!(start.elapsed() >= Duration::from_millis(50));
        let start = std::time::Instant::now();
        let (a, b) = tokio::join!(fake.list_children(&root), fake.list_children(&root));
        a.and(b).expect("lists");
        let both = start.elapsed();
        assert!(
            both >= Duration::from_millis(50) && both < Duration::from_millis(100),
            "{both:?}"
        );
        // An upload is three round trips; a long listing one per 150 children.
        let start = std::time::Instant::now();
        fake.upload_new(&root, "f", bytes(b"x"), meta())
            .await
            .expect("upload");
        assert!(start.elapsed() >= Duration::from_millis(150));
        fake.set_profile(NetProfile::default());
        for i in 0..300 {
            fake.create_folder(&root, &format!("d{i}"), SystemTime::UNIX_EPOCH)
                .await
                .expect("mkdir");
        }
        fake.set_profile(rtt(50));
        let start = std::time::Instant::now();
        assert_eq!(fake.list_children(&root).await.expect("list").len(), 301);
        assert!(start.elapsed() >= Duration::from_millis(150), "3 pages");
    }

    /// The layout costs nothing (the claimed sizes); a read lists its
    /// block's page, a round trip, then downloads it beside the listing of
    /// the other pages for the manifest; a later read in a listed page
    /// costs its download only (D135).
    #[tokio::test]
    async fn a_read_lists_its_block_page_and_the_rest_beside_it() {
        let fake = FakeBackend::new();
        fake.set_block_size(1000);
        let root = root(&fake).await;
        let large = Bytes::from(vec![1u8; 25 * 1000]);
        let (file, rev) = fake
            .upload_new(&root, "large", UploadSource::Bytes(large), meta())
            .await
            .expect("upload");
        fake.set_profile(rtt(40));
        let timed = |index| {
            let (fake, file, rev) = (&fake, &file, &rev);
            async move {
                let start = std::time::Instant::now();
                let mut buf = vec![0; 1000];
                fake.fetch_block_into(file, rev, index, &mut buf)
                    .await
                    .expect("block");
                start.elapsed()
            }
        };
        let start = std::time::Instant::now();
        assert_eq!(fake.blocks(&file, &rev).await.expect("blocks").len(), 25);
        assert!(start.elapsed() < Duration::from_millis(40), "no listing");
        let first = timed(25).await;
        assert!(
            first >= Duration::from_millis(80) && first < Duration::from_millis(120),
            "its page, then the download beside page 1: {first:?}"
        );
        let later = timed(3).await;
        assert!(
            later >= Duration::from_millis(40) && later < Duration::from_millis(80),
            "the download alone: {later:?}"
        );
    }

    #[tokio::test]
    async fn transfers_share_one_pipe_per_direction() {
        let fake = FakeBackend::new();
        fake.set_block_size(1_000_000);
        let root = root(&fake).await;
        let data = Bytes::from(vec![7u8; 2_000_000]);
        let (file, rev) = fake
            .upload_new(&root, "f", UploadSource::Bytes(data), meta())
            .await
            .expect("upload");
        fake.set_profile(NetProfile {
            down_bytes_per_sec: Some(10_000_000),
            ..NetProfile::default()
        });
        let start = std::time::Instant::now();
        let mut buf = vec![0u8; 1_000_000];
        let (first, second) = tokio::join!(
            fake.fetch_block(&file, &rev, 1),
            fake.fetch_block_into(&file, &rev, 2, &mut buf)
        );
        assert_eq!(first.expect("block 1").len(), 1_000_000);
        assert_eq!(second, Ok(1_000_000));
        let both = start.elapsed();
        assert!(
            both >= Duration::from_millis(195) && both < Duration::from_millis(300),
            "two 100 ms transfers one after the other: {both:?}"
        );
        // Uploads use the other direction: unlimited here.
        let start = std::time::Instant::now();
        fake.upload_revision(&file, &rev, bytes(b"y"), meta())
            .await
            .expect("revision");
        assert!(start.elapsed() < Duration::from_millis(50));
    }

    #[tokio::test]
    async fn a_daemon_picks_up_a_changed_profile_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_profile(dir.path(), rtt(40)).expect("profile");
        let daemon = FakeBackend::open(dir.path()).expect("open");
        let timed = || async {
            let start = std::time::Instant::now();
            daemon.root().await.expect("root");
            start.elapsed()
        };
        assert!(timed().await >= Duration::from_millis(40), "read at open");
        clear_profile(dir.path()).expect("clear");
        assert!(timed().await < Duration::from_millis(40), "cleared");
        let profile = NetProfile {
            rtt: Duration::from_millis(60),
            up_bytes_per_sec: Some(20_000_000),
            down_bytes_per_sec: None,
        };
        write_profile(dir.path(), profile).expect("profile");
        assert_eq!(read_profile(dir.path()), profile);
        assert!(timed().await >= Duration::from_millis(60), "changed");
        std::fs::write(dir.path().join("profile.json"), b"not json").expect("write");
        assert!(
            timed().await < Duration::from_millis(40),
            "malformed: no cost"
        );
    }

    #[tokio::test]
    async fn faults_fire_before_the_round_trip() {
        let fake = FakeBackend::new();
        fake.set_profile(rtt(200));
        fake.inject_failure(FakeOp::Root, BackendError::Transient("503".into()));
        let start = std::time::Instant::now();
        assert!(fake.root().await.is_err());
        assert!(start.elapsed() < Duration::from_millis(200));
    }

    /// The child index and the used total equal a full scan.
    fn assert_index_consistent(fake: &FakeBackend) {
        let state = fake.state.lock();
        let mut scanned: HashMap<LinkId, BTreeMap<String, LinkId>> = HashMap::new();
        for node in state.nodes.values().filter(|n| !n.trashed) {
            if let Some(parent) = &node.parent {
                let previous = scanned
                    .entry(parent.clone())
                    .or_default()
                    .insert(node.name.clone(), node.link.clone());
                assert!(previous.is_none(), "duplicate name {}", node.name);
            }
        }
        assert_eq!(state.children, scanned);
        let used: u64 = state
            .nodes
            .values()
            .flat_map(|n| &n.revisions)
            .map(|r| r.size)
            .sum();
        assert_eq!(state.used, used);
    }

    #[tokio::test]
    async fn the_child_index_follows_random_changes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fake = FakeBackend::open(dir.path()).expect("open");
        // A second process on the same drive applies the first one's journal.
        let other = FakeBackend::open(dir.path()).expect("open");
        fake.set_revision_limit(2);
        let root = root(&fake).await;
        let mut folders = vec![root.clone()];
        let mut files: Vec<(LinkId, RevisionId)> = Vec::new();
        let mut seed = 0x2545_f491_4f6c_dd1d_u64;
        let mut next = |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n as u64) as usize
        };
        for step in 0..400 {
            let handle = if next(3) == 0 { &other } else { &fake };
            let parent = folders[next(folders.len())].clone();
            let name = format!("n{}", next(12));
            // Failures (a taken name, a trashed parent, a cycle) are fine.
            match next(6) {
                0 => {
                    if let Ok(link) = handle
                        .create_folder(&parent, &name, SystemTime::UNIX_EPOCH)
                        .await
                    {
                        folders.push(link);
                    }
                }
                1 => {
                    let data = Bytes::from(vec![0u8; next(100)]);
                    if let Ok(file) = handle
                        .upload_new(&parent, &name, UploadSource::Bytes(data), meta())
                        .await
                    {
                        files.push(file);
                    }
                }
                2 if !files.is_empty() => {
                    let i = next(files.len());
                    let data = Bytes::from(vec![1u8; next(100)]);
                    let (file, base) = files[i].clone();
                    if let Ok(revision) = handle
                        .upload_revision(&file, &base, UploadSource::Bytes(data), meta())
                        .await
                    {
                        files[i].1 = revision;
                    }
                }
                3 => {
                    let node = if next(2) == 0 && !files.is_empty() {
                        files[next(files.len())].0.clone()
                    } else {
                        folders[next(folders.len())].clone()
                    };
                    let _ = handle.rename(&node, Some(&parent), &name, None).await;
                }
                4 if step % 3 == 0 => {
                    let node = if next(2) == 0 && !files.is_empty() {
                        files[next(files.len())].0.clone()
                    } else {
                        folders[next(folders.len())].clone()
                    };
                    let _ = handle.trash(&[node]).await;
                }
                _ => {
                    let _ = handle.list_children(&parent).await;
                }
            }
            if step % 50 == 0 {
                fake.state.lock().compact().expect("compact");
            }
            // Both handles catch up at their next call.
            fake.storage_info().await.expect("info");
            other.storage_info().await.expect("info");
            assert_index_consistent(&fake);
            assert_index_consistent(&other);
        }
        assert!(
            folders.len() > 20 && files.len() > 20,
            "the steps did something"
        );
        assert_index_consistent(&FakeBackend::open(dir.path()).expect("reopen"));
    }

    /// Volumes and shares (M5.10): each tree's handle sees its own root and
    /// its own volume's events; roles decide what a member may change.
    #[tokio::test]
    async fn trees_on_other_volumes_have_their_own_events_and_roles() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fake = FakeBackend::open(dir.path()).expect("open");
        let main_root = root(&fake).await;
        let added = fake
            .add_root("Team", Some("owner@example.test"), Some(FakeRole::Editor))
            .expect("add root");
        assert_ne!(added.volume, MAIN_VOLUME);
        let share = added.share.clone().expect("shared");
        let tree = fake.tree(&added.volume, added.root.clone());
        assert_eq!(tree.account().await.expect("account").root, added.root);
        assert_eq!(tree.root().await.expect("root").name, "Team");
        // A device-like root on the drive's own volume.
        let device = fake.add_root("Laptop", None, None).expect("device root");
        assert_eq!(device.volume, MAIN_VOLUME);
        assert_eq!(device.share, None);

        let main_since = fake.latest_event_id().await.expect("cursor");
        let tree_since = tree.latest_event_id().await.expect("cursor");
        let folder = tree
            .create_folder(&added.root, "docs", SystemTime::UNIX_EPOCH)
            .await
            .expect("an editor creates a folder");
        let (file, _) = fake
            .upload_new(&main_root, "mine", bytes(b"x"), meta())
            .await
            .expect("upload");
        let main_page = fake.poll_events(&main_since).await.expect("poll");
        let tree_page = tree.poll_events(&tree_since).await.expect("poll");
        let links = |page: &EventPage| -> Vec<LinkId> {
            page.events.iter().map(|e| e.link_id.clone()).collect()
        };
        assert_eq!(links(&main_page), std::slice::from_ref(&file));
        assert_eq!(links(&tree_page), std::slice::from_ref(&folder));
        assert!(!main_page.more && !tree_page.more);
        // Both cursors reach the end of the log, past the other's events.
        assert_eq!(main_page.next, tree_page.next);

        // A move between volumes is refused.
        assert_eq!(
            fake.rename(&file, Some(&folder), "mine", None).await,
            Err(BackendError::InvalidMove)
        );
        // A viewer reads but cannot change.
        fake.set_member_role(&share, "fake@example.test", Some(FakeRole::Viewer))
            .expect("role");
        assert_eq!(
            tree.create_folder(&added.root, "more", SystemTime::UNIX_EPOCH)
                .await,
            Err(BackendError::PermissionDenied)
        );
        assert_eq!(
            tree.list_children(&added.root).await.expect("list").len(),
            1
        );
        // A volume's root cannot be trashed.
        assert_eq!(
            tree.trash(std::slice::from_ref(&added.root))
                .await
                .expect("trash")[0],
            Err(BackendError::PermissionDenied)
        );
        // The owner may do anything on their volume.
        let owner = tree.with_client(FakeClient {
            user: Some("owner@example.test".into()),
            ..(*tree.client).clone()
        });
        owner
            .create_folder(&added.root, "more", SystemTime::UNIX_EPOCH)
            .await
            .expect("the owner writes");

        // Volumes, shares and roles survive reopening.
        let reopened = FakeBackend::open(dir.path()).expect("reopen");
        let tree = reopened.tree(&added.volume, added.root.clone());
        assert_eq!(
            tree.create_folder(&added.root, "again", SystemTime::UNIX_EPOCH)
                .await,
            Err(BackendError::PermissionDenied)
        );
        // Without the membership the share is gone for the member.
        reopened
            .set_member_role(&share, "fake@example.test", None)
            .expect("remove");
        assert_eq!(
            tree.list_children(&added.root).await,
            Err(BackendError::NotFound)
        );
        assert_eq!(tree.get_nodes(&[folder]).await.expect("get"), [None]);
        assert_eq!(tree.account().await, Err(BackendError::NotFound));
    }

    #[tokio::test]
    async fn a_refresh_is_asked_per_volume() {
        let fake = FakeBackend::new();
        let added = fake
            .add_root("Team", Some("owner@example.test"), Some(FakeRole::Admin))
            .expect("add root");
        let tree = fake.tree(&added.volume, added.root.clone());
        let since = fake.latest_event_id().await.expect("cursor");
        fake.request_volume_refresh(&added.volume);
        assert!(!fake.poll_events(&since).await.expect("poll").refresh);
        assert!(tree.poll_events(&since).await.expect("poll").refresh);
        assert!(!tree.poll_events(&since).await.expect("poll").refresh);
    }

    /// M5.12: the listing holds the shares of other users with the drive's
    /// user as a member, oldest first, files and folders; a revoked share
    /// leaves it; the core stream signals only when told to, and keeps its
    /// position in a persisted fake.
    #[tokio::test]
    async fn shared_with_me_lists_the_members_shares_and_the_core_stream_signals() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fake = FakeBackend::open(dir.path()).expect("open");
        fake.add_root("Laptop", None, None).expect("own root");
        let team = fake
            .add_root("Team", Some("alice@example.test"), Some(FakeRole::Viewer))
            .expect("share");
        fake.add_root("Private", Some("alice@example.test"), None)
            .expect("not shared");
        let file = fake
            .add_shared_file("plan.txt", b"plan", "bob@example.test", FakeRole::Editor)
            .await
            .expect("file share");
        let listing = fake.shared_with_me().await.expect("listing");
        let names: Vec<(&str, NodeKind, &str, ShareRole)> = listing
            .iter()
            .map(|e| (e.name.as_str(), e.kind, e.owner.as_str(), e.role))
            .collect();
        assert_eq!(
            names,
            [
                (
                    "Team",
                    NodeKind::Folder,
                    "alice@example.test",
                    ShareRole::Viewer
                ),
                (
                    "plan.txt",
                    NodeKind::File,
                    "bob@example.test",
                    ShareRole::Editor
                ),
            ]
        );
        assert_eq!(listing[0].share, team.share.clone().expect("id"));
        assert_eq!(listing[1].root, file.root);
        assert_eq!(listing[1].volume, file.volume);
        assert!(listing[0].created < listing[1].created);
        // The owner sees none of them as shared with them.
        let alice = fake.with_client(FakeClient {
            user: Some("alice@example.test".into()),
            ..FakeClient::default()
        });
        assert!(
            alice
                .shared_with_me()
                .await
                .expect("listing")
                .iter()
                .all(|e| e.owner != "alice@example.test")
        );

        let since = fake.latest_core_event_id().await.expect("core id");
        let page = fake.poll_core_events(&since).await.expect("core");
        assert!(!page.shares_changed && !page.more && page.next == since);
        // A revocation changes the listing, not the core stream ...
        fake.set_own_role(team.share.as_deref().expect("id"), None)
            .expect("revoke");
        assert_eq!(fake.shared_with_me().await.expect("listing").len(), 1);
        assert!(
            !fake
                .poll_core_events(&since)
                .await
                .expect("core")
                .shares_changed
        );
        // ... until the signal, which a reopened fake keeps.
        fake.core_signal().expect("signal");
        let reopened = FakeBackend::open(dir.path()).expect("reopen");
        let page = reopened.poll_core_events(&since).await.expect("core");
        assert!(page.shares_changed);
        assert!(
            !reopened
                .poll_core_events(&page.next)
                .await
                .expect("core")
                .shares_changed
        );
        assert!(
            reopened
                .poll_core_events(&EventId::new("core-99"))
                .await
                .is_err()
        );
    }
}
