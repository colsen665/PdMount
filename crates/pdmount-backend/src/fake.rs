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
//! the fake's directory makes calls fail as if offline or signed out.
//!
//! Growth is bounded for long runs (fsx, the scenario matrix): files keep
//! their last revisions (`PDMOUNT_FAKE_KEEP_REVISIONS`, default 16), the
//! most recent trashed nodes are kept, and the event and change logs are
//! trimmed, keeping absolute sequence numbers. Injected failures and
//! latency stay per process.

use crate::{
    AccountInfo, Backend, BackendError, BlockInfo, EventId, EventKind, EventPage, FileMeta, LinkId,
    NameHash, NodeKind, RemoteEvent, RemoteNode, Result, RevisionId, RevisionMeta, StorageInfo,
    UploadSource,
};
use bytes::Bytes;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sha1::Digest;
use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
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
}

/// Who is calling: handles made by [`FakeBackend::with_client`] differ in
/// what their uploads record, like the daemon and the official apps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeClient {
    /// The SDK client UID (`pdmount-<device id>`); `None` for tests that
    /// do not care.
    pub uid: Option<String>,
    /// Uploads carry a SHA-1 that reads report.
    pub claims_sha1: bool,
    /// Uploads carry `pdmount.mode` (the official apps leave it out).
    pub claims_mode: bool,
}

impl Default for FakeClient {
    fn default() -> Self {
        Self {
            uid: None,
            claims_sha1: true,
            claims_mode: true,
        }
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
        }
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
    BackendError::Other(format!("fake backend: {context}: {error}"))
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
}

/// One journaled delta.
#[derive(Debug, Clone, Serialize, Deserialize)]
enum Record {
    Node(Node),
    Removed(LinkId),
    Event(RemoteEvent),
    Change(FakeChange),
    Meta(Meta),
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
}

impl Delta {
    fn is_empty(&self) -> bool {
        self.nodes.is_empty() && self.events == 0 && self.changes == 0 && !self.meta
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
    events: Vec<RemoteEvent>,
    changes: Vec<FakeChange>,
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
    #[serde(skip, default = "default_page_size")]
    event_page_size: usize,
    #[serde(skip)]
    refresh_pending: bool,
    #[serde(skip)]
    faults: VecDeque<(FakeOp, BackendError)>,
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
            events_base: 0,
            changes_base: 0,
            next_id: 0,
            quota_max: 5 << 30,
            block_size: FakeBackend::DEFAULT_BLOCK_SIZE,
            keep_revisions: DEFAULT_KEEP_REVISIONS,
            event_page_size: default_page_size(),
            refresh_pending: false,
            faults: VecDeque::new(),
            latency: Duration::ZERO,
            pauses: Vec::new(),
            commit_checks_revision: true,
            name_hash_enforced: true,
            fault_cache: None,
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

    /// Fails like the rules of `faults.json` say, for the calling client.
    fn check_fault_file(&mut self, client: Option<&str>) -> Result<()> {
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
                && rule.client.as_deref().is_none_or(|uid| Some(uid) == client);
            if applies {
                return Err(match rule.kind {
                    FaultKind::Offline => BackendError::Offline("injected".into()),
                    FaultKind::Unauthorized => BackendError::Unauthorized,
                });
            }
        }
        Ok(())
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
        self.nodes
            .values()
            .find(|n| !n.trashed && n.parent.as_ref() == Some(parent) && n.name == name)
    }

    fn ensure_name_free(&self, parent: &LinkId, name: &str, except: Option<&LinkId>) -> Result<()> {
        match self.child_named(parent, name) {
            Some(existing) if Some(&existing.link) != except => Err(BackendError::AlreadyExists {
                existing: Some(existing.link.clone()),
            }),
            _ => Ok(()),
        }
    }

    fn used(&self) -> u64 {
        self.nodes
            .values()
            .flat_map(|n| &n.revisions)
            .map(|r| r.size)
            .sum()
    }

    fn ensure_quota(&self, extra: u64) -> Result<()> {
        if self.used() + extra > self.quota_max {
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
        let node = &self.nodes[link];
        self.events.push(RemoteEvent {
            kind,
            link_id: link.clone(),
            parent_link_id: node.parent.clone(),
            trashed: node.trashed,
        });
        self.delta.events += 1;
    }

    fn push_change(&mut self, change: FakeChange) {
        self.changes.push(change);
        self.delta.changes += 1;
    }

    fn latest_event(&self) -> u64 {
        self.events_base + self.events.len() as u64
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

    /// `len` bytes of a revision from `start`.
    fn read_range(&self, revision: &Revision, start: u64, len: u64) -> Result<Bytes> {
        if let Some(content) = &revision.content {
            let end = (start + len).min(content.len() as u64);
            return Ok(content.slice(start as usize..end as usize));
        }
        let path = self
            .blob(&revision.id)
            .ok_or_else(|| BackendError::Other(format!("no content for {}", revision.id)))?;
        let mut file = std::fs::File::open(&path).map_err(|e| io_error("open blob", e))?;
        file.seek(SeekFrom::Start(start))
            .map_err(|e| io_error("seek blob", e))?;
        let mut out = Vec::with_capacity(len as usize);
        file.take(len)
            .read_to_end(&mut out)
            .map_err(|e| io_error("read blob", e))?;
        Ok(Bytes::from(out))
    }

    /// [`State::read_range`] into `buf`, without allocating.
    fn read_range_into(
        &self,
        revision: &Revision,
        start: u64,
        len: u64,
        buf: &mut [u8],
    ) -> Result<usize> {
        let capacity = buf.len();
        let target = buf.get_mut(..len as usize).ok_or_else(|| {
            BackendError::Integrity(format!(
                "block has {len} bytes, more than the {capacity} expected"
            ))
        })?;
        if let Some(content) = &revision.content {
            let end = (start + len).min(content.len() as u64);
            let source = &content[start as usize..end as usize];
            target[..source.len()].copy_from_slice(source);
            return Ok(source.len());
        }
        let path = self
            .blob(&revision.id)
            .ok_or_else(|| BackendError::Other(format!("no content for {}", revision.id)))?;
        let file = std::fs::File::open(&path).map_err(|e| io_error("open blob", e))?;
        std::os::unix::fs::FileExt::read_exact_at(&file, target, start)
            .map_err(|e| io_error("read blob", e))?;
        Ok(target.len())
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
        }
    }

    /// Bounds growth after an operation. Returns the revisions whose blobs
    /// are no longer referenced.
    fn retain(&mut self) -> Vec<RevisionId> {
        let mut dropped = Vec::new();
        let keep = self.keep_revisions.max(1);
        for link in self.delta.nodes.clone() {
            if let Some(node) = self.nodes.get_mut(&link)
                && node.revisions.len() > keep
            {
                let excess = node.revisions.len() - keep;
                dropped.extend(node.revisions.drain(..excess).map(|r| r.id));
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
                    if let Some(node) = self.nodes.remove(link) {
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
            Record::Node(node) => {
                self.nodes.insert(node.link.clone(), node);
            }
            Record::Removed(link) => {
                self.nodes.remove(&link);
            }
            Record::Event(event) => self.events.push(event),
            Record::Change(change) => self.changes.push(change),
            Record::Meta(meta) => {
                self.next_id = meta.next_id;
                self.quota_max = meta.quota_max;
                self.block_size = meta.block_size;
                self.keep_revisions = meta.keep_revisions;
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
            events_base,
            changes_base,
            next_id,
            quota_max,
            block_size,
            keep_revisions,
            ..
        } = loaded;
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
    match std::fs::read(dir.join("journal.jsonl")) {
        Ok(journal) => state.journal_offset = state.replay(&journal, 0),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(io_error("read journal", error)),
    }
    Ok(state)
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
        }
    }

    /// A handle on the same drive that acts as another client.
    pub fn with_client(&self, client: FakeClient) -> Self {
        Self {
            state: Arc::clone(&self.state),
            client: Arc::new(client),
        }
    }

    /// Opens the fake persisted in `dir`, creating an empty drive there if
    /// needed. `PDMOUNT_FAKE_KEEP_REVISIONS` sets how many revisions files
    /// keep.
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
        Ok(Self {
            state: Arc::new(Mutex::new(state)),
            client: Arc::default(),
        })
    }

    /// Makes the next call of `op` fail with `error` (one-shot, FIFO).
    pub fn inject_failure(&self, op: FakeOp, error: BackendError) {
        self.state.lock().faults.push_back((op, error));
    }

    /// Makes the next event poll answer `refresh = true`.
    pub fn request_refresh(&self) {
        self.state.lock().refresh_pending = true;
    }

    pub fn set_quota(&self, max: u64) {
        let _ = self.with_state(None, |s| {
            s.quota_max = max;
            s.delta.meta = true;
            Ok(())
        });
    }

    pub fn set_block_size(&self, block_size: usize) {
        assert!(block_size > 0, "block size must be positive");
        let _ = self.with_state(None, |s| {
            s.block_size = block_size;
            s.delta.meta = true;
            Ok(())
        });
    }

    /// How many revisions each file keeps (older ones are deleted).
    pub fn set_revision_limit(&self, keep: usize) {
        assert!(keep > 0, "files keep at least one revision");
        let _ = self.with_state(None, |s| {
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
        self.with_state(None, |s| {
            Ok(s.nodes
                .values()
                .flat_map(|n| &n.revisions)
                .map(|r| r.sha1)
                .collect())
        })
        .unwrap_or_default()
    }

    /// Delays every read call (root, listings, blocks) by `latency`.
    pub fn set_latency(&self, latency: Duration) {
        self.state.lock().latency = latency;
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
        self.with_state(None, |s| {
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
        self.with_state(None, |s| {
            Ok(s.nodes.get(file).map_or(0, |n| n.revisions.len()))
        })
        .unwrap_or(0)
    }

    /// The changes still kept, in order.
    pub fn changes(&self) -> Vec<FakeChange> {
        self.with_state(None, |s| Ok(s.changes.clone()))
            .unwrap_or_default()
    }

    /// Kept changes with sequence numbers above `after`. Numbers start at 1
    /// and survive trimming, so callers can remember where they were.
    pub fn changes_since(&self, after: u64) -> Vec<(u64, FakeChange)> {
        self.with_state(None, |s| {
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
    /// With `op`, an injected failure for it fires first.
    fn with_state<T>(
        &self,
        op: Option<FakeOp>,
        f: impl FnOnce(&mut State) -> Result<T>,
    ) -> Result<T> {
        let mut state = self.state.lock();
        let _lock = state.dir.as_deref().map(lock_dir).transpose()?;
        state.refresh()?;
        if let Some(op) = op {
            state.check_fault_file(self.client.uid.as_deref())?;
            state.take_fault(op)?;
        }
        let result = f(&mut state);
        state.finish()?;
        result
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

#[async_trait::async_trait]
impl Backend for FakeBackend {
    fn upload_block_size(&self) -> u64 {
        self.state.lock().block_size as u64
    }

    async fn account(&self) -> Result<AccountInfo> {
        self.with_state(Some(FakeOp::Account), |s| {
            Ok(AccountInfo {
                username: s.username.clone(),
                root: s.root.clone(),
            })
        })
    }

    async fn root(&self) -> Result<RemoteNode> {
        self.delay().await;
        self.with_state(Some(FakeOp::Root), |s| Ok(s.nodes[&s.root].to_remote()))
    }

    async fn list_children(&self, folder: &LinkId) -> Result<Vec<RemoteNode>> {
        self.delay().await;
        self.checkpoint(FakePoint::ListBeforeRead).await;
        self.with_state(Some(FakeOp::ListChildren), |s| {
            s.folder(folder)?;
            let mut children: Vec<_> = s
                .nodes
                .values()
                .filter(|n| !n.trashed && n.parent.as_ref() == Some(folder))
                .map(Node::to_remote)
                .collect();
            children.sort_by(|a, b| a.name.cmp(&b.name));
            Ok(children)
        })
    }

    async fn get_nodes(&self, ids: &[LinkId]) -> Result<Vec<Option<RemoteNode>>> {
        self.delay().await;
        self.with_state(Some(FakeOp::GetNodes), |s| {
            Ok(ids
                .iter()
                .map(|id| s.nodes.get(id).map(Node::to_remote))
                .collect())
        })
    }

    async fn blocks(&self, file: &LinkId, revision: &RevisionId) -> Result<Vec<BlockInfo>> {
        self.delay().await;
        self.with_state(Some(FakeOp::Blocks), |s| {
            let node = s.nodes.get(file).ok_or(BackendError::NotFound)?;
            let rev = node.revision(revision)?;
            let block = s.block_size as u64;
            Ok((0..rev.size.div_ceil(block))
                .map(|i| BlockInfo {
                    index: i as u32 + 1,
                    plain_size: Some((rev.size - i * block).min(block) as u32),
                })
                .collect())
        })
    }

    async fn fetch_block(&self, file: &LinkId, revision: &RevisionId, index: u32) -> Result<Bytes> {
        self.delay().await;
        self.with_state(Some(FakeOp::FetchBlock), |s| {
            let node = s.nodes.get(file).ok_or(BackendError::NotFound)?;
            let rev = node.revision(revision)?;
            let block = s.block_size as u64;
            let start = u64::from(index)
                .checked_sub(1)
                .map(|i| i * block)
                .filter(|&start| start < rev.size)
                .ok_or(BackendError::NotFound)?;
            s.read_range(rev, start, block.min(rev.size - start))
        })
    }

    async fn fetch_block_into(
        &self,
        file: &LinkId,
        revision: &RevisionId,
        index: u32,
        buf: &mut [u8],
    ) -> Result<usize> {
        self.delay().await;
        self.with_state(Some(FakeOp::FetchBlock), |s| {
            let node = s.nodes.get(file).ok_or(BackendError::NotFound)?;
            let rev = node.revision(revision)?;
            let block = s.block_size as u64;
            let start = u64::from(index)
                .checked_sub(1)
                .map(|i| i * block)
                .filter(|&start| start < rev.size)
                .ok_or(BackendError::NotFound)?;
            s.read_range_into(rev, start, block.min(rev.size - start), buf)
        })
    }

    async fn upload_new(
        &self,
        parent: &LinkId,
        name: &str,
        source: UploadSource,
        meta: FileMeta,
    ) -> Result<(LinkId, RevisionId)> {
        let staged = self.stage(source).await?;
        // Draft: the server checks the name when the draft is created ...
        self.with_state(Some(FakeOp::UploadNew), |s| {
            s.folder(parent)?;
            s.ensure_name_free(parent, name, None)
        })?;
        self.checkpoint(FakePoint::NewBeforeCommit).await;
        // ... and again when the file is committed.
        self.with_state(None, |s| {
            s.folder(parent)?;
            s.ensure_name_free(parent, name, None)?;
            s.ensure_quota(staged.size)?;
            let link = LinkId::new(s.new_id("link-"));
            let (revision, rev) = s.new_revision(staged, &meta, &self.client)?;
            s.nodes.insert(
                link.clone(),
                Node {
                    link: link.clone(),
                    parent: Some(parent.clone()),
                    name: name.to_owned(),
                    kind: NodeKind::File,
                    trashed: false,
                    folder_mtime: None,
                    revisions: vec![rev],
                    trashed_at: 0,
                },
            );
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
        let staged = self.stage(source).await?;
        // Draft: the base must be the active revision.
        self.with_state(Some(FakeOp::UploadRevision), |s| {
            let node = s.node(file)?;
            if node.kind != NodeKind::File {
                return Err(BackendError::Other(format!("{file} is not a file")));
            }
            let current = node.active().map(|r| r.id.clone());
            if current.as_ref() != Some(base) {
                return Err(BackendError::RevisionConflict { current });
            }
            Ok(())
        })?;
        self.checkpoint(FakePoint::RevisionBeforeCommit).await;
        self.with_state(None, |s| {
            let current = s.node(file)?.active().map(|r| r.id.clone());
            if s.commit_checks_revision && current.as_ref() != Some(base) {
                return Err(BackendError::RevisionConflict { current });
            }
            s.ensure_quota(staged.size)?;
            let (revision, rev) = s.new_revision(staged, &meta, &self.client)?;
            if let Some(node) = s.nodes.get_mut(file) {
                node.revisions.push(rev);
            }
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
        self.with_state(Some(FakeOp::CreateFolder), |s| {
            s.folder(parent)?;
            s.ensure_name_free(parent, name, None)?;
            let link = LinkId::new(s.new_id("link-"));
            s.nodes.insert(
                link.clone(),
                Node {
                    link: link.clone(),
                    parent: Some(parent.clone()),
                    name: name.to_owned(),
                    kind: NodeKind::Folder,
                    trashed: false,
                    folder_mtime: Some(mtime),
                    revisions: Vec::new(),
                    trashed_at: 0,
                },
            );
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
        self.with_state(Some(FakeOp::Rename), |s| {
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
            if s.is_descendant(&parent, node) {
                return Err(BackendError::InvalidMove);
            }
            s.ensure_name_free(&parent, new_name, Some(node))?;
            if let Some(entry) = s.nodes.get_mut(node) {
                entry.parent = Some(parent);
                entry.name = new_name.to_owned();
            }
            s.touch(node);
            s.push_event(EventKind::UpdateMetadata, node);
            s.push_change(FakeChange::Renamed {
                link: node.clone(),
                name: new_name.to_owned(),
            });
            Ok(())
        })
    }

    async fn trash(&self, nodes: &[LinkId]) -> Result<Vec<Result<()>>> {
        self.checkpoint(FakePoint::TrashBeforeCommit).await;
        self.with_state(Some(FakeOp::Trash), |s| {
            Ok(nodes
                .iter()
                .map(|link| {
                    if link == &s.root {
                        return Err(BackendError::PermissionDenied);
                    }
                    s.node(link)?;
                    let at = s.generation + 1;
                    if let Some(entry) = s.nodes.get_mut(link) {
                        entry.trashed = true;
                        entry.trashed_at = at;
                    }
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
        self.with_state(Some(FakeOp::Revisions), |s| {
            let node = s.nodes.get(file).ok_or(BackendError::NotFound)?;
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
        self.with_state(Some(FakeOp::LatestEventId), |s| {
            Ok(EventId::new(s.latest_event().to_string()))
        })
    }

    async fn poll_events(&self, since: &EventId) -> Result<EventPage> {
        self.with_state(Some(FakeOp::PollEvents), |s| {
            let latest = s.latest_event();
            let since: u64 = since
                .as_str()
                .parse()
                .ok()
                .filter(|&i| i <= latest)
                .ok_or_else(|| BackendError::Other(format!("unknown event id {since}")))?;
            // A cursor older than the kept events: the client must refresh.
            if std::mem::take(&mut s.refresh_pending) || since < s.events_base {
                return Ok(EventPage {
                    next: EventId::new(latest.to_string()),
                    more: false,
                    refresh: true,
                    events: Vec::new(),
                });
            }
            let start = (since - s.events_base) as usize;
            let end = (start + s.event_page_size).min(s.events.len());
            Ok(EventPage {
                next: EventId::new((s.events_base + end as u64).to_string()),
                more: end < s.events.len(),
                refresh: false,
                events: s.events[start..end].to_vec(),
            })
        })
    }

    async fn storage_info(&self) -> Result<StorageInfo> {
        self.with_state(Some(FakeOp::StorageInfo), |s| {
            Ok(StorageInfo {
                used: s.used(),
                max: s.quota_max,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

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
        std::fs::remove_file(dir.path().join("faults.json")).expect("remove");
        daemon.root().await.expect("a missing file is no faults");
        std::fs::write(dir.path().join("faults.json"), b"not json").expect("write");
        daemon.root().await.expect("a malformed file is ignored");
    }
}
