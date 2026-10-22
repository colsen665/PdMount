//! Inode table (DESIGN.md §4.3, §8).
//!
//! The tree in RAM is authoritative for FUSE. Each node holds its **local**
//! (desired) state: placement, attributes, generations; and the **last known
//! remote** state, if the node exists remotely. The journal executor
//! reconciles the two (§8.2). A directory is `listed` once its children are
//! known; from then on lookups in it, including negative ones, are answered
//! from RAM. New local directories are listed from birth.
//!
//! Listings are merged, not copied: a node follows its remote placement and
//! attributes only while it has no pending local change of that kind, and
//! local nodes the server does not know yet are kept.
//!
//! Inode numbers are allocated from a persistent counter and never reused.
//! A node leaves the namespace (`attached == false`) when it is unlinked or
//! removed remotely; it stays in RAM while the kernel holds references, a
//! file handle is open, or it is a tombstone whose remote node still has to
//! be trashed.

use crate::errors::CoreError;
use pdmount_backend::{LinkId, NameHash, NodeKind, RemoteNode, RevisionId};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub type Ino = u64;
pub const ROOT_INO: Ino = 1;
pub const MAX_NAME_BYTES: usize = 255;
pub const DEFAULT_FILE_MODE: u32 = 0o644;
pub const DEFAULT_DIR_MODE: u32 = 0o755;

type Result<T> = std::result::Result<T, CoreError>;

/// Attributes reported to the kernel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attr {
    pub ino: Ino,
    pub kind: NodeKind,
    pub size: u64,
    pub mtime: SystemTime,
    pub ctime: SystemTime,
    pub atime: SystemTime,
    /// Permission bits only.
    pub perm: u32,
    /// Directories report 1 ("unknown"), so tools never infer the number of
    /// subdirectories from it.
    pub nlink: u32,
}

/// Last known state of the remote node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteState {
    pub link: LinkId,
    pub parent_link: Option<LinkId>,
    pub name: String,
    pub rev: Option<RevisionId>,
    pub size: u64,
    pub mtime: SystemTime,
    pub mode: Option<u32>,
    pub sha1: Option<[u8; 20]>,
    pub name_hash: Option<NameHash>,
}

impl RemoteState {
    pub fn from_remote(node: &RemoteNode) -> Self {
        Self {
            link: node.link_id.clone(),
            parent_link: node.parent_link_id.clone(),
            name: node.name.clone(),
            rev: node.active_revision.clone(),
            size: node.size.unwrap_or(0),
            mtime: node.mtime.unwrap_or(UNIX_EPOCH),
            mode: node.mode.map(|mode| mode & 0o7777),
            sha1: node.sha1,
            name_hash: node.name_hash.clone(),
        }
    }
}

/// Where a file's bytes come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Content {
    /// A remote revision; `None` for folders and for empty files without
    /// a revision.
    Remote {
        link: LinkId,
        rev: Option<RevisionId>,
    },
    /// The staging file, with not yet materialized ranges from the base
    /// revision (§7.1).
    Staged,
}

/// Generation counters (§7.1). A change is pending while a counter is ahead
/// of its uploaded counterpart.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Gens {
    pub content: u64,
    pub meta: u64,
    pub uploaded_content: u64,
    pub uploaded_meta: u64,
}

#[derive(Debug)]
pub struct Node {
    pub ino: Ino,
    /// Meaningful only while `attached`.
    pub parent: Ino,
    pub name: String,
    pub kind: NodeKind,
    pub size: u64,
    pub mtime: SystemTime,
    pub ctime: SystemTime,
    /// Set by `setattr` and kept locally only (§8.5); `None` follows `mtime`.
    pub atime: Option<SystemTime>,
    /// Permission bits.
    pub mode: u32,
    pub remote: Option<RemoteState>,
    pub content: Content,
    pub listed: bool,
    /// Children by name; complete only when `listed`.
    pub children: BTreeMap<String, Ino>,
    pub nlookup: u64,
    pub attached: bool,
    /// Unlinked locally while it exists remotely: its remote node must be
    /// trashed.
    pub deleted: bool,
    /// Matched by `.pdignore`: never uploaded (§7.7).
    pub local_only: bool,
    pub gens: Gens,
    /// Error of a failed upload, returned once by the next flush or fsync
    /// (§7.8).
    pub deferred_error: Option<i32>,
    pub open_handles: u32,
    pub open_writers: u32,
    /// Bumped when remote content replaces what the kernel may have cached.
    pub content_epoch: u64,
    /// `content_epoch` at the last open (`FOPEN_KEEP_CACHE`).
    pub served_epoch: Option<u64>,
    /// When the executor last changed this node remotely (engine clock).
    /// A listing fetched before that must not touch the node.
    pub remote_stamp: u64,
}

impl Node {
    fn new(ino: Ino, parent: Ino, name: String, kind: NodeKind) -> Self {
        Self {
            ino,
            parent,
            name,
            kind,
            size: 0,
            mtime: UNIX_EPOCH,
            ctime: UNIX_EPOCH,
            atime: None,
            mode: default_mode(kind),
            remote: None,
            content: Content::Staged,
            listed: false,
            children: BTreeMap::new(),
            nlookup: 0,
            attached: true,
            deleted: false,
            local_only: false,
            gens: Gens::default(),
            deferred_error: None,
            open_handles: 0,
            open_writers: 0,
            content_epoch: 0,
            served_epoch: None,
            remote_stamp: 0,
        }
    }

    /// A node mirroring a remote one, with no local change.
    fn from_remote(ino: Ino, parent: Ino, remote: RemoteState, kind: NodeKind) -> Self {
        let mut node = Self::new(ino, parent, remote.name.clone(), kind);
        node.adopt_remote_attrs(&remote);
        node.content = Content::Remote {
            link: remote.link.clone(),
            rev: remote.rev.clone(),
        };
        node.remote = Some(remote);
        node
    }

    fn adopt_remote_attrs(&mut self, remote: &RemoteState) {
        if self.kind == NodeKind::File {
            self.size = remote.size;
        }
        self.mtime = remote.mtime;
        self.ctime = remote.mtime;
        self.mode = remote.mode.unwrap_or(default_mode(self.kind));
    }

    pub fn attr(&self) -> Attr {
        Attr {
            ino: self.ino,
            kind: self.kind,
            size: match self.kind {
                NodeKind::File => self.size,
                NodeKind::Folder => 0,
            },
            mtime: self.mtime,
            ctime: self.ctime,
            atime: self.atime.unwrap_or(self.mtime),
            perm: self.mode,
            // An unlinked file that is still open has no name left.
            nlink: u32::from(self.attached),
        }
    }

    pub fn is_dir(&self) -> bool {
        self.kind == NodeKind::Folder
    }

    pub fn content_pending(&self) -> bool {
        self.content == Content::Staged && self.gens.content != self.gens.uploaded_content
    }

    /// Folder attributes never upload (D013).
    pub fn meta_pending(&self) -> bool {
        self.kind == NodeKind::File && self.gens.meta != self.gens.uploaded_meta
    }

    pub fn link(&self) -> Option<&LinkId> {
        self.remote.as_ref().map(|remote| &remote.link)
    }
}

pub fn default_mode(kind: NodeKind) -> u32 {
    match kind {
        NodeKind::File => DEFAULT_FILE_MODE,
        NodeKind::Folder => DEFAULT_DIR_MODE,
    }
}

/// Kernel cache invalidations (DESIGN.md §9.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Invalidation {
    Entry {
        parent: Ino,
        name: String,
    },
    Inode {
        ino: Ino,
    },
    Delete {
        parent: Ino,
        child: Ino,
        name: String,
    },
}

/// Everything the store keeps about a node (§13.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRow {
    pub ino: Ino,
    pub kind: NodeKind,
    /// `None` for the root and for detached nodes (tombstones).
    pub parent: Option<Ino>,
    pub name: Option<String>,
    pub size: u64,
    pub mtime: SystemTime,
    pub ctime: SystemTime,
    pub atime: Option<SystemTime>,
    pub mode: u32,
    pub listed: bool,
    pub local_only: bool,
    pub deleted: bool,
    pub gens: Gens,
    pub deferred_error: Option<i32>,
    pub remote: Option<RemoteState>,
}

/// Effects of a tree change: rows to rewrite or delete, kernel caches to
/// drop.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Changes {
    pub changed: Vec<Ino>,
    pub removed: Vec<Ino>,
    pub invalidations: Vec<Invalidation>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ApplyError {
    /// The directory no longer exists, is not a directory, or is not known
    /// remotely.
    NotADirectory,
    /// More new inode numbers are needed than were provided.
    NeedInos(usize),
}

#[derive(Debug, PartialEq, Eq)]
pub enum Lookup {
    Found(Attr),
    Negative,
    /// The parent was never listed; the slow path must list it first.
    NeedsListing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    pub ino: Ino,
    pub kind: NodeKind,
    pub name: String,
}

/// What a rename did, for the engine's follow-up (adoption, tombstones).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Renamed {
    pub ino: Ino,
    /// The node that was at the target name, now detached.
    pub replaced: Option<Ino>,
    pub old_parent: Ino,
    pub old_name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenameMode {
    Replace,
    NoReplace,
}

#[derive(Debug, Default)]
pub struct Tree {
    nodes: HashMap<Ino, Node>,
    by_link: HashMap<LinkId, Ino>,
    /// Remote placement `(parent link, name)` of every node known remotely.
    by_remote_name: HashMap<(LinkId, String), Ino>,
    /// Links the executor trashed, with the clock at that time, so a listing
    /// fetched before does not bring them back.
    recently_trashed: HashMap<LinkId, u64>,
}

/// Recently trashed links kept for stale listings.
const RECENTLY_TRASHED: usize = 8192;

/// Checks a name from the kernel (§8.8).
pub fn validate_name(name: &[u8]) -> Result<&str> {
    if name.len() > MAX_NAME_BYTES {
        return Err(CoreError::NameTooLong);
    }
    let name = std::str::from_utf8(name).map_err(|_| CoreError::InvalidName)?;
    if name.is_empty() || name == "." || name == ".." || name.contains('/') {
        return Err(CoreError::InvalidArgument);
    }
    Ok(name)
}

/// Whether a remote name can appear in a POSIX directory.
fn representable(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && name.len() <= MAX_NAME_BYTES
        && !name.contains(['/', '\0'])
}

impl Tree {
    /// A tree holding only the root.
    pub fn with_root(root: RemoteState) -> Self {
        let mut tree = Self::default();
        let node = Node::from_remote(ROOT_INO, ROOT_INO, root, NodeKind::Folder);
        tree.insert(node);
        tree
    }

    /// Rebuilds the tree from stored rows. Attached rows whose parent is
    /// missing or not listed are dropped and returned so the store can
    /// delete them. Detached rows are tombstones.
    pub fn from_rows(rows: Vec<NodeRow>) -> (Self, Vec<Ino>) {
        let mut tree = Self::default();
        let mut stale = Vec::new();
        let mut by_parent: HashMap<Ino, Vec<Ino>> = HashMap::new();
        for row in rows {
            let attached = row.ino == ROOT_INO || row.parent.is_some();
            if !attached && !(row.deleted && row.remote.is_some()) {
                stale.push(row.ino);
                continue;
            }
            let mut node = Node::new(
                row.ino,
                row.parent.unwrap_or(ROOT_INO),
                row.name.unwrap_or_default(),
                row.kind,
            );
            node.size = row.size;
            node.mtime = row.mtime;
            node.ctime = row.ctime;
            node.atime = row.atime;
            node.mode = row.mode;
            node.listed = row.listed;
            node.local_only = row.local_only;
            node.deleted = row.deleted;
            node.gens = row.gens;
            node.deferred_error = row.deferred_error;
            node.attached = attached;
            node.content = match &row.remote {
                Some(remote) => Content::Remote {
                    link: remote.link.clone(),
                    rev: remote.rev.clone(),
                },
                None => Content::Staged,
            };
            node.remote = row.remote;
            if attached && row.ino != ROOT_INO {
                by_parent.entry(node.parent).or_default().push(node.ino);
            }
            tree.insert(node);
        }
        // Attach children to listed parents; anything unreachable is stale.
        let mut reachable = HashSet::from([ROOT_INO]);
        let mut queue = vec![ROOT_INO];
        while let Some(dir) = queue.pop() {
            if !tree.nodes.get(&dir).is_some_and(|node| node.listed) {
                continue;
            }
            for child in by_parent.remove(&dir).unwrap_or_default() {
                let name = tree.nodes[&child].name.clone();
                if let Some(dir_node) = tree.nodes.get_mut(&dir) {
                    dir_node.children.insert(name, child);
                }
                reachable.insert(child);
                queue.push(child);
            }
        }
        let unreachable: Vec<Ino> = tree
            .nodes
            .values()
            .filter(|node| node.attached && !reachable.contains(&node.ino))
            .map(|node| node.ino)
            .collect();
        for ino in unreachable {
            tree.drop_node(ino);
            stale.push(ino);
        }
        (tree, stale)
    }

    fn insert(&mut self, node: Node) {
        let ino = node.ino;
        let remote = node.remote.clone();
        self.nodes.insert(ino, node);
        self.index_remote(ino, remote.as_ref());
    }

    fn index_remote(&mut self, ino: Ino, remote: Option<&RemoteState>) {
        if let Some(remote) = remote {
            self.by_link.insert(remote.link.clone(), ino);
            if let Some(parent) = &remote.parent_link {
                self.by_remote_name
                    .insert((parent.clone(), remote.name.clone()), ino);
            }
        }
    }

    fn unindex_remote(&mut self, ino: Ino, remote: Option<&RemoteState>) {
        if let Some(remote) = remote {
            if self.by_link.get(&remote.link) == Some(&ino) {
                self.by_link.remove(&remote.link);
            }
            if let Some(parent) = &remote.parent_link {
                let key = (parent.clone(), remote.name.clone());
                if self.by_remote_name.get(&key) == Some(&ino) {
                    self.by_remote_name.remove(&key);
                }
            }
        }
    }

    /// Replaces a node's remote state, keeping the indexes consistent.
    pub fn set_remote(&mut self, ino: Ino, remote: Option<RemoteState>) {
        let Some(node) = self.nodes.get_mut(&ino) else {
            return;
        };
        let old = std::mem::replace(&mut node.remote, remote.clone());
        self.unindex_remote(ino, old.as_ref());
        self.index_remote(ino, remote.as_ref());
    }

    /// Removes a node from RAM entirely.
    pub fn drop_node(&mut self, ino: Ino) {
        if let Some(node) = self.nodes.remove(&ino) {
            self.unindex_remote(ino, node.remote.as_ref());
        }
    }

    pub fn get(&self, ino: Ino) -> Option<&Node> {
        self.nodes.get(&ino)
    }

    pub fn get_mut(&mut self, ino: Ino) -> Option<&mut Node> {
        self.nodes.get_mut(&ino)
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn inos(&self) -> impl Iterator<Item = Ino> + '_ {
        self.nodes.keys().copied()
    }

    pub fn by_link(&self, link: &LinkId) -> Option<Ino> {
        self.by_link.get(link).copied()
    }

    /// An attached directory whose children are known.
    pub fn is_listed(&self, ino: Ino) -> bool {
        self.listed_dir(ino).is_ok()
    }

    /// Whether `remote` is what the tree already holds for `ino`: the same
    /// placement and name and, for a file, the same active revision (mode
    /// and mtime travel with revisions).
    pub fn knows_remote(&self, ino: Ino, remote: &RemoteNode) -> bool {
        self.nodes
            .get(&ino)
            .and_then(|node| node.remote.as_ref())
            .is_some_and(|known| {
                !remote.trashed
                    && known.parent_link == remote.parent_link_id
                    && known.name == remote.name
                    && (remote.kind == NodeKind::Folder || known.rev == remote.active_revision)
            })
    }

    /// The listed directories that hold `ino`: its local parent and the
    /// directory of its known remote parent, when they differ.
    pub fn listed_parents(&self, ino: Ino) -> Vec<Ino> {
        let Some(node) = self.nodes.get(&ino) else {
            return Vec::new();
        };
        let remote_parent = node
            .remote
            .as_ref()
            .and_then(|remote| remote.parent_link.as_ref())
            .and_then(|link| self.by_link(link));
        let mut parents: Vec<Ino> = [Some(node.parent), remote_parent]
            .into_iter()
            .flatten()
            .filter(|&dir| self.is_listed(dir))
            .collect();
        parents.dedup();
        parents
    }

    /// The node that holds `(parent, name)` remotely, if any.
    pub fn remote_occupant(&self, parent: &LinkId, name: &str) -> Option<Ino> {
        self.by_remote_name
            .get(&(parent.clone(), name.to_owned()))
            .copied()
    }

    /// The remote link of an attached directory, if it exists remotely.
    pub fn link_of(&self, ino: Ino) -> Option<&LinkId> {
        self.nodes.get(&ino)?.link()
    }

    /// Path of an attached node, for messages and access rules.
    pub fn path(&self, ino: Ino) -> Option<String> {
        let mut parts = Vec::new();
        let mut current = ino;
        while current != ROOT_INO {
            let node = self.nodes.get(&current)?;
            if !node.attached {
                return None;
            }
            parts.push(node.name.as_str());
            current = node.parent;
        }
        parts.reverse();
        Some(format!("/{}", parts.join("/")))
    }

    /// Whether `ino` is `ancestor` or below it.
    pub fn is_within(&self, mut ino: Ino, ancestor: Ino) -> bool {
        loop {
            if ino == ancestor {
                return true;
            }
            match self.nodes.get(&ino) {
                Some(node) if node.attached && ino != ROOT_INO => ino = node.parent,
                _ => return false,
            }
        }
    }

    /// Listed directories, root first (breadth-first).
    pub fn listed_dirs(&self) -> Vec<Ino> {
        let mut out = Vec::new();
        let mut queue = std::collections::VecDeque::from([ROOT_INO]);
        while let Some(ino) = queue.pop_front() {
            let Some(node) = self.nodes.get(&ino) else {
                continue;
            };
            if node.is_dir() && node.listed {
                if node.remote.is_some() {
                    out.push(ino);
                }
                queue.extend(node.children.values().copied());
            }
        }
        out
    }

    /// Hot-path lookup. Increments `nlookup` on success.
    pub fn lookup(&mut self, parent: Ino, name: &str) -> Result<Lookup> {
        let dir = self.nodes.get(&parent).ok_or(CoreError::NotFound)?;
        if !dir.is_dir() {
            return Err(CoreError::NotADirectory);
        }
        if !dir.listed {
            return Ok(Lookup::NeedsListing);
        }
        let Some(&child) = dir.children.get(name) else {
            return Ok(Lookup::Negative);
        };
        let node = self.nodes.get_mut(&child).ok_or(CoreError::NotFound)?;
        node.nlookup += 1;
        Ok(Lookup::Found(node.attr()))
    }

    /// The child at `name` of a listed directory.
    pub fn child(&self, parent: Ino, name: &str) -> Result<Option<Ino>> {
        let dir = self.listed_dir(parent)?;
        Ok(dir.children.get(name).copied())
    }

    /// An attached, listed directory; `NeedsListing` when not listed.
    fn listed_dir(&self, ino: Ino) -> Result<&Node> {
        let dir = self.nodes.get(&ino).ok_or(CoreError::NotFound)?;
        if !dir.is_dir() {
            return Err(CoreError::NotADirectory);
        }
        if !dir.attached {
            return Err(CoreError::NotFound);
        }
        if !dir.listed {
            return Err(CoreError::NeedsListing(ino));
        }
        Ok(dir)
    }

    /// Adds kernel references, e.g. for entries returned by readdirplus.
    pub fn add_lookup(&mut self, ino: Ino) -> Option<Attr> {
        let node = self.nodes.get_mut(&ino)?;
        node.nlookup += 1;
        Some(node.attr())
    }

    /// Whether a detached node can leave RAM.
    pub fn is_droppable(&self, ino: Ino) -> bool {
        self.nodes.get(&ino).is_some_and(|node| {
            ino != ROOT_INO
                && !node.attached
                && node.nlookup == 0
                && node.open_handles == 0
                && !node.deleted
        })
    }

    /// Drops kernel references. Returns whether the node is now droppable.
    pub fn forget(&mut self, ino: Ino, count: u64) -> bool {
        let Some(node) = self.nodes.get_mut(&ino) else {
            return false;
        };
        node.nlookup = node.nlookup.saturating_sub(count);
        self.is_droppable(ino)
    }

    /// Snapshot of a listed directory for readdir, with `.` and `..`.
    pub fn dir_entries(&self, ino: Ino) -> Result<Option<Vec<DirEntry>>> {
        let dir = self.nodes.get(&ino).ok_or(CoreError::NotFound)?;
        if !dir.is_dir() {
            return Err(CoreError::NotADirectory);
        }
        if !dir.listed {
            return Ok(None);
        }
        let mut entries = Vec::with_capacity(dir.children.len() + 2);
        entries.push(DirEntry {
            ino,
            kind: NodeKind::Folder,
            name: ".".into(),
        });
        entries.push(DirEntry {
            ino: dir.parent,
            kind: NodeKind::Folder,
            name: "..".into(),
        });
        for (name, &child) in &dir.children {
            if let Some(node) = self.nodes.get(&child) {
                entries.push(DirEntry {
                    ino: child,
                    kind: node.kind,
                    name: name.clone(),
                });
            }
        }
        Ok(Some(entries))
    }

    /// Number of children of a listing that have no inode yet.
    pub fn unknown_count(&self, children: &[RemoteNode]) -> usize {
        children
            .iter()
            .filter(|child| !self.by_link.contains_key(&child.link_id))
            .count()
    }

    /// Whether the node or anything below it has a local change that is not
    /// uploaded yet.
    pub fn subtree_has_pending(&self, ino: Ino) -> bool {
        let mut stack = vec![ino];
        while let Some(current) = stack.pop() {
            let Some(node) = self.nodes.get(&current) else {
                continue;
            };
            if node.local_only || node.content_pending() || node.meta_pending() {
                return true;
            }
            if self.placement_pending(current) {
                return true;
            }
            stack.extend(node.children.values().copied());
        }
        false
    }

    /// Whether an attached node's local placement differs from its remote
    /// one (including not existing remotely).
    pub fn placement_pending(&self, ino: Ino) -> bool {
        let Some(node) = self.nodes.get(&ino) else {
            return false;
        };
        if ino == ROOT_INO || !node.attached {
            return false;
        }
        let Some(remote) = &node.remote else {
            return true;
        };
        remote.name != node.name || remote.parent_link.as_ref() != self.link_of(node.parent)
    }

    /// Records a link the executor trashed at clock `stamp`.
    pub fn note_trashed(&mut self, link: LinkId, stamp: u64) {
        if self.recently_trashed.len() >= RECENTLY_TRASHED {
            let mut stamps: Vec<u64> = self.recently_trashed.values().copied().collect();
            stamps.sort_unstable();
            let cut = stamps[stamps.len() / 2];
            self.recently_trashed.retain(|_, s| *s > cut);
        }
        self.recently_trashed.insert(link, stamp);
    }

    /// Applies a full listing of `dir`, taking new inode numbers from
    /// `new_inos`. Nothing changes when an error is returned.
    pub fn apply_listing(
        &mut self,
        dir: Ino,
        children: &[RemoteNode],
        new_inos: &mut Vec<Ino>,
    ) -> std::result::Result<Changes, ApplyError> {
        self.apply_listing_at(dir, children, new_inos, u64::MAX)
    }

    /// [`Tree::apply_listing`] for a listing fetched at clock `fetched_at`:
    /// nodes the executor changed since, and links it trashed since, are
    /// left alone, because the listing does not show those changes yet.
    pub fn apply_listing_at(
        &mut self,
        dir: Ino,
        children: &[RemoteNode],
        new_inos: &mut Vec<Ino>,
        fetched_at: u64,
    ) -> std::result::Result<Changes, ApplyError> {
        let dir_node = self.nodes.get(&dir).ok_or(ApplyError::NotADirectory)?;
        if !dir_node.is_dir() || !dir_node.attached {
            return Err(ApplyError::NotADirectory);
        }
        let dir_link = dir_node.link().cloned().ok_or(ApplyError::NotADirectory)?;
        let was_listed = dir_node.listed;

        let mut incoming: Vec<&RemoteNode> = Vec::with_capacity(children.len());
        let mut seen_names = HashSet::new();
        for child in children {
            if child.trashed {
                continue;
            }
            if !representable(&child.name) {
                tracing::warn!(name = ?child.name, "skipping a remote name that cannot be shown");
                continue;
            }
            if child.link_id == dir_link || !seen_names.insert(child.name.as_str()) {
                tracing::warn!(name = ?child.name, "skipping a duplicate remote name");
                continue;
            }
            if self
                .recently_trashed
                .get(&child.link_id)
                .is_some_and(|stamp| *stamp > fetched_at)
            {
                continue;
            }
            incoming.push(child);
        }
        let changed_since = |tree: &Self, ino: Ino| {
            tree.nodes
                .get(&ino)
                .is_some_and(|node| node.remote_stamp > fetched_at)
        };
        let unknown = incoming
            .iter()
            .filter(|child| !self.by_link.contains_key(&child.link_id))
            .count();
        if unknown > new_inos.len() {
            return Err(ApplyError::NeedInos(unknown - new_inos.len()));
        }
        let incoming_links: HashMap<&LinkId, &RemoteNode> =
            incoming.iter().map(|c| (&c.link_id, *c)).collect();

        let mut changes = Changes::default();
        let old_children = std::mem::take(&mut self.nodes.get_mut(&dir).expect("checked").children);
        // Whether each known node follows its remote placement: it has no
        // local placement change of its own.
        let follows: HashSet<Ino> = incoming
            .iter()
            .filter_map(|c| self.by_link.get(&c.link_id).copied())
            .filter(|&ino| {
                self.nodes
                    .get(&ino)
                    .is_some_and(|node| node.attached && !node.deleted)
                    && !self.placement_pending(ino)
                    && !changed_since(self, ino)
            })
            .collect();

        // Old children: keep, remove (gone remotely) or leave to the
        // incoming pass (followers).
        let mut kept: BTreeMap<String, Ino> = BTreeMap::new();
        for (name, &ino) in &old_children {
            if follows.contains(&ino) {
                continue;
            }
            if changed_since(self, ino) {
                kept.insert(name.clone(), ino);
                continue;
            }
            let node = &self.nodes[&ino];
            let remote_here = node
                .remote
                .as_ref()
                .filter(|r| r.parent_link.as_ref() == Some(&dir_link));
            match remote_here {
                Some(remote) if !incoming_links.contains_key(&remote.link) => {
                    if self.subtree_has_pending(ino) {
                        // Gone remotely with local changes inside: keep it
                        // and upload it again (§10, resurrection).
                        tracing::warn!(name, "removed remotely but changed locally; keeping it");
                        self.forget_remote_subtree(ino, &mut changes);
                        kept.insert(name.clone(), ino);
                    } else {
                        if was_listed {
                            changes.invalidations.push(Invalidation::Delete {
                                parent: dir,
                                child: ino,
                                name: name.clone(),
                            });
                        }
                        self.remove_subtree(ino, &mut changes.removed);
                    }
                }
                _ => {
                    kept.insert(name.clone(), ino);
                }
            }
        }

        let mut final_children = kept;
        for child in incoming {
            let remote = RemoteState::from_remote(child);
            match self.by_link.get(&child.link_id).copied() {
                Some(ino) if changed_since(self, ino) => {}
                Some(ino) => {
                    self.set_remote(ino, Some(remote.clone()));
                    self.refresh_attrs(ino, &remote, &mut changes);
                    if !follows.contains(&ino) {
                        // A tombstone, or placed locally elsewhere.
                        continue;
                    }
                    let (old_parent, old_name) = {
                        let node = &self.nodes[&ino];
                        (node.parent, node.name.clone())
                    };
                    if final_children.contains_key(&child.name) {
                        // The name is taken by a local change; stay put.
                        tracing::warn!(
                            name = child.name,
                            "remote rename onto a locally used name; not applied"
                        );
                        if old_parent == dir {
                            match final_children.entry(old_name) {
                                std::collections::btree_map::Entry::Vacant(entry) => {
                                    entry.insert(ino);
                                }
                                std::collections::btree_map::Entry::Occupied(_) => {
                                    // Both names are taken locally: hidden
                                    // until the conflict engine (M3).
                                    if let Some(node) = self.nodes.get_mut(&ino) {
                                        node.attached = false;
                                    }
                                    changes.removed.push(ino);
                                }
                            }
                        }
                        continue;
                    }
                    if old_parent != dir
                        && let Some(old_dir) = self.nodes.get_mut(&old_parent)
                        && old_dir.children.get(&old_name) == Some(&ino)
                    {
                        old_dir.children.remove(&old_name);
                        changes.invalidations.push(Invalidation::Entry {
                            parent: old_parent,
                            name: old_name.clone(),
                        });
                    }
                    let node = self.nodes.get_mut(&ino).expect("indexed");
                    node.parent = dir;
                    node.name = child.name.clone();
                    final_children.insert(child.name.clone(), ino);
                    changes.changed.push(ino);
                }
                None => {
                    if final_children.contains_key(&child.name) {
                        tracing::warn!(
                            name = child.name,
                            "a new remote node has a locally used name; not shown"
                        );
                        continue;
                    }
                    let ino = new_inos.pop().expect("counted above");
                    self.insert(Node::from_remote(ino, dir, remote, child.kind));
                    final_children.insert(child.name.clone(), ino);
                    changes.changed.push(ino);
                }
            }
        }

        if was_listed {
            for (name, ino) in &old_children {
                if final_children.get(name) != Some(ino)
                    && self.nodes.get(ino).is_some_and(|n| n.attached)
                {
                    changes.invalidations.push(Invalidation::Entry {
                        parent: dir,
                        name: name.clone(),
                    });
                }
            }
            for (name, ino) in &final_children {
                if old_children.get(name) != Some(ino) {
                    changes.invalidations.push(Invalidation::Entry {
                        parent: dir,
                        name: name.clone(),
                    });
                }
            }
            changes.invalidations.dedup();
            // Directory pages are cached (`FOPEN_CACHE_DIR`); drop them too.
            let entries_changed = changes.invalidations.iter().any(|i| {
                matches!(i, Invalidation::Entry { parent, .. } | Invalidation::Delete { parent, .. } if *parent == dir)
            });
            if entries_changed {
                changes.invalidations.push(Invalidation::Inode { ino: dir });
            }
        }
        changes.changed.extend(final_children.values().copied());
        changes.changed.push(dir);
        changes.changed.sort_unstable();
        changes.changed.dedup();
        let dir_node = self.nodes.get_mut(&dir).expect("checked");
        dir_node.children = final_children;
        dir_node.listed = true;
        Ok(changes)
    }

    /// Takes over remote attributes and content where nothing local is
    /// pending.
    fn refresh_attrs(&mut self, ino: Ino, remote: &RemoteState, changes: &mut Changes) {
        let Some(node) = self.nodes.get_mut(&ino) else {
            return;
        };
        let mut changed = false;
        if node.kind == NodeKind::File
            && matches!(node.content, Content::Remote { .. })
            && !node.content_pending()
        {
            let content = Content::Remote {
                link: remote.link.clone(),
                rev: remote.rev.clone(),
            };
            if node.content != content || node.size != remote.size {
                node.content = content;
                node.size = remote.size;
                node.content_epoch += 1;
                changed = true;
            }
        }
        // A file waiting for upload keeps its local mtime and mode too: the
        // upload carries them.
        if !node.meta_pending() && !node.content_pending() && node.kind == NodeKind::File {
            let mode = remote.mode.unwrap_or(DEFAULT_FILE_MODE);
            if node.mtime != remote.mtime || node.mode != mode {
                node.mtime = remote.mtime;
                node.ctime = remote.mtime;
                node.mode = mode;
                changed = true;
            }
        }
        if changed {
            changes.invalidations.push(Invalidation::Inode { ino });
        }
    }

    /// Clears the remote state of a subtree removed remotely, so that it is
    /// created again.
    fn forget_remote_subtree(&mut self, ino: Ino, changes: &mut Changes) {
        let mut stack = vec![ino];
        while let Some(current) = stack.pop() {
            let Some(node) = self.nodes.get(&current) else {
                continue;
            };
            stack.extend(node.children.values().copied());
            self.set_remote(current, None);
            if let Some(node) = self.nodes.get_mut(&current)
                && node.kind == NodeKind::File
                && matches!(node.content, Content::Remote { .. })
            {
                // Its content is still readable from the old revision;
                // the engine stages it before uploading.
                node.gens.content = node.gens.uploaded_content + 1;
            }
            changes.changed.push(current);
        }
    }

    pub fn row(&self, ino: Ino) -> Option<NodeRow> {
        let node = self.nodes.get(&ino)?;
        let attached = node.attached && ino != ROOT_INO;
        Some(NodeRow {
            ino,
            kind: node.kind,
            parent: attached.then_some(node.parent),
            name: attached.then(|| node.name.clone()),
            size: node.size,
            mtime: node.mtime,
            ctime: node.ctime,
            atime: node.atime,
            mode: node.mode,
            listed: node.listed,
            local_only: node.local_only,
            deleted: node.deleted,
            gens: node.gens,
            deferred_error: node.deferred_error,
            remote: node.remote.clone(),
        })
    }

    /// Whether a node should have a store row: attached, or a tombstone.
    pub fn persisted(&self, ino: Ino) -> bool {
        self.nodes
            .get(&ino)
            .is_some_and(|node| node.attached || (node.deleted && node.remote.is_some()))
    }

    /// Removes a subtree that is gone remotely. Nodes the kernel still
    /// references stay, detached, until forgotten.
    fn remove_subtree(&mut self, ino: Ino, removed: &mut Vec<Ino>) {
        let mut stack = vec![ino];
        while let Some(current) = stack.pop() {
            let Some(node) = self.nodes.get_mut(&current) else {
                continue;
            };
            stack.extend(node.children.values().copied());
            node.children.clear();
            node.attached = false;
            node.listed = false;
            removed.push(current);
            self.set_remote(current, None);
            if self.is_droppable(current) {
                self.drop_node(current);
            }
        }
    }

    // ---- local namespace changes (§8) ----

    /// Adds a new local node. The caller checked the name.
    pub fn create(
        &mut self,
        parent: Ino,
        name: &str,
        kind: NodeKind,
        mode: u32,
        now: SystemTime,
        ino: Ino,
    ) -> Result<Attr> {
        let dir = self.listed_dir(parent)?;
        if dir.children.contains_key(name) {
            return Err(CoreError::Exists);
        }
        let mut node = Node::new(ino, parent, name.to_owned(), kind);
        node.mode = mode & 0o7777;
        node.mtime = now;
        node.ctime = now;
        node.nlookup = 1;
        match kind {
            NodeKind::Folder => node.listed = true,
            NodeKind::File => node.gens.content = 1,
        }
        let attr = node.attr();
        self.insert(node);
        let dir = self.nodes.get_mut(&parent).expect("checked");
        dir.children.insert(name.to_owned(), ino);
        dir.mtime = now;
        dir.ctime = now;
        Ok(attr)
    }

    /// Unlinks a file or an empty directory. Returns the detached node.
    pub fn remove_entry(
        &mut self,
        parent: Ino,
        name: &str,
        expect_dir: bool,
        now: SystemTime,
    ) -> Result<Ino> {
        let ino = self
            .listed_dir(parent)?
            .children
            .get(name)
            .copied()
            .ok_or(CoreError::NotFound)?;
        let node = self.nodes.get(&ino).ok_or(CoreError::NotFound)?;
        match (expect_dir, node.is_dir()) {
            (false, true) => return Err(CoreError::IsADirectory),
            (true, false) => return Err(CoreError::NotADirectory),
            (true, true) => {
                if !node.listed {
                    return Err(CoreError::NeedsListing(ino));
                }
                if !node.children.is_empty() {
                    return Err(CoreError::NotEmpty);
                }
            }
            (false, false) => {}
        }
        let dir = self.nodes.get_mut(&parent).expect("checked");
        dir.children.remove(name);
        dir.mtime = now;
        dir.ctime = now;
        self.detach(ino, now);
        Ok(ino)
    }

    /// Takes a node out of the namespace; with a remote node it becomes a
    /// tombstone.
    fn detach(&mut self, ino: Ino, now: SystemTime) {
        if let Some(node) = self.nodes.get_mut(&ino) {
            node.attached = false;
            node.deleted = node.remote.is_some();
            node.ctime = now;
        }
    }

    /// POSIX rename within the tree (§8.3). The target, if replaced, is
    /// detached (and a tombstone if remote); the caller may turn that into
    /// an adoption.
    pub fn rename(
        &mut self,
        parent: Ino,
        name: &str,
        new_parent: Ino,
        new_name: &str,
        mode: RenameMode,
        now: SystemTime,
    ) -> Result<Renamed> {
        let ino = self
            .listed_dir(parent)?
            .children
            .get(name)
            .copied()
            .ok_or(CoreError::NotFound)?;
        let target = self.listed_dir(new_parent)?.children.get(new_name).copied();
        let renamed = Renamed {
            ino,
            replaced: None,
            old_parent: parent,
            old_name: name.to_owned(),
        };
        if target == Some(ino) {
            return Ok(renamed);
        }
        let source_dir = self.nodes[&ino].is_dir();
        if source_dir && self.is_within(new_parent, ino) {
            return Err(CoreError::InvalidArgument);
        }
        if let Some(target) = target {
            if mode == RenameMode::NoReplace {
                return Err(CoreError::Exists);
            }
            let target_node = &self.nodes[&target];
            match (source_dir, target_node.is_dir()) {
                (true, false) => return Err(CoreError::NotADirectory),
                (false, true) => return Err(CoreError::IsADirectory),
                (true, true) => {
                    if !target_node.listed {
                        return Err(CoreError::NeedsListing(target));
                    }
                    if !target_node.children.is_empty() {
                        return Err(CoreError::NotEmpty);
                    }
                }
                (false, false) => {}
            }
        }
        if let Some(target) = target {
            self.nodes
                .get_mut(&new_parent)
                .expect("checked")
                .children
                .remove(new_name);
            self.detach(target, now);
        }
        for dir in [parent, new_parent] {
            if let Some(dir) = self.nodes.get_mut(&dir) {
                dir.mtime = now;
                dir.ctime = now;
            }
        }
        self.nodes
            .get_mut(&parent)
            .expect("checked")
            .children
            .remove(name);
        self.nodes
            .get_mut(&new_parent)
            .expect("checked")
            .children
            .insert(new_name.to_owned(), ino);
        let node = self.nodes.get_mut(&ino).expect("checked");
        node.parent = new_parent;
        node.name = new_name.to_owned();
        node.ctime = now;
        Ok(Renamed {
            replaced: target,
            ..renamed
        })
    }
}

pub fn system_time_to_ns(time: SystemTime) -> i64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(after) => i64::try_from(after.as_nanos()).unwrap_or(i64::MAX),
        Err(before) => -i64::try_from(before.duration().as_nanos()).unwrap_or(i64::MAX),
    }
}

pub fn ns_to_system_time(ns: i64) -> SystemTime {
    if ns >= 0 {
        UNIX_EPOCH + Duration::from_nanos(ns as u64)
    } else {
        UNIX_EPOCH - Duration::from_nanos(ns.unsigned_abs())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root_state() -> RemoteState {
        RemoteState {
            link: LinkId::new("root"),
            parent_link: None,
            name: String::new(),
            rev: None,
            size: 0,
            mtime: UNIX_EPOCH,
            mode: None,
            sha1: None,
            name_hash: None,
        }
    }

    fn remote_in(parent: &str, link: &str, name: &str, kind: NodeKind, size: u64) -> RemoteNode {
        RemoteNode {
            link_id: LinkId::new(link),
            parent_link_id: Some(LinkId::new(parent)),
            name: name.into(),
            kind,
            active_revision: (kind == NodeKind::File)
                .then(|| RevisionId::new(format!("{link}-r1"))),
            size: Some(size),
            mtime: Some(UNIX_EPOCH + Duration::from_secs(1000)),
            mode: None,
            sha1: None,
            name_hash: None,
            trashed: false,
        }
    }

    fn remote(link: &str, name: &str, kind: NodeKind, size: u64) -> RemoteNode {
        remote_in("root", link, name, kind, size)
    }

    fn inos(range: std::ops::Range<Ino>) -> Vec<Ino> {
        range.rev().collect()
    }

    fn now() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(5000)
    }

    #[test]
    fn first_listing_creates_children_without_invalidations() {
        let mut tree = Tree::with_root(root_state());
        let listing = [
            remote("a", "a.txt", NodeKind::File, 3),
            remote("d", "dir", NodeKind::Folder, 0),
        ];
        let changes = tree
            .apply_listing(ROOT_INO, &listing, &mut inos(2..4))
            .expect("apply");
        assert!(changes.changed.contains(&2) && changes.changed.contains(&3));
        assert!(changes.invalidations.is_empty());
        assert_eq!(
            tree.lookup(ROOT_INO, "a.txt").expect("lookup"),
            Lookup::Found(tree.get(2).expect("a").attr())
        );
        assert_eq!(
            tree.lookup(ROOT_INO, "missing").expect("lookup"),
            Lookup::Negative
        );
        assert_eq!(tree.lookup(3, "x").expect("lookup"), Lookup::NeedsListing);
    }

    #[test]
    fn relisting_reports_renames_updates_and_removals() {
        let mut tree = Tree::with_root(root_state());
        let first = [
            remote("a", "a.txt", NodeKind::File, 3),
            remote("b", "b.txt", NodeKind::File, 3),
        ];
        tree.apply_listing(ROOT_INO, &first, &mut inos(2..4))
            .expect("apply");
        let mut renamed = remote("a", "renamed.txt", NodeKind::File, 5);
        renamed.active_revision = Some(RevisionId::new("a-r2"));
        let changes = tree
            .apply_listing(ROOT_INO, &[renamed], &mut Vec::new())
            .expect("apply");
        assert!(
            changes
                .invalidations
                .contains(&Invalidation::Inode { ino: 2 })
        );
        assert!(changes.invalidations.contains(&Invalidation::Entry {
            parent: ROOT_INO,
            name: "a.txt".into()
        }));
        assert!(changes.invalidations.contains(&Invalidation::Delete {
            parent: ROOT_INO,
            child: 3,
            name: "b.txt".into()
        }));
        assert_eq!(changes.removed, vec![3]);
        // The inode number survives the rename.
        assert!(
            matches!(tree.lookup(ROOT_INO, "renamed.txt"), Ok(Lookup::Found(attr)) if attr.ino == 2 && attr.size == 5)
        );
    }

    #[test]
    fn a_move_between_listed_directories_keeps_the_inode() {
        let mut tree = Tree::with_root(root_state());
        tree.apply_listing(
            ROOT_INO,
            &[
                remote("d1", "d1", NodeKind::Folder, 0),
                remote("d2", "d2", NodeKind::Folder, 0),
            ],
            &mut inos(2..4),
        )
        .expect("root");
        tree.apply_listing(
            2,
            &[remote_in("d1", "f", "f", NodeKind::File, 1)],
            &mut inos(4..5),
        )
        .expect("d1");
        tree.apply_listing(3, &[], &mut Vec::new()).expect("d2");
        let changes = tree
            .apply_listing(
                3,
                &[remote_in("d2", "f", "f", NodeKind::File, 1)],
                &mut Vec::new(),
            )
            .expect("move");
        assert!(changes.invalidations.contains(&Invalidation::Entry {
            parent: 2,
            name: "f".into()
        }));
        assert_eq!(tree.lookup(2, "f").expect("old"), Lookup::Negative);
        assert!(matches!(tree.lookup(3, "f"), Ok(Lookup::Found(attr)) if attr.ino == 4));
    }

    #[test]
    fn removed_nodes_with_kernel_references_live_until_forgotten() {
        let mut tree = Tree::with_root(root_state());
        tree.apply_listing(
            ROOT_INO,
            &[remote("a", "a", NodeKind::File, 1)],
            &mut inos(2..3),
        )
        .expect("apply");
        tree.lookup(ROOT_INO, "a").expect("lookup");
        tree.apply_listing(ROOT_INO, &[], &mut Vec::new())
            .expect("apply");
        assert!(tree.get(2).is_some_and(|node| !node.attached));
        assert!(tree.forget(2, 1));
    }

    #[test]
    fn unrepresentable_and_duplicate_names_are_skipped() {
        let mut tree = Tree::with_root(root_state());
        let listing = [
            remote("a", "ok", NodeKind::File, 1),
            remote("b", "ok", NodeKind::File, 1),
            remote("c", "..", NodeKind::File, 1),
            remote("d", &"x".repeat(256), NodeKind::File, 1),
        ];
        tree.apply_listing(ROOT_INO, &listing, &mut inos(2..6))
            .expect("apply");
        assert_eq!(tree.get(ROOT_INO).expect("root").children.len(), 1);
    }

    #[test]
    fn missing_inode_numbers_are_requested_without_changes() {
        let mut tree = Tree::with_root(root_state());
        let listing = [
            remote("a", "a", NodeKind::File, 1),
            remote("b", "b", NodeKind::File, 1),
        ];
        assert_eq!(
            tree.apply_listing(ROOT_INO, &listing, &mut inos(2..3)),
            Err(ApplyError::NeedInos(1))
        );
        assert!(!tree.get(ROOT_INO).expect("root").listed);
    }

    #[test]
    fn rows_round_trip_with_local_nodes_and_tombstones() {
        let mut tree = Tree::with_root(root_state());
        tree.apply_listing(
            ROOT_INO,
            &[
                remote("d", "d", NodeKind::Folder, 0),
                remote("f", "f", NodeKind::File, 1),
            ],
            &mut inos(2..4),
        )
        .expect("apply");
        tree.create(ROOT_INO, "new", NodeKind::File, 0o600, now(), 10)
            .expect("create");
        tree.remove_entry(ROOT_INO, "f", false, now())
            .expect("unlink");
        let mut rows: Vec<NodeRow> = tree.inos().filter_map(|ino| tree.row(ino)).collect();
        rows.push(NodeRow {
            ino: 9,
            kind: NodeKind::File,
            parent: Some(2), // d is not listed, so this row is stale
            name: Some("stale".into()),
            size: 0,
            mtime: UNIX_EPOCH,
            ctime: UNIX_EPOCH,
            atime: None,
            mode: 0o644,
            listed: false,
            local_only: false,
            deleted: false,
            gens: Gens::default(),
            deferred_error: None,
            remote: None,
        });
        let (mut loaded, stale) = Tree::from_rows(rows);
        assert_eq!(stale, vec![9]);
        assert!(matches!(loaded.lookup(ROOT_INO, "d"), Ok(Lookup::Found(attr)) if attr.ino == 2));
        let new = loaded.get(10).expect("local node");
        assert!(new.remote.is_none() && new.attached && new.mode == 0o600);
        let tombstone = loaded.get(3).expect("tombstone");
        assert!(tombstone.deleted && !tombstone.attached);
        assert_eq!(loaded.by_link(&LinkId::new("f")), Some(3));
        assert_eq!(
            loaded.lookup(ROOT_INO, "f").expect("lookup"),
            Lookup::Negative
        );
    }

    #[test]
    fn listings_keep_local_changes() {
        let mut tree = Tree::with_root(root_state());
        tree.apply_listing(
            ROOT_INO,
            &[
                remote("a", "a", NodeKind::File, 1),
                remote("b", "b", NodeKind::File, 1),
            ],
            &mut inos(2..4),
        )
        .expect("apply");
        tree.create(ROOT_INO, "new", NodeKind::File, 0o644, now(), 10)
            .expect("create");
        tree.rename(ROOT_INO, "a", ROOT_INO, "a2", RenameMode::Replace, now())
            .expect("rename");
        tree.remove_entry(ROOT_INO, "b", false, now())
            .expect("unlink");
        // The server still has a and b under their old names.
        tree.apply_listing(
            ROOT_INO,
            &[
                remote("a", "a", NodeKind::File, 1),
                remote("b", "b", NodeKind::File, 1),
            ],
            &mut Vec::new(),
        )
        .expect("relist");
        let root = tree.get(ROOT_INO).expect("root");
        let names: Vec<&str> = root.children.keys().map(String::as_str).collect();
        assert_eq!(names, ["a2", "new"]);
        assert!(tree.placement_pending(2));
        assert!(tree.get(3).expect("b").deleted);
    }

    #[test]
    fn a_remote_removal_keeps_locally_changed_subtrees() {
        let mut tree = Tree::with_root(root_state());
        tree.apply_listing(
            ROOT_INO,
            &[remote("d", "d", NodeKind::Folder, 0)],
            &mut inos(2..3),
        )
        .expect("root");
        tree.apply_listing(2, &[], &mut Vec::new()).expect("d");
        tree.create(2, "pending", NodeKind::File, 0o644, now(), 10)
            .expect("create");
        tree.apply_listing(ROOT_INO, &[], &mut Vec::new())
            .expect("relist");
        let d = tree.get(2).expect("d");
        assert!(d.attached && d.remote.is_none(), "kept for re-creation");
        assert_eq!(tree.child(2, "pending").expect("child"), Some(10));
    }

    #[test]
    fn rename_follows_posix_rules() {
        let mut tree = Tree::with_root(root_state());
        tree.apply_listing(
            ROOT_INO,
            &[
                remote("d", "d", NodeKind::Folder, 0),
                remote("e", "e", NodeKind::Folder, 0),
                remote("f", "f", NodeKind::File, 1),
                remote("g", "g", NodeKind::File, 1),
            ],
            &mut inos(2..6),
        )
        .expect("apply");
        tree.apply_listing(2, &[], &mut Vec::new()).expect("d");
        tree.apply_listing(3, &[], &mut Vec::new()).expect("e");
        let rename = |tree: &mut Tree, from: &str, to: &str, mode| {
            tree.rename(ROOT_INO, from, ROOT_INO, to, mode, now())
        };
        assert!(matches!(
            rename(&mut tree, "f", "g", RenameMode::NoReplace),
            Err(CoreError::Exists)
        ));
        assert!(matches!(
            rename(&mut tree, "f", "d", RenameMode::Replace),
            Err(CoreError::IsADirectory)
        ));
        assert!(matches!(
            rename(&mut tree, "d", "f", RenameMode::Replace),
            Err(CoreError::NotADirectory)
        ));
        assert!(matches!(
            tree.rename(ROOT_INO, "d", 2, "x", RenameMode::Replace, now()),
            Err(CoreError::InvalidArgument)
        ));
        let same = rename(&mut tree, "f", "f", RenameMode::Replace).expect("same");
        assert_eq!(same.replaced, None);
        let over = rename(&mut tree, "f", "g", RenameMode::Replace).expect("over");
        assert_eq!(over.replaced, Some(5));
        assert!(tree.get(5).expect("g").deleted);
        let dir_over =
            rename(&mut tree, "d", "e", RenameMode::Replace).expect("dir over empty dir");
        assert_eq!(dir_over.replaced, Some(3));
        tree.create(3, "x", NodeKind::File, 0o644, now(), 10)
            .expect_err("e is detached");
    }

    #[test]
    fn rmdir_needs_an_empty_listed_directory() {
        let mut tree = Tree::with_root(root_state());
        tree.apply_listing(
            ROOT_INO,
            &[remote("d", "d", NodeKind::Folder, 0)],
            &mut inos(2..3),
        )
        .expect("apply");
        assert!(matches!(
            tree.remove_entry(ROOT_INO, "d", true, now()),
            Err(CoreError::NeedsListing(2))
        ));
        tree.apply_listing(2, &[], &mut Vec::new()).expect("d");
        tree.create(2, "x", NodeKind::Folder, 0o755, now(), 10)
            .expect("mkdir");
        assert!(matches!(
            tree.remove_entry(ROOT_INO, "d", true, now()),
            Err(CoreError::NotEmpty)
        ));
        tree.remove_entry(2, "x", true, now()).expect("rmdir x");
        assert!(!tree.is_droppable(10), "x still has its lookup");
        tree.remove_entry(ROOT_INO, "d", true, now())
            .expect("rmdir d");
    }

    #[test]
    fn name_validation() {
        assert!(validate_name(b"ok").is_ok());
        assert!(matches!(
            validate_name(&[0xff]),
            Err(CoreError::InvalidName)
        ));
        assert!(matches!(
            validate_name(&[b'x'; 256]),
            Err(CoreError::NameTooLong)
        ));
        assert!(validate_name(b"..").is_err());
    }

    #[test]
    fn time_conversion_round_trips() {
        let time = UNIX_EPOCH + Duration::from_nanos(1_700_000_000_123_456_789);
        assert_eq!(ns_to_system_time(system_time_to_ns(time)), time);
        let before = UNIX_EPOCH - Duration::from_secs(5);
        assert_eq!(ns_to_system_time(system_time_to_ns(before)), before);
    }
}
