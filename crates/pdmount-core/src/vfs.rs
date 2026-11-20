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

use crate::conflicts::{ConflictKind, ConflictNote, Namer};
use crate::errors::CoreError;
use pdmount_backend::{LinkId, NameHash, NodeKind, RemoteNode, RevisionId, Unverified};
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
    /// Signatures that did not verify at the last decode (D132); kept in
    /// the store's `signatures` table.
    pub unverified: Option<Unverified>,
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
            unverified: node.unverified.clone(),
        }
    }

    /// This state with the placement (parent, name, name hash) of `fresh`,
    /// which is the same node. The content fields stay: a newer revision is
    /// a matter of the upload rules. So do the failed signatures: only a
    /// whole decode ([`RemoteState::from_remote`]) replaces them.
    pub fn with_placement_of(&self, fresh: &RemoteNode) -> Self {
        Self {
            parent_link: fresh.parent_link_id.clone(),
            name: fresh.name.clone(),
            name_hash: fresh.name_hash.clone(),
            ..self.clone()
        }
    }

    /// This state with the content fields (revision, size, mtime, mode,
    /// SHA-1) of `fresh`, which is the same file. Parent and name stay: a
    /// move or rename of the node is a matter of the namespace rules.
    pub fn with_content_of(&self, fresh: &RemoteNode) -> Self {
        Self {
            rev: fresh.active_revision.clone(),
            size: fresh.size.unwrap_or(0),
            mtime: fresh.mtime.unwrap_or(UNIX_EPOCH),
            mode: fresh.mode.map(|mode| mode & 0o7777),
            sha1: fresh.sha1,
            name_hash: fresh.name_hash.clone().or_else(|| self.name_hash.clone()),
            ..self.clone()
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

impl Content {
    /// The content of a remote node's active revision.
    pub fn of(remote: &RemoteState) -> Self {
        Self::Remote {
            link: remote.link.clone(),
            rev: remote.rev.clone(),
        }
    }
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
    /// A newer remote revision exists than the one this node's dirty content
    /// is based on (§10.2); persisted so a restart does not forget it.
    pub remote_newer: bool,
    /// A listing left a remote node out because this node held its name.
    /// When this node leaves the name, the directory is listed again.
    /// Memory only: after a restart every created, not yet uploaded node
    /// counts, since the listing that hid the remote node is forgotten.
    pub clashed: bool,
    /// Detached because the remote removed it (a trash, a move this tree
    /// cannot follow), not by a local change: a write through a handle
    /// still open brings it back (§10.5, D128). Memory only, as are the
    /// handles.
    pub removed_remotely: bool,
    pub open_handles: u32,
    pub open_writers: u32,
    /// Bumped when remote content replaces what the kernel may have cached.
    pub content_epoch: u64,
    /// `content_epoch` at the last open (`FOPEN_KEEP_CACHE`).
    pub served_epoch: Option<u64>,
    /// When the executor last changed or freshly read this node remotely
    /// (engine clock). A listing fetched before that must not touch the node.
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
            remote_newer: false,
            clashed: false,
            removed_remotely: false,
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
        node.content = Content::of(&remote);
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
    pub remote_newer: bool,
    pub remote: Option<RemoteState>,
}

/// Effects of a tree change: rows to rewrite or delete, kernel caches to
/// drop.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Changes {
    pub changed: Vec<Ino>,
    pub removed: Vec<Ino>,
    pub invalidations: Vec<Invalidation>,
    /// Nodes whose remote counterpart is gone (resolved by the engine, §10.5).
    pub vanished: Vec<Ino>,
    /// Nodes that lost a name to another node (resolved lazily, §10.5).
    pub clashes: Vec<Ino>,
    /// Conflicts found while applying the change, for the registry (§10.8).
    pub notes: Vec<ConflictNote>,
    /// Dirty nodes whose remote file has a newer revision than their base.
    pub remote_newer: Vec<Ino>,
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenameMode {
    Replace,
    NoReplace,
}

/// The result of [`Tree::resurrect_minimal`].
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Resurrected {
    pub changes: Changes,
    /// Nodes kept for re-creation, parents before children.
    pub kept: Vec<Ino>,
    /// Removed nodes whose local placement differed from the remote one: a
    /// clean rename lost against the remote delete (§10.5).
    pub removed_renamed: Vec<Ino>,
}

fn push_unique<T: PartialEq>(list: &mut Vec<T>, item: T) {
    if !list.contains(&item) {
        list.push(item);
    }
}

/// Effects of tree mutations, collected while they happen (`Tree::mutate`;
/// a listing finishes its own after its checks):
/// rows to write, the directory entries that changed and the nodes whose
/// attributes changed. [`Effects::finish`] compares each touched entry with
/// its state before the first change and derives the invalidations from
/// that (§9.5), so no mutation builds them by hand.
#[derive(Debug, Default)]
struct Effects {
    changes: Changes,
    dirs: Vec<Touched>,
    attrs: Vec<Ino>,
}

/// The entries of one directory that a mutation touched.
#[derive(Debug)]
struct Touched {
    dir: Ino,
    /// Listed before the first change: only then can the kernel have cached
    /// its entries.
    listed: bool,
    /// Touched names, in order, each with its node before the first change.
    names: Vec<(String, Option<Ino>)>,
    seen: HashSet<String>,
    /// All entries before a listing (which takes them and puts back the
    /// result); replaces `names`.
    all_before: Option<BTreeMap<String, Ino>>,
}

impl Effects {
    /// A row to write.
    fn row(&mut self, ino: Ino) {
        self.changes.changed.push(ino);
    }

    /// The node's attributes changed.
    fn attrs(&mut self, ino: Ino) {
        self.attrs.push(ino);
    }

    fn touched(&mut self, tree: &Tree, dir: Ino) -> &mut Touched {
        let at = match self.dirs.iter().position(|touched| touched.dir == dir) {
            Some(at) => at,
            None => {
                self.dirs.push(Touched {
                    dir,
                    listed: tree.nodes.get(&dir).is_some_and(|node| node.listed),
                    names: Vec::new(),
                    seen: HashSet::new(),
                    all_before: None,
                });
                self.dirs.len() - 1
            }
        };
        &mut self.dirs[at]
    }

    /// `(dir, name)` is about to change.
    fn entry(&mut self, tree: &Tree, dir: Ino, name: &str) {
        let before = tree
            .nodes
            .get(&dir)
            .and_then(|node| node.children.get(name))
            .copied();
        let touched = self.touched(tree, dir);
        if touched.all_before.is_none() && touched.seen.insert(name.to_owned()) {
            touched.names.push((name.to_owned(), before));
        }
    }

    /// A listing replaced all entries of `dir`; `before` held them.
    fn listing(&mut self, tree: &Tree, dir: Ino, listed: bool, before: BTreeMap<String, Ino>) {
        let touched = self.touched(tree, dir);
        touched.listed = listed;
        touched.names.clear();
        touched.seen.clear();
        touched.all_before = Some(before);
    }

    /// The changes with their invalidations: per listed directory that is
    /// still attached, one per changed name (`Delete` when the node that
    /// was there left the namespace, else `Entry`) and then the directory
    /// itself (its pages are cached, `FOPEN_CACHE_DIR`); then the nodes
    /// whose attributes changed. Rows and invalidations are unique, in the
    /// order they were first recorded.
    fn finish(mut self, tree: &Tree) -> Changes {
        let mut out = Vec::new();
        for touched in &self.dirs {
            let Some(dir) = tree
                .nodes
                .get(&touched.dir)
                .filter(|node| node.attached && touched.listed)
            else {
                continue;
            };
            let start = out.len();
            let mut entry = |name: &str, before: Option<Ino>| {
                if dir.children.get(name).copied() == before {
                    return;
                }
                let gone =
                    before.filter(|child| !tree.nodes.get(child).is_some_and(|n| n.attached));
                out.push(match gone {
                    Some(child) => Invalidation::Delete {
                        parent: touched.dir,
                        child,
                        name: name.to_owned(),
                    },
                    None => Invalidation::Entry {
                        parent: touched.dir,
                        name: name.to_owned(),
                    },
                });
            };
            match &touched.all_before {
                Some(all) => {
                    for (name, &ino) in all {
                        entry(name, Some(ino));
                    }
                    for name in dir.children.keys().filter(|name| !all.contains_key(*name)) {
                        entry(name, None);
                    }
                }
                None => {
                    for (name, before) in &touched.names {
                        entry(name, *before);
                    }
                }
            }
            if out.len() > start {
                out.push(Invalidation::Inode { ino: touched.dir });
            }
        }
        let mut seen: HashSet<Ino> = out
            .iter()
            .filter_map(|invalidation| match invalidation {
                Invalidation::Inode { ino } => Some(*ino),
                _ => None,
            })
            .collect();
        for &ino in &self.attrs {
            if seen.insert(ino) {
                out.push(Invalidation::Inode { ino });
            }
        }
        self.changes.invalidations = out;
        let mut seen = HashSet::new();
        self.changes.changed.retain(|ino| seen.insert(*ino));
        self.changes
    }
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
    /// Directories to list again: a node that held back a remote node by
    /// its name left it.
    relist: Vec<Ino>,
    /// Names conflict copies (§10.7).
    namer: Namer,
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
            node.remote_newer = row.remote_newer;
            node.attached = attached;
            node.clashed =
                attached && row.ino != ROOT_INO && row.remote.is_none() && !row.local_only;
            node.content = match &row.remote {
                Some(remote) => Content::of(remote),
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

    /// [`Tree::set_remote`] with a state the engine has just written or read
    /// fresh, at `stamp` (`Engine::stamp`): a listing fetched before it
    /// leaves the node alone instead of reverting it.
    pub fn set_remote_at(&mut self, ino: Ino, remote: Option<RemoteState>, stamp: u64) {
        self.set_remote(ino, remote);
        if let Some(node) = self.nodes.get_mut(&ino) {
            node.remote_stamp = node.remote_stamp.max(stamp);
        }
    }

    /// Runs a mutation that records its effects in an [`Effects`], and
    /// returns its result with the [`Changes`] they amount to.
    fn mutate<T>(&mut self, f: impl FnOnce(&mut Self, &mut Effects) -> T) -> (T, Changes) {
        let mut fx = Effects::default();
        let out = f(self, &mut fx);
        (out, fx.finish(self))
    }

    /// Puts `ino` at `name` in `dir`, which holds no other node there.
    fn place(&mut self, fx: &mut Effects, ino: Ino, dir: Ino, name: &str) {
        fx.entry(self, dir, name);
        if let Some(dir_node) = self.nodes.get_mut(&dir) {
            dir_node.children.insert(name.to_owned(), ino);
        }
        if let Some(node) = self.nodes.get_mut(&ino) {
            node.parent = dir;
            node.name = name.to_owned();
            node.removed_remotely = false;
        }
        fx.row(ino);
    }

    /// Takes `ino` out of its parent's entries, if it is there (the caller
    /// places it again or removes it).
    fn unplace(&mut self, fx: &mut Effects, ino: Ino) {
        let Some(node) = self.nodes.get(&ino) else {
            return;
        };
        let (parent, name) = (node.parent, node.name.clone());
        if self
            .nodes
            .get(&parent)
            .is_some_and(|dir| dir.children.get(&name) == Some(&ino))
        {
            fx.entry(self, parent, &name);
            self.nodes
                .get_mut(&parent)
                .expect("checked")
                .children
                .remove(&name);
        }
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
                    && known.unverified == remote.unverified
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

    pub fn set_namer(&mut self, namer: Namer) {
        self.namer = namer;
    }

    /// The first conflict name for `name` in `parent` that is free locally,
    /// remotely and for the caller's `taken` (§10.7).
    pub fn pick_conflict_name(
        &self,
        parent: Ino,
        name: &str,
        taken: &dyn Fn(&str) -> bool,
    ) -> String {
        let dir = self.nodes.get(&parent);
        let link = self.link_of(parent);
        (1..)
            .map(|attempt| self.namer.name(name, attempt))
            .find(|candidate| {
                dir.is_none_or(|dir| !dir.children.contains_key(candidate.as_str()))
                    && link.is_none_or(|link| self.remote_occupant(link, candidate).is_none())
                    && !taken(candidate)
            })
            .expect("an unbounded range has a free name")
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

    /// Hot-path lookup. Increments `nlookup` on success; `NeedsListing`
    /// when the parent was never listed.
    pub fn lookup(&mut self, parent: Ino, name: &str) -> Result<Lookup> {
        let dir = self.nodes.get(&parent).ok_or(CoreError::NotFound)?;
        if !dir.is_dir() {
            return Err(CoreError::NotADirectory);
        }
        if !dir.listed {
            return Err(CoreError::NeedsListing(parent));
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
    pub fn dir_entries(&self, ino: Ino) -> Result<Vec<DirEntry>> {
        let dir = self.nodes.get(&ino).ok_or(CoreError::NotFound)?;
        if !dir.is_dir() {
            return Err(CoreError::NotADirectory);
        }
        if !dir.listed {
            return Err(CoreError::NeedsListing(ino));
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
        Ok(entries)
    }

    /// Number of children of a listing that need an inode: those that have
    /// none yet, and the tombstones a listing revives (it may count more
    /// than it needs: leftovers are dropped).
    pub fn unknown_count(&self, children: &[RemoteNode]) -> usize {
        children
            .iter()
            .filter(|child| {
                self.by_link
                    .get(&child.link_id)
                    .is_none_or(|&ino| self.revives(ino, child))
            })
            .count()
    }

    /// Whether the listed `child` brings the tombstone `ino` back: the file
    /// was deleted locally and has another revision remotely (§10.5).
    fn revives(&self, ino: Ino, child: &RemoteNode) -> bool {
        self.nodes.get(&ino).is_some_and(|node| {
            node.deleted
                && node.kind == NodeKind::File
                && (node.remote_newer
                    || node
                        .remote
                        .as_ref()
                        .is_some_and(|remote| remote.rev != child.active_revision))
        })
    }

    /// The nodes at and below `top`, each before its children.
    pub fn subtree(&self, top: Ino) -> impl Iterator<Item = Ino> + '_ {
        let mut stack = vec![top];
        std::iter::from_fn(move || {
            loop {
                let current = stack.pop()?;
                if let Some(node) = self.nodes.get(&current) {
                    stack.extend(node.children.values().copied());
                    return Some(current);
                }
            }
        })
    }

    /// Whether the node has a change the journal has to send: a delete, new
    /// content or metadata, or a placement that differs from the remote one.
    pub fn has_pending_work(&self, ino: Ino) -> bool {
        self.nodes
            .get(&ino)
            .is_some_and(|node| node.deleted || node.content_pending() || node.meta_pending())
            || self.placement_pending(ino)
    }

    /// Whether the node or anything below it has a local change that is not
    /// uploaded yet.
    pub fn subtree_has_pending(&self, ino: Ino) -> bool {
        self.subtree(ino).any(|current| {
            let node = &self.nodes[&current];
            node.local_only
                || node.content_pending()
                || node.meta_pending()
                || self.placement_pending(current)
        })
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

    /// The nodes at or below `top` whose local placement differs from the
    /// remote one, with their local name and link, parents first.
    pub fn renamed_in(&self, top: Ino) -> Vec<(Ino, String, Option<LinkId>)> {
        self.subtree(top)
            .filter_map(|current| {
                let node = &self.nodes[&current];
                (node.remote.is_some() && self.placement_pending(current))
                    .then(|| (current, node.name.clone(), node.link().cloned()))
            })
            .collect()
    }

    /// Attached nodes below `links` remotely, that is, whose known remote
    /// parent is one of `links`, and that have local changes: the nodes
    /// that were moved out of a removed folder (or are in it).
    pub fn pending_under_remote(&self, links: &HashSet<LinkId>) -> Vec<Ino> {
        let mut out: Vec<Ino> = self
            .nodes
            .values()
            .filter(|node| {
                node.attached
                    && node
                        .remote
                        .as_ref()
                        .and_then(|remote| remote.parent_link.as_ref())
                        .is_some_and(|parent| links.contains(parent))
                    && self.subtree_has_pending(node.ino)
            })
            .map(|node| node.ino)
            .collect();
        out.sort_unstable();
        out
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
    #[cfg(test)]
    pub fn apply_listing(
        &mut self,
        dir: Ino,
        children: &[RemoteNode],
        new_inos: &mut Vec<Ino>,
    ) -> std::result::Result<Changes, ApplyError> {
        self.apply_listing_at(dir, children, &[], new_inos, u64::MAX)
    }

    /// [`Tree::apply_listing`] for a listing fetched at clock `fetched_at`:
    /// nodes the executor changed since, and links it trashed since, are
    /// left alone, because the listing does not show those changes yet.
    /// Known children whose link is `degraded` (listed, but their metadata
    /// cannot be decrypted) stay as they are: degraded is not absent (D132).
    pub fn apply_listing_at(
        &mut self,
        dir: Ino,
        children: &[RemoteNode],
        degraded: &[LinkId],
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
        // Tombstones whose remote file got another revision: the local
        // delete loses against the edit (§10.5). They are unbound below and
        // the link comes back as a new inode.
        let revived: Vec<(Ino, &RemoteNode)> = incoming
            .iter()
            .filter_map(|&child| {
                let ino = *self.by_link.get(&child.link_id)?;
                (self.revives(ino, child) && !changed_since(self, ino)).then_some((ino, child))
            })
            .collect();
        let unknown = incoming
            .iter()
            .filter(|child| !self.by_link.contains_key(&child.link_id))
            .count()
            + revived.len();
        if unknown > new_inos.len() {
            return Err(ApplyError::NeedInos(unknown - new_inos.len()));
        }
        let incoming_links: HashMap<&LinkId, &RemoteNode> =
            incoming.iter().map(|c| (&c.link_id, *c)).collect();
        let incoming_names: HashSet<&str> = incoming.iter().map(|&c| c.name.as_str()).collect();

        let mut fx = Effects::default();
        for &(ino, child) in &revived {
            let local_rev = self
                .nodes
                .get(&ino)
                .and_then(|node| node.remote.as_ref())
                .and_then(|remote| remote.rev.clone());
            if let Some(unbound) = self.unbind_tombstone(ino) {
                fx.changes.removed.extend(unbound.removed);
            }
            fx.changes.notes.push(ConflictNote {
                link: Some(child.link_id.clone()),
                local_rev,
                remote_rev: child.active_revision.clone(),
                ..ConflictNote::new(ConflictKind::LocalDeleteRemoteEdit, ino, child.name.clone())
            });
        }
        let old_children = std::mem::take(&mut self.nodes.get_mut(&dir).expect("checked").children);
        // Whether each known node follows its remote placement: it has no
        // local placement change of its own, or the remote moved it too (it
        // committed first and wins, §10.5).
        let mut follows: HashSet<Ino> = HashSet::new();
        let mut contested: HashSet<Ino> = HashSet::new();
        for child in &incoming {
            let Some(&ino) = self.by_link.get(&child.link_id) else {
                continue;
            };
            let Some(node) = self.nodes.get(&ino) else {
                continue;
            };
            if !node.attached
                || node.deleted
                || changed_since(self, ino)
                || self.is_within(dir, ino)
            {
                continue;
            }
            // At its own temporary name in its known folder: this device's
            // move aside, whose reply was lost (D117 amended, V-7). Only the
            // remote state follows; the journal goes on to the local name.
            if child.name == crate::journal::swap_name(ino)
                && node
                    .remote
                    .as_ref()
                    .is_some_and(|remote| remote.parent_link == child.parent_link_id)
            {
                continue;
            }
            if !self.placement_pending(ino) {
                follows.insert(ino);
            } else if node.remote.as_ref().is_some_and(|remote| {
                (&remote.parent_link, &remote.name) != (&child.parent_link_id, &child.name)
            }) {
                follows.insert(ino);
                contested.insert(ino);
            }
        }

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
                Some(remote)
                    if !incoming_links.contains_key(&remote.link)
                        && !degraded.contains(&remote.link) =>
                {
                    if self.subtree_has_pending(ino) {
                        // Gone remotely (or moved) with local changes
                        // inside: the engine decides which (§10.5).
                        kept.insert(name.clone(), ino);
                        fx.changes.vanished.push(ino);
                    } else {
                        // A node inside that the remote moved here comes
                        // with the incoming pass: take it out first.
                        let movers: Vec<Ino> = self
                            .subtree(ino)
                            .filter(|inner| follows.contains(inner))
                            .collect();
                        for mover in movers {
                            self.unplace(&mut fx, mover);
                        }
                        self.remove_subtree(&mut fx, ino);
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
                    if follows.contains(&ino)
                        && let Some(&occupant) = final_children.get(&child.name)
                    {
                        self.lose_name(
                            dir,
                            ino,
                            occupant,
                            child,
                            &mut final_children,
                            &incoming_names,
                            &mut fx,
                        );
                        continue;
                    }
                    // Layer 1 of §10.4: the remote file moved on while this
                    // one has unuploaded content.
                    if let Some(node) = self.nodes.get_mut(&ino)
                        && node.kind == NodeKind::File
                        && node.content_pending()
                        && node
                            .remote
                            .as_ref()
                            .is_some_and(|known| known.rev != remote.rev)
                    {
                        node.remote_newer = true;
                        fx.changes.remote_newer.push(ino);
                        fx.changes.changed.push(ino);
                    }
                    let signatures_changed = self
                        .nodes
                        .get(&ino)
                        .and_then(|node| node.remote.as_ref())
                        .is_none_or(|known| known.unverified != remote.unverified);
                    self.set_remote(ino, Some(remote.clone()));
                    self.refresh_attrs(ino, &remote, &mut fx);
                    if signatures_changed {
                        // Its row carries them (D132).
                        push_unique(&mut fx.changes.changed, ino);
                    }
                    if !follows.contains(&ino) {
                        // A tombstone, or placed locally elsewhere.
                        continue;
                    }
                    let (old_parent, old_name) = {
                        let node = &self.nodes[&ino];
                        (node.parent, node.name.clone())
                    };
                    if contested.contains(&ino) && (old_parent, &old_name) != (dir, &child.name) {
                        fx.changes.notes.push(ConflictNote {
                            link: Some(child.link_id.clone()),
                            remote_rev: child.active_revision.clone(),
                            detail: Some(format!("kept {} from another device", child.name)),
                            ..ConflictNote::new(ConflictKind::RenameRename, ino, old_name.clone())
                        });
                    }
                    if old_parent != dir {
                        self.unplace(&mut fx, ino);
                    }
                    let node = self.nodes.get_mut(&ino).expect("indexed");
                    node.parent = dir;
                    node.name = child.name.clone();
                    final_children.insert(child.name.clone(), ino);
                    fx.changes.changed.push(ino);
                }
                None => {
                    let mut yielded = None;
                    if let Some(&occupant) = final_children.get(&child.name) {
                        if !self
                            .nodes
                            .get(&occupant)
                            .is_some_and(|node| node.local_only)
                        {
                            // Resolved lazily: identical content never
                            // yields (D016, §10.6).
                            tracing::debug!(
                                name = child.name,
                                "a new remote node has a locally used name; not shown"
                            );
                            push_unique(&mut fx.changes.clashes, occupant);
                            if let Some(node) = self.nodes.get_mut(&occupant) {
                                node.clashed = true;
                            }
                            continue;
                        }
                        // A file that never uploads gives way at once.
                        let new_name = self.pick_conflict_name(dir, &child.name, &|name| {
                            final_children.contains_key(name) || incoming_names.contains(name)
                        });
                        let now = (self.namer.clock)();
                        final_children.remove(&child.name);
                        final_children.insert(new_name.clone(), occupant);
                        if let Some(node) = self.nodes.get_mut(&occupant) {
                            node.name = new_name.clone();
                            node.ctime = now;
                        }
                        yielded = Some((occupant, new_name));
                    }
                    let ino = new_inos.pop().expect("counted above");
                    self.insert(Node::from_remote(ino, dir, remote, child.kind));
                    final_children.insert(child.name.clone(), ino);
                    fx.changes.changed.push(ino);
                    if let Some((occupant, new_name)) = yielded {
                        fx.changes.notes.push(ConflictNote {
                            other_ino: Some(ino),
                            link: Some(child.link_id.clone()),
                            conflict_name: Some(new_name),
                            remote_rev: child.active_revision.clone(),
                            ..ConflictNote::new(
                                ConflictKind::NameTaken,
                                occupant,
                                child.name.clone(),
                            )
                        });
                    }
                }
            }
        }
        for note in &mut fx.changes.notes {
            if note.kind == ConflictKind::LocalDeleteRemoteEdit
                && let Some(link) = &note.link
            {
                note.other_ino = self.by_link.get(link).copied();
            }
        }

        fx.listing(self, dir, was_listed, old_children);
        fx.changes.changed.extend(final_children.values().copied());
        fx.changes.changed.push(dir);
        fx.changes.changed.sort_unstable();
        let dir_node = self.nodes.get_mut(&dir).expect("checked");
        dir_node.children = final_children;
        dir_node.listed = true;
        Ok(fx.finish(self))
    }

    /// Takes over remote attributes and content where nothing local is
    /// pending.
    fn refresh_attrs(&mut self, ino: Ino, remote: &RemoteState, fx: &mut Effects) {
        let Some(node) = self.nodes.get_mut(&ino) else {
            return;
        };
        let mut changed = false;
        if node.kind == NodeKind::File
            && matches!(node.content, Content::Remote { .. })
            && !node.content_pending()
        {
            let content = Content::of(remote);
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
            fx.attrs(ino);
        }
    }

    /// The remote moved `ino` to `child.name`, which a local node holds
    /// (`occupant`): the occupant keeps the name and is reported as a clash.
    /// A node with nothing to upload is detached, and its link comes back as
    /// a new inode once the name is free. Anything else becomes a new local
    /// node that is uploaded again, under its old name or a conflict name,
    /// with only its pending items: the clean rest is still in the remote
    /// folder and comes back with it.
    #[allow(clippy::too_many_arguments)]
    fn lose_name(
        &mut self,
        dir: Ino,
        ino: Ino,
        occupant: Ino,
        child: &RemoteNode,
        final_children: &mut BTreeMap<String, Ino>,
        incoming_names: &HashSet<&str>,
        fx: &mut Effects,
    ) {
        push_unique(&mut fx.changes.clashes, occupant);
        if let Some(node) = self.nodes.get_mut(&occupant) {
            node.clashed = true;
        }
        let (old_parent, old_name) = {
            let node = &self.nodes[&ino];
            (node.parent, node.name.clone())
        };
        if !self.subtree_has_pending(ino) {
            self.remove_subtree(fx, ino);
            return;
        }
        let renamed = self.renamed_in(ino);
        let removed_before = fx.changes.removed.len();
        let (kept, removed_renamed) = self.keep_pending(fx, ino, false);
        for lost in &removed_renamed {
            let Some((_, name, link)) = renamed.iter().find(|(renamed, ..)| renamed == lost) else {
                continue;
            };
            fx.changes.notes.push(ConflictNote {
                link: link.clone(),
                detail: Some(
                    "renamed here inside a folder renamed on another device; the remote placement won"
                        .to_owned(),
                ),
                ..ConflictNote::new(ConflictKind::RenameRename, *lost, name.clone())
            });
        }
        let detail = format!(
            "renamed remotely onto a name used locally; kept {}, removed {}",
            kept.len(),
            fx.changes.removed.len() - removed_before
        );
        if !kept.contains(&ino) {
            // Only renames inside it were pending; they lose, as above.
            return;
        }
        let mut final_name = old_name.clone();
        if old_parent == dir {
            if final_children.contains_key(&old_name) || incoming_names.contains(old_name.as_str())
            {
                final_name = self.pick_conflict_name(dir, &old_name, &|name| {
                    final_children.contains_key(name) || incoming_names.contains(name)
                });
                self.nodes.get_mut(&ino).expect("checked").name = final_name.clone();
            }
            final_children.insert(final_name.clone(), ino);
        }
        fx.changes.notes.push(ConflictNote {
            other_ino: Some(occupant),
            link: Some(child.link_id.clone()),
            conflict_name: Some(final_name),
            remote_rev: child.active_revision.clone(),
            detail: Some(detail),
            ..ConflictNote::new(ConflictKind::NameTaken, ino, child.name.clone())
        });
    }

    pub fn row(&self, ino: Ino) -> Option<NodeRow> {
        let node = self.nodes.get(&ino)?;
        let attached = node.attached && ino != ROOT_INO;
        Some(NodeRow {
            ino,
            kind: node.kind,
            parent: attached.then_some(node.parent),
            // A tombstone keeps its name for `queue` (D121).
            name: (attached || node.deleted).then(|| node.name.clone()),
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
            remote_newer: node.remote_newer,
            remote: node.remote.clone(),
        })
    }

    /// Whether a node should have a store row: attached, or a tombstone.
    pub fn persisted(&self, ino: Ino) -> bool {
        self.nodes
            .get(&ino)
            .is_some_and(|node| node.attached || (node.deleted && node.remote.is_some()))
    }

    /// Removes a subtree that is gone remotely, taking its top out of its
    /// parent's entries. Nodes the kernel still references stay, detached,
    /// until forgotten.
    fn remove_subtree(&mut self, fx: &mut Effects, ino: Ino) {
        self.unplace(fx, ino);
        let mut stack = vec![ino];
        while let Some(current) = stack.pop() {
            let Some(node) = self.nodes.get_mut(&current) else {
                continue;
            };
            stack.extend(node.children.values().copied());
            node.children.clear();
            node.attached = false;
            node.listed = false;
            node.removed_remotely = true;
            fx.changes.removed.push(current);
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
        self.name_freed(ino, parent);
        Ok(ino)
    }

    /// `ino` left `dir`: if it kept a remote node of that name out of the
    /// listing, the directory must be listed again.
    fn name_freed(&mut self, ino: Ino, dir: Ino) {
        if let Some(node) = self.nodes.get_mut(&ino)
            && std::mem::take(&mut node.clashed)
        {
            push_unique(&mut self.relist, dir);
        }
    }

    /// Directories to list again after namespace changes.
    pub fn take_relist(&mut self) -> Vec<Ino> {
        std::mem::take(&mut self.relist)
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
            self.name_freed(target, new_parent);
        }
        self.name_freed(ino, parent);
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

// ---- conflict resolution primitives (§10.5) ----

impl Tree {
    /// Edit/edit: the dirty local file `ino` becomes the conflict copy and
    /// the server's version `server` takes its name as a new inode `new_ino`
    /// (§10.5). `None` when `ino` is not an attached file bound to
    /// `server.link`. The server version is inserted only into a listed
    /// folder that has no node of that name; otherwise the next listing
    /// brings it.
    pub fn split_conflict(
        &mut self,
        ino: Ino,
        server: RemoteState,
        new_ino: Ino,
        now: SystemTime,
    ) -> Option<(Changes, ConflictNote)> {
        let node = self.nodes.get(&ino)?;
        if ino == ROOT_INO
            || !node.attached
            || node.kind != NodeKind::File
            || node.remote.as_ref().map(|remote| &remote.link) != Some(&server.link)
        {
            return None;
        }
        let local_rev = node.remote.as_ref().and_then(|remote| remote.rev.clone());
        let (note, changes) = self.mutate(|tree, fx| {
            let (name, conflict_name) = tree.yield_name_in(fx, ino, now)?;
            tree.set_remote(ino, None);
            tree.nodes.get_mut(&ino).expect("checked").remote_newer = false;
            let mut note = ConflictNote {
                link: Some(server.link.clone()),
                conflict_name: Some(conflict_name),
                local_rev,
                remote_rev: server.rev.clone(),
                ..ConflictNote::new(ConflictKind::EditEdit, ino, name)
            };
            let target = server
                .parent_link
                .as_ref()
                .and_then(|link| tree.by_link(link))
                .filter(|&dir| {
                    representable(&server.name)
                        && tree
                            .listed_dir(dir)
                            .is_ok_and(|dir| !dir.children.contains_key(&server.name))
                });
            if let Some(dir) = target {
                let name = server.name.clone();
                tree.insert(Node::from_remote(new_ino, dir, server, NodeKind::File));
                tree.place(fx, new_ino, dir, &name);
                fx.row(dir);
                note.other_ino = Some(new_ino);
            }
            Some(note)
        });
        Some((changes, note?))
    }

    /// A local rename of `ino` within its parent to a free conflict name, so
    /// that another node may take the name. Returns the old and the new name.
    pub fn yield_name(&mut self, ino: Ino, now: SystemTime) -> Option<(Changes, String, String)> {
        let (names, changes) = self.mutate(|tree, fx| tree.yield_name_in(fx, ino, now));
        let (name, conflict_name) = names?;
        Some((changes, name, conflict_name))
    }

    fn yield_name_in(
        &mut self,
        fx: &mut Effects,
        ino: Ino,
        now: SystemTime,
    ) -> Option<(String, String)> {
        let node = self.nodes.get(&ino)?;
        if ino == ROOT_INO || !node.attached || !self.nodes.contains_key(&node.parent) {
            return None;
        }
        let parent = node.parent;
        let name = node.name.clone();
        let conflict_name = self.pick_conflict_name(parent, &name, &|_| false);
        self.unplace(fx, ino);
        self.place(fx, ino, parent, &conflict_name);
        self.nodes.get_mut(&ino).expect("checked").ctime = now;
        let dir = self.nodes.get_mut(&parent).expect("checked");
        dir.mtime = now;
        dir.ctime = now;
        fx.row(parent);
        Some((name, conflict_name))
    }

    /// Forgets the remote node of a tombstone (it is gone or taken over), so
    /// that its link is free. `None` when `ino` is not a tombstone.
    pub fn unbind_tombstone(&mut self, ino: Ino) -> Option<Changes> {
        let node = self.nodes.get_mut(&ino)?;
        if !node.deleted {
            return None;
        }
        node.deleted = false;
        self.set_remote(ino, None);
        if self.is_droppable(ino) {
            self.drop_node(ino);
        }
        Some(Changes {
            removed: vec![ino],
            ..Changes::default()
        })
    }

    /// Moves `ino` to where its remote node is: the directory of its remote
    /// parent, under its remote name. `None` when that is impossible: the
    /// directory is unknown, not listed or inside `ino`, or the name is
    /// taken by another node. An empty change when it is there already.
    pub fn adopt_remote_placement(&mut self, ino: Ino) -> Option<Changes> {
        let (adopted, changes) = self.mutate(|tree, fx| tree.adopt_remote_placement_in(fx, ino));
        adopted.map(|()| changes)
    }

    fn adopt_remote_placement_in(&mut self, fx: &mut Effects, ino: Ino) -> Option<()> {
        let node = self.nodes.get(&ino)?;
        if ino == ROOT_INO || !node.attached {
            return None;
        }
        let remote = node.remote.as_ref()?;
        let name = remote.name.clone();
        let dir = self.by_link(remote.parent_link.as_ref()?)?;
        let target = self.listed_dir(dir).ok()?;
        if !target.attached
            || self.is_within(dir, ino)
            || !representable(&name)
            || target.children.get(&name).is_some_and(|&held| held != ino)
        {
            return None;
        }
        if (node.parent, node.name.as_str()) != (dir, name.as_str()) {
            self.unplace(fx, ino);
            self.place(fx, ino, dir, &name);
        }
        Some(())
    }

    /// `queue drop` of a node that never reached the server: it leaves with
    /// its subtree (D061).
    pub fn remove_never_uploaded(&mut self, top: Ino) -> Changes {
        let ((), changes) = self.mutate(|tree, fx| {
            if let Some(parent) = tree.nodes.get(&top).map(|node| node.parent) {
                tree.name_freed(top, parent);
            }
            let subtree: Vec<Ino> = tree.subtree(top).collect();
            tree.remove_subtree(fx, top);
            // Removed on purpose: a write through an open handle does not
            // bring it back.
            for ino in subtree {
                if let Some(node) = tree.nodes.get_mut(&ino) {
                    node.removed_remotely = false;
                }
            }
        });
        changes
    }

    /// A file the remote removed (a clean node, so it was detached, not
    /// kept: §10.5) was written through a handle opened before: it comes
    /// back as a new local node at its old place, with the removed folders
    /// above it that are still in memory, each with only the chain to it
    /// (D128). The topmost goes into the nearest attached folder under its
    /// old name, or its conflict name when that is taken; into the root if
    /// that folder is no longer known. Returns the topmost node brought
    /// back. `None` if `ino` is attached or was removed locally.
    pub fn bring_back(&mut self, ino: Ino, now: SystemTime) -> Option<(Ino, Changes)> {
        let node = self.nodes.get(&ino)?;
        if ino == ROOT_INO || node.attached || !node.removed_remotely {
            return None;
        }
        let mut chain = vec![ino];
        let mut current = ino;
        let home = loop {
            let parent = self.nodes[&current].parent;
            match self.nodes.get(&parent) {
                Some(dir) if dir.attached && dir.kind == NodeKind::Folder => break parent,
                Some(dir)
                    if dir.removed_remotely
                        && dir.kind == NodeKind::Folder
                        && parent != ROOT_INO
                        && !chain.contains(&parent) =>
                {
                    chain.push(parent);
                    current = parent;
                }
                _ => break ROOT_INO,
            }
        };
        let top = *chain.last().expect("not empty");
        let ((), changes) = self.mutate(|tree, fx| {
            let name = tree.nodes[&top].name.clone();
            let taken = tree
                .nodes
                .get(&home)
                .is_some_and(|dir| dir.children.contains_key(&name))
                || tree
                    .link_of(home)
                    .is_some_and(|link| tree.remote_occupant(link, &name).is_some());
            let top_name = if taken {
                tree.pick_conflict_name(home, &name, &|_| false)
            } else {
                name
            };
            let mut dir = home;
            for (depth, &current) in chain.iter().rev().enumerate() {
                let name = if depth == 0 {
                    top_name.clone()
                } else {
                    tree.nodes[&current].name.clone()
                };
                tree.set_remote(current, None);
                let node = tree.nodes.get_mut(&current).expect("in the chain");
                node.attached = true;
                node.remote_newer = false;
                node.ctime = now;
                if node.kind == NodeKind::Folder {
                    // A new local folder: its entries are the chain.
                    node.listed = true;
                    node.children.clear();
                } else if matches!(node.content, Content::Remote { .. }) {
                    // Still readable from the old revision; staged before
                    // the upload.
                    node.gens.content = node.gens.uploaded_content + 1;
                }
                tree.place(fx, current, dir, &name);
                dir = current;
            }
            if let Some(dir) = tree.nodes.get_mut(&home) {
                dir.mtime = now;
                dir.ctime = now;
            }
            fx.row(home);
            fx.attrs(home);
        });
        Some((top, changes))
    }

    /// `queue drop`: the file goes back to its known remote revision and
    /// attributes. `false` without a remote node.
    pub fn revert_to_remote(&mut self, ino: Ino) -> bool {
        let Some(node) = self.nodes.get_mut(&ino) else {
            return false;
        };
        let Some(remote) = node.remote.clone() else {
            return false;
        };
        if node.kind == NodeKind::File {
            node.adopt_remote_attrs(&remote);
            node.atime = None;
            node.content = Content::of(&remote);
            node.content_epoch += 1;
        }
        node.gens.uploaded_content = node.gens.content;
        node.gens.uploaded_meta = node.gens.meta;
        node.deferred_error = None;
        node.remote_newer = false;
        true
    }

    /// The subtree at `top` was removed remotely: keeps only the pending
    /// items and the folders that lead to them, as nodes to create again
    /// (no remote state), and removes the rest (§10.5). `top` is never the
    /// root.
    pub fn resurrect_minimal(&mut self, top: Ino) -> Resurrected {
        let ((kept, removed_renamed), changes) =
            self.mutate(|tree, fx| tree.keep_pending(fx, top, true));
        Resurrected {
            changes,
            kept,
            removed_renamed,
        }
    }

    /// The pending items below `top` and the folders that lead to them
    /// become nodes to create again, and the rest is removed. A node moved
    /// in from outside goes back there with `send_home` (its folder is gone
    /// remotely, so the rename loses); otherwise it stays bound and moves
    /// into the new folder (its folder only lost its name). Returns the
    /// kept nodes, parents first, and the removed nodes that were renamed
    /// (see [`Resurrected`]).
    fn keep_pending(
        &mut self,
        fx: &mut Effects,
        top: Ino,
        send_home: bool,
    ) -> (Vec<Ino>, Vec<Ino>) {
        if top == ROOT_INO || !self.nodes.contains_key(&top) {
            return (Vec::new(), Vec::new());
        }
        // A node renamed into the folder lives outside it remotely.
        let mut moved_in: Vec<Ino> = self
            .subtree(top)
            .skip(1)
            .filter(|&current| {
                let alive_home = self.nodes[&current]
                    .remote
                    .as_ref()
                    .and_then(|remote| remote.parent_link.as_ref())
                    .and_then(|parent| self.by_link(parent))
                    .is_some_and(|home| !self.is_within(home, top));
                alive_home && self.placement_pending(current)
            })
            .collect();
        let stay_bound: HashSet<Ino> = if send_home {
            HashSet::new()
        } else {
            std::mem::take(&mut moved_in).into_iter().collect()
        };
        for ino in moved_in {
            self.adopt_remote_placement_in(fx, ino);
        }
        let mut order = Vec::new();
        let mut stack = vec![top];
        while let Some(current) = stack.pop() {
            let Some(node) = self.nodes.get(&current) else {
                continue;
            };
            order.push(current);
            // A node that stays bound keeps its subtree as it is.
            if !stay_bound.contains(&current) {
                stack.extend(node.children.values().copied());
            }
        }
        // Children come after their parent in `order`: walk it backwards.
        let mut keep = HashSet::new();
        for &ino in order.iter().rev() {
            let node = &self.nodes[&ino];
            if node.local_only
                || stay_bound.contains(&ino)
                || node.remote.is_none()
                || node.content_pending()
                || node.meta_pending()
                || (node.kind == NodeKind::File && node.gens.content != node.gens.uploaded_content)
                || node.children.values().any(|child| keep.contains(child))
            {
                keep.insert(ino);
            }
        }
        // Before the remote states go: which removed nodes were renamed?
        let removed_renamed = order
            .iter()
            .copied()
            .filter(|ino| !keep.contains(ino) && self.placement_pending(*ino))
            .collect();
        let topmost: Vec<Ino> = order
            .iter()
            .copied()
            .filter(|ino| {
                !keep.contains(ino) && (*ino == top || keep.contains(&self.nodes[ino].parent))
            })
            .collect();

        let mut kept = Vec::new();
        for &ino in order.iter().filter(|ino| keep.contains(ino)) {
            if stay_bound.contains(&ino) {
                fx.row(ino);
                continue;
            }
            self.set_remote(ino, None);
            let node = self.nodes.get_mut(&ino).expect("listed");
            node.remote_newer = false;
            if node.kind == NodeKind::File && matches!(node.content, Content::Remote { .. }) {
                // Still readable from the old revision; staged before the
                // upload.
                node.gens.content = node.gens.uploaded_content + 1;
            }
            kept.push(ino);
            fx.row(ino);
        }
        for ino in topmost {
            self.remove_subtree(fx, ino);
        }
        (kept, removed_renamed)
    }

    /// Checks the tree's internal consistency.
    #[cfg(test)]
    pub fn check_invariants(&self) -> std::result::Result<(), String> {
        for (&ino, node) in &self.nodes {
            if node.ino != ino {
                return Err(format!("node {ino} stores ino {}", node.ino));
            }
            if node.kind != NodeKind::Folder && !node.children.is_empty() {
                return Err(format!("file {ino} has children"));
            }
            for (name, child) in &node.children {
                let Some(child_node) = self.nodes.get(child) else {
                    return Err(format!("{ino}/{name} is missing node {child}"));
                };
                if !child_node.attached || child_node.parent != ino || &child_node.name != name {
                    return Err(format!(
                        "{ino}/{name} disagrees with node {child} ({}, parent {}, attached {})",
                        child_node.name, child_node.parent, child_node.attached
                    ));
                }
            }
            if node.attached && ino != ROOT_INO {
                let Some(parent) = self.nodes.get(&node.parent) else {
                    return Err(format!("node {ino} has no parent {}", node.parent));
                };
                if parent.children.get(&node.name) != Some(&ino) {
                    return Err(format!(
                        "node {ino} ({}) is not in the children of {}",
                        node.name, node.parent
                    ));
                }
                let mut current = node.parent;
                for _ in 0..=self.nodes.len() {
                    if current == ROOT_INO {
                        break;
                    }
                    current = self.nodes[&current].parent;
                }
                if current != ROOT_INO {
                    return Err(format!("node {ino} is in a cycle"));
                }
            }
            if let Some(remote) = &node.remote {
                if self.by_link.get(&remote.link) != Some(&ino) {
                    return Err(format!("link {} does not lead to node {ino}", remote.link));
                }
                if let Some(parent) = &remote.parent_link
                    && self
                        .by_remote_name
                        .get(&(parent.clone(), remote.name.clone()))
                        != Some(&ino)
                {
                    return Err(format!(
                        "remote name {parent}/{} does not lead to node {ino}",
                        remote.name
                    ));
                }
            }
        }
        for (link, ino) in &self.by_link {
            let remote = self.nodes.get(ino).and_then(|node| node.remote.as_ref());
            if remote.map(|remote| &remote.link) != Some(link) {
                return Err(format!("by_link {link} points at node {ino}"));
            }
        }
        for ((parent, name), ino) in &self.by_remote_name {
            let remote = self.nodes.get(ino).and_then(|node| node.remote.as_ref());
            if !remote.is_some_and(|remote| {
                remote.parent_link.as_ref() == Some(parent) && &remote.name == name
            }) {
                return Err(format!(
                    "by_remote_name {parent}/{name} points at node {ino}"
                ));
            }
        }
        Ok(())
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
            unverified: None,
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
            unverified: None,
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
        assert!(matches!(
            tree.lookup(3, "x"),
            Err(CoreError::NeedsListing(3))
        ));
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
        assert_eq!(
            changes.invalidations,
            [
                Invalidation::Entry {
                    parent: ROOT_INO,
                    name: "a.txt".into()
                },
                Invalidation::Delete {
                    parent: ROOT_INO,
                    child: 3,
                    name: "b.txt".into()
                },
                Invalidation::Entry {
                    parent: ROOT_INO,
                    name: "renamed.txt".into()
                },
                Invalidation::Inode { ino: ROOT_INO },
                // New content and size.
                Invalidation::Inode { ino: 2 },
            ]
        );
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
        // Both directories' cached pages go too.
        assert_eq!(
            changes.invalidations,
            [
                Invalidation::Entry {
                    parent: 2,
                    name: "f".into()
                },
                Invalidation::Inode { ino: 2 },
                Invalidation::Entry {
                    parent: 3,
                    name: "f".into()
                },
                Invalidation::Inode { ino: 3 },
            ]
        );
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
            remote_newer: false,
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
        let changes = tree
            .apply_listing(ROOT_INO, &[], &mut Vec::new())
            .expect("relist");
        // Kept as it is until the engine has looked at the server.
        assert_eq!(changes.vanished, [2]);
        assert!(tree.get(2).expect("d").remote.is_some());
        let resurrected = tree.resurrect_minimal(2);
        let d = tree.get(2).expect("d");
        assert!(d.attached && d.remote.is_none(), "kept for re-creation");
        assert_eq!(resurrected.kept, [2, 10]);
        assert_eq!(tree.child(2, "pending").expect("child"), Some(10));
        tree.check_invariants().expect("consistent");
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

    // ---- conflict resolution primitives ----

    fn listed_tree(children: &[RemoteNode], first_ino: Ino) -> Tree {
        let mut tree = Tree::with_root(root_state());
        let last = first_ino + children.len() as Ino;
        tree.apply_listing(ROOT_INO, children, &mut inos(first_ino..last))
            .expect("apply");
        tree
    }

    fn make_dirty(tree: &mut Tree, ino: Ino) {
        let node = tree.get_mut(ino).expect("node");
        node.content = Content::Staged;
        node.gens.content = 1;
    }

    fn server_state(link: &str, parent: &str, name: &str) -> RemoteState {
        let mut node = remote_in(parent, link, name, NodeKind::File, 7);
        node.active_revision = Some(RevisionId::new(format!("{link}-r2")));
        RemoteState::from_remote(&node)
    }

    fn copy_name(name: &str, attempt: u32) -> String {
        crate::conflicts::conflict_name(name, "pdmount", UNIX_EPOCH, attempt)
    }

    #[test]
    fn split_moves_the_local_inode_to_the_conflict_name_and_inserts_the_server_version() {
        let mut tree = listed_tree(&[remote("a", "a.txt", NodeKind::File, 1)], 2);
        make_dirty(&mut tree, 2);
        tree.get_mut(2).expect("a").remote_newer = true;
        let copy = copy_name("a.txt", 1);

        let (changes, note) = tree
            .split_conflict(2, server_state("a", "root", "a.txt"), 10, now())
            .expect("split");

        assert_eq!(changes.changed, [2, ROOT_INO, 10]);
        assert_eq!(
            changes.invalidations,
            [
                Invalidation::Entry {
                    parent: ROOT_INO,
                    name: "a.txt".into()
                },
                Invalidation::Entry {
                    parent: ROOT_INO,
                    name: copy.clone()
                },
                Invalidation::Inode { ino: ROOT_INO },
            ]
        );
        assert_eq!(note.kind, ConflictKind::EditEdit);
        assert_eq!((note.ino, note.other_ino), (2, Some(10)));
        assert_eq!(note.link, Some(LinkId::new("a")));
        assert_eq!(note.orig_name, "a.txt");
        assert_eq!(note.conflict_name.as_deref(), Some(copy.as_str()));
        assert_eq!(note.local_rev, Some(RevisionId::new("a-r1")));
        assert_eq!(note.remote_rev, Some(RevisionId::new("a-r2")));

        let x = tree.get(2).expect("x");
        assert_eq!((x.name.as_str(), x.parent), (copy.as_str(), ROOT_INO));
        assert!(x.remote.is_none() && !x.remote_newer && x.content_pending());
        assert_eq!(x.ctime, now());
        let y = tree.get(10).expect("y");
        assert_eq!(y.name, "a.txt");
        assert_eq!(
            y.content,
            Content::Remote {
                link: LinkId::new("a"),
                rev: Some(RevisionId::new("a-r2"))
            }
        );
        assert_eq!(y.size, 7);
        assert_eq!(tree.by_link(&LinkId::new("a")), Some(10));
        let root = tree.get(ROOT_INO).expect("root");
        assert_eq!(root.children.get("a.txt"), Some(&10));
        assert_eq!(root.children.get(&copy), Some(&2));
        assert_eq!(root.mtime, now());
        tree.check_invariants().expect("invariants");
    }

    #[test]
    fn split_skips_the_server_version_when_its_folder_is_not_listed() {
        let mut tree = listed_tree(
            &[
                remote("u", "u", NodeKind::Folder, 0),
                remote("a", "a.txt", NodeKind::File, 1),
            ],
            2,
        );
        make_dirty(&mut tree, 3);
        let copy = copy_name("a.txt", 1);

        let (changes, note) = tree
            .split_conflict(3, server_state("a", "u", "a.txt"), 10, now())
            .expect("split");

        assert_eq!(changes.changed, [3, ROOT_INO]);
        assert_eq!(changes.invalidations.len(), 3);
        assert_eq!(note.other_ino, None);
        assert!(tree.get(10).is_none());
        assert_eq!(tree.by_link(&LinkId::new("a")), None);
        assert_eq!(tree.child(ROOT_INO, &copy).expect("child"), Some(3));
        tree.check_invariants().expect("invariants");

        // Not an attached file bound to the server's link: nothing happens.
        assert!(
            tree.split_conflict(2, server_state("a", "root", "u"), 11, now())
                .is_none()
        );
        assert!(
            tree.split_conflict(3, server_state("other", "root", "a.txt"), 11, now())
                .is_none()
        );
        tree.check_invariants().expect("invariants");
    }

    #[test]
    fn resurrection_keeps_only_pending_items_and_their_folders() {
        let mut tree = listed_tree(&[remote("d", "d", NodeKind::Folder, 0)], 2);
        tree.apply_listing(
            2,
            &[
                remote_in("d", "clean", "clean", NodeKind::File, 1),
                remote_in("d", "sub", "sub", NodeKind::Folder, 0),
                remote_in("d", "p", "p", NodeKind::File, 1),
                remote_in("d", "sub2", "sub2", NodeKind::Folder, 0),
            ],
            &mut inos(3..7),
        )
        .expect("d");
        // 3 clean, 4 sub, 5 p, 6 sub2
        tree.apply_listing(
            4,
            &[
                remote_in("sub", "q", "q", NodeKind::File, 1),
                remote_in("sub", "r", "r", NodeKind::File, 1),
            ],
            &mut inos(7..9),
        )
        .expect("sub");
        tree.apply_listing(
            6,
            &[remote_in("sub2", "s", "s", NodeKind::File, 1)],
            &mut inos(9..10),
        )
        .expect("sub2");
        make_dirty(&mut tree, 5);
        // 7 (q) has only a metadata change and still reads from its revision.
        tree.get_mut(7).expect("q").gens.meta = 1;
        tree.get_mut(7).expect("q").remote_newer = true;

        let out = tree.resurrect_minimal(2);

        assert_eq!(out.kept, [2, 4, 7, 5]);
        assert!(out.removed_renamed.is_empty());
        for ino in &out.kept {
            let node = tree.get(*ino).expect("kept");
            assert!(node.remote.is_none() && node.attached && !node.remote_newer);
        }
        assert_eq!(
            tree.get(7).expect("q").gens.content,
            1,
            "staged from the old revision"
        );
        assert_eq!(tree.get(5).expect("p").gens.content, 1);
        let d = tree.get(2).expect("d");
        assert_eq!(d.children.keys().collect::<Vec<_>>(), ["p", "sub"]);
        assert_eq!(
            tree.get(4)
                .expect("sub")
                .children
                .keys()
                .collect::<Vec<_>>(),
            ["q"]
        );
        let mut removed = out.changes.removed.clone();
        removed.sort_unstable();
        assert_eq!(removed, [3, 6, 8, 9]);
        assert!(tree.get(3).is_none() && tree.get(9).is_none());
        // Only the tops of removed subtrees: s goes with sub2.
        let delete = |parent, child, name: &str| Invalidation::Delete {
            parent,
            child,
            name: name.into(),
        };
        assert_eq!(
            out.changes.invalidations,
            [
                delete(2, 6, "sub2"),
                delete(2, 3, "clean"),
                Invalidation::Inode { ino: 2 },
                delete(4, 8, "r"),
                Invalidation::Inode { ino: 4 },
            ]
        );
        assert_eq!(tree.by_link(&LinkId::new("d")), None);
        assert_eq!(tree.by_link(&LinkId::new("clean")), None);
        tree.check_invariants().expect("invariants");
    }

    #[test]
    fn a_clean_renamed_node_under_a_removed_folder_is_removed() {
        let mut tree = listed_tree(&[remote("d", "d", NodeKind::Folder, 0)], 2);
        tree.apply_listing(
            2,
            &[
                remote_in("d", "f", "f", NodeKind::File, 1),
                remote_in("d", "g", "g", NodeKind::File, 1),
            ],
            &mut inos(3..5),
        )
        .expect("d");
        tree.rename(2, "f", 2, "f2", RenameMode::Replace, now())
            .expect("rename");
        assert!(tree.placement_pending(3));

        let out = tree.resurrect_minimal(2);

        assert!(out.kept.is_empty());
        assert_eq!(out.removed_renamed, [3]);
        assert_eq!(
            out.changes.invalidations,
            [
                Invalidation::Delete {
                    parent: ROOT_INO,
                    child: 2,
                    name: "d".into()
                },
                Invalidation::Inode { ino: ROOT_INO },
            ]
        );
        assert!(tree.get(ROOT_INO).expect("root").children.is_empty());
        assert!(tree.get(2).is_none() && tree.get(3).is_none());
        tree.check_invariants().expect("invariants");
    }

    #[test]
    fn a_node_renamed_into_a_removed_folder_goes_home_without_touching_the_folder() {
        let mut tree = listed_tree(
            &[
                remote("d", "d", NodeKind::Folder, 0),
                remote("f", "f", NodeKind::File, 1),
            ],
            2,
        );
        tree.apply_listing(2, &[], &mut Vec::new()).expect("d");
        tree.rename(ROOT_INO, "f", 2, "f", RenameMode::NoReplace, now())
            .expect("move f");

        let out = tree.resurrect_minimal(2);

        assert!(out.kept.is_empty());
        assert_eq!(tree.path(3).as_deref(), Some("/f"));
        // The entry f left in d goes with d.
        assert_eq!(
            out.changes.invalidations,
            [
                Invalidation::Entry {
                    parent: ROOT_INO,
                    name: "f".into()
                },
                Invalidation::Delete {
                    parent: ROOT_INO,
                    child: 2,
                    name: "d".into()
                },
                Invalidation::Inode { ino: ROOT_INO },
            ]
        );
        tree.check_invariants().expect("invariants");
    }

    #[test]
    fn unbinding_a_tombstone_frees_its_link() {
        let mut tree = listed_tree(&[remote("f", "f", NodeKind::File, 1)], 2);
        assert!(tree.unbind_tombstone(2).is_none(), "not a tombstone");
        tree.remove_entry(ROOT_INO, "f", false, now())
            .expect("unlink");
        assert_eq!(tree.by_link(&LinkId::new("f")), Some(2));

        let changes = tree.unbind_tombstone(2).expect("unbind");

        assert_eq!(changes.removed, [2]);
        assert_eq!(tree.by_link(&LinkId::new("f")), None);
        assert!(tree.get(2).is_none(), "dropped: nothing holds it");
        tree.check_invariants().expect("invariants");

        // A kernel reference keeps the detached node, without its link.
        let mut tree = listed_tree(&[remote("f", "f", NodeKind::File, 1)], 2);
        tree.add_lookup(2);
        tree.remove_entry(ROOT_INO, "f", false, now())
            .expect("unlink");
        tree.unbind_tombstone(2).expect("unbind");
        let node = tree.get(2).expect("kept for the kernel");
        assert!(!node.deleted && !node.attached && node.remote.is_none());
        assert_eq!(tree.by_link(&LinkId::new("f")), None);
        tree.check_invariants().expect("invariants");
    }

    #[test]
    fn adopting_the_remote_placement_refuses_cycles_and_taken_names() {
        let mut tree = listed_tree(
            &[
                remote("d", "d", NodeKind::Folder, 0),
                remote("f", "f", NodeKind::File, 1),
            ],
            2,
        );
        tree.apply_listing(2, &[], &mut Vec::new()).expect("d");
        // 2 = d, 3 = f. Moved locally into d under another name.
        tree.rename(ROOT_INO, "f", 2, "f2", RenameMode::Replace, now())
            .expect("rename");
        // Another node took f's name at the root.
        tree.create(ROOT_INO, "f", NodeKind::File, 0o644, now(), 10)
            .expect("create");
        assert!(tree.adopt_remote_placement(3).is_none(), "name taken");
        assert_eq!(tree.path(3).as_deref(), Some("/d/f2"));
        tree.remove_entry(ROOT_INO, "f", false, now())
            .expect("unlink");

        let changes = tree.adopt_remote_placement(3).expect("adopt");

        assert_eq!(changes.changed, [3]);
        assert_eq!(
            changes.invalidations,
            [
                Invalidation::Entry {
                    parent: 2,
                    name: "f2".into()
                },
                Invalidation::Inode { ino: 2 },
                Invalidation::Entry {
                    parent: ROOT_INO,
                    name: "f".into()
                },
                Invalidation::Inode { ino: ROOT_INO },
            ]
        );
        assert_eq!(tree.path(3).as_deref(), Some("/f"));
        assert!(!tree.placement_pending(3));
        assert_eq!(
            tree.adopt_remote_placement(3),
            Some(Changes::default()),
            "already there"
        );
        tree.check_invariants().expect("invariants");

        // d moved into its own remote child's place: refused as a cycle.
        let mut tree = listed_tree(
            &[
                remote("a", "a", NodeKind::Folder, 0),
                remote("b", "b", NodeKind::Folder, 0),
            ],
            2,
        );
        tree.apply_listing(3, &[], &mut Vec::new()).expect("b");
        tree.apply_listing(2, &[], &mut Vec::new()).expect("a");
        tree.rename(ROOT_INO, "b", 2, "b", RenameMode::Replace, now())
            .expect("b under a");
        let mut state = tree.get(2).expect("a").remote.clone().expect("remote");
        state.parent_link = Some(LinkId::new("b"));
        tree.set_remote(2, Some(state));
        assert!(tree.adopt_remote_placement(2).is_none(), "cycle");
        // A directory that is not listed is refused too.
        let mut tree = listed_tree(
            &[
                remote("u", "u", NodeKind::Folder, 0),
                remote("f", "f", NodeKind::File, 1),
            ],
            2,
        );
        let mut state = tree.get(3).expect("f").remote.clone().expect("remote");
        state.parent_link = Some(LinkId::new("u"));
        tree.set_remote(3, Some(state));
        assert!(tree.adopt_remote_placement(3).is_none(), "not listed");
        tree.check_invariants().expect("invariants");
    }

    #[test]
    fn a_local_only_occupant_yields_to_a_new_remote_node() {
        let mut tree = listed_tree(&[], 2);
        tree.create(ROOT_INO, "scratch.txt", NodeKind::File, 0o644, now(), 7)
            .expect("create");
        tree.get_mut(7).expect("scratch").local_only = true;
        let changes = tree
            .apply_listing(
                ROOT_INO,
                &[remote("s", "scratch.txt", NodeKind::File, 4)],
                &mut vec![8],
            )
            .expect("apply");
        assert_eq!(tree.child(ROOT_INO, "scratch.txt").expect("child"), Some(8));
        let yielded = tree.get(7).expect("scratch").name.clone();
        assert_eq!(yielded, copy_name("scratch.txt", 1));
        assert_eq!(tree.child(ROOT_INO, &yielded).expect("child"), Some(7));
        assert!(changes.clashes.is_empty());
        assert_eq!(changes.notes.len(), 1);
        assert_eq!(changes.notes[0].kind, ConflictKind::NameTaken);
        assert_eq!(
            (changes.notes[0].ino, changes.notes[0].other_ino),
            (7, Some(8))
        );
        tree.check_invariants().expect("consistent");
    }

    #[test]
    fn a_pending_node_renamed_remotely_onto_a_used_name_becomes_a_new_local_node() {
        let mut tree = listed_tree(
            &[
                remote("a", "a.txt", NodeKind::File, 1),
                remote("c", "c.txt", NodeKind::File, 1),
            ],
            2,
        );
        make_dirty(&mut tree, 2);
        tree.create(ROOT_INO, "b.txt", NodeKind::File, 0o644, now(), 9)
            .expect("create");
        // Another device renamed a.txt to b.txt.
        let changes = tree
            .apply_listing(
                ROOT_INO,
                &[
                    remote("a", "b.txt", NodeKind::File, 1),
                    remote("c", "c.txt", NodeKind::File, 1),
                ],
                &mut Vec::new(),
            )
            .expect("apply");
        assert_eq!(changes.clashes, [9]);
        let a = tree.get(2).expect("a");
        assert!(a.remote.is_none(), "uploaded again as a new file");
        assert_eq!(a.name, "a.txt");
        assert_eq!(tree.child(ROOT_INO, "b.txt").expect("child"), Some(9));
        assert_eq!(tree.child(ROOT_INO, "a.txt").expect("child"), Some(2));
        assert_eq!(changes.notes[0].kind, ConflictKind::NameTaken);
        assert_eq!(changes.invalidations, [], "every name holds what it held");
        tree.check_invariants().expect("consistent");
    }

    #[test]
    fn a_folder_renamed_remotely_onto_a_used_name_keeps_only_its_pending_items() {
        let mut tree = listed_tree(
            &[
                remote("d", "d", NodeKind::Folder, 0),
                remote("m", "m", NodeKind::File, 1),
            ],
            2,
        );
        tree.apply_listing(
            2,
            &[remote_in("d", "b", "b", NodeKind::File, 1)],
            &mut inos(4..5),
        )
        .expect("d");
        // 2 d, 3 m (moved into d here), 4 d/b (renamed to b2 here), 9 d/new,
        // 10 a
        tree.rename(ROOT_INO, "m", 2, "m", RenameMode::NoReplace, now())
            .expect("move m");
        tree.rename(2, "b", 2, "b2", RenameMode::NoReplace, now())
            .expect("rename b");
        tree.create(2, "new", NodeKind::File, 0o644, now(), 9)
            .expect("create");
        tree.create(ROOT_INO, "a", NodeKind::File, 0o644, now(), 10)
            .expect("create");
        // Another device renamed d to a.
        let changes = tree
            .apply_listing(
                ROOT_INO,
                &[
                    remote("d", "a", NodeKind::Folder, 0),
                    remote("m", "m", NodeKind::File, 1),
                ],
                &mut Vec::new(),
            )
            .expect("apply");

        assert_eq!(changes.clashes, [10]);
        let notes: Vec<_> = changes.notes.iter().map(|n| (n.kind, n.ino)).collect();
        assert_eq!(
            notes,
            [
                (ConflictKind::RenameRename, 4),
                (ConflictKind::NameTaken, 2)
            ],
            "the rename inside d lost"
        );
        let d = tree.get(2).expect("d");
        assert!(d.remote.is_none(), "created again");
        assert_eq!(d.children.keys().collect::<Vec<_>>(), ["m", "new"]);
        assert_eq!(tree.by_link(&LinkId::new("d")), None);
        // The child is still in the remote folder and comes back with it.
        assert_eq!(changes.removed, [4]);
        assert_eq!(
            changes.invalidations,
            [
                Invalidation::Delete {
                    parent: 2,
                    child: 4,
                    name: "b2".into()
                },
                Invalidation::Inode { ino: 2 },
            ],
            "d keeps its name; only the lost rename's entry changes"
        );
        assert_eq!(tree.by_link(&LinkId::new("b")), None);
        // The node moved in keeps its link: its move goes to the new folder.
        assert_eq!(tree.by_link(&LinkId::new("m")), Some(3));
        assert!(tree.placement_pending(3));
        tree.check_invariants().expect("consistent");
    }

    /// A folder gone from the listing takes its subtree along, except a node
    /// inside that the same listing shows here (moved out before the
    /// folder was trashed). It used to be removed with the folder, and the
    /// listing then needed an inode number it had not counted.
    #[test]
    fn a_node_moved_out_of_a_vanished_folder_survives_the_listing() {
        let mut tree = listed_tree(&[remote("d", "d", NodeKind::Folder, 0)], 2);
        tree.apply_listing(
            2,
            &[remote_in("d", "b", "b", NodeKind::Folder, 0)],
            &mut inos(3..4),
        )
        .expect("d");
        // Remotely: d/b was renamed to /a, then d was trashed.
        let changes = tree
            .apply_listing(
                ROOT_INO,
                &[remote("b", "a", NodeKind::Folder, 0)],
                &mut Vec::new(),
            )
            .expect("no new inode needed");
        assert_eq!(tree.child(ROOT_INO, "a").expect("child"), Some(3));
        assert_eq!(tree.child(ROOT_INO, "d").expect("child"), None);
        assert_eq!(changes.removed, [2]);
        // Nothing for the entries of the removed folder itself.
        assert_eq!(
            changes.invalidations,
            [
                Invalidation::Delete {
                    parent: ROOT_INO,
                    child: 2,
                    name: "d".into()
                },
                Invalidation::Entry {
                    parent: ROOT_INO,
                    name: "a".into()
                },
                Invalidation::Inode { ino: ROOT_INO },
            ]
        );
        tree.check_invariants().expect("consistent");
    }

    /// D132: a child the server lists but cannot decrypt is not gone; a
    /// child missing from the same listing is.
    #[test]
    fn a_degraded_child_stays_and_a_missing_one_goes() {
        let mut tree = listed_tree(
            &[
                remote("a", "a", NodeKind::File, 1),
                remote("b", "b", NodeKind::File, 1),
            ],
            2,
        );
        let listed = [remote("c", "c", NodeKind::File, 1)];
        let changes = tree
            .apply_listing_at(
                ROOT_INO,
                &listed,
                &[LinkId::new("a")],
                &mut inos(4..5),
                u64::MAX,
            )
            .expect("apply");
        assert_eq!(changes.removed, vec![3], "b is gone");
        assert_eq!(tree.child(ROOT_INO, "a").expect("dir"), Some(2), "a stays");
        assert_eq!(tree.child(ROOT_INO, "b").expect("dir"), None);
        assert_eq!(tree.child(ROOT_INO, "c").expect("dir"), Some(4));
        tree.check_invariants().expect("consistent");
    }

    #[test]
    fn a_listing_fetched_before_a_stamp_leaves_the_node_alone() {
        let mut tree = listed_tree(&[remote("a", "a", NodeKind::File, 1)], 2);
        // The engine read the node fresh at stamp 5: it is now called b.
        let fresh = RemoteState::from_remote(&remote("a", "b", NodeKind::File, 1));
        tree.set_remote_at(2, Some(fresh.clone()), 5);
        let stale = [remote("a", "a", NodeKind::File, 1)];
        tree.apply_listing_at(ROOT_INO, &stale, &[], &mut Vec::new(), 4)
            .expect("apply");
        assert_eq!(tree.get(2).expect("a").remote.as_ref(), Some(&fresh));
        // A listing fetched after the stamp applies.
        tree.apply_listing_at(ROOT_INO, &stale, &[], &mut Vec::new(), 5)
            .expect("apply");
        let remote_name = tree
            .get(2)
            .and_then(|node| node.remote.as_ref())
            .map(|r| r.name.clone());
        assert_eq!(remote_name.as_deref(), Some("a"));
        tree.check_invariants().expect("consistent");
    }

    #[test]
    fn a_folder_with_only_a_pending_rename_that_loses_its_name_is_invalidated() {
        let mut tree = listed_tree(&[remote("d", "d", NodeKind::Folder, 0)], 2);
        tree.apply_listing(
            2,
            &[remote_in("d", "b", "b", NodeKind::File, 1)],
            &mut inos(3..4),
        )
        .expect("d");
        // 2 d, 3 d/b (renamed to b2 here), 10 a
        tree.rename(2, "b", 2, "b2", RenameMode::NoReplace, now())
            .expect("rename b");
        tree.create(ROOT_INO, "a", NodeKind::File, 0o644, now(), 10)
            .expect("create");
        // Another device renamed d to a: the rename inside d loses, and d
        // with it.
        let changes = tree
            .apply_listing(
                ROOT_INO,
                &[remote("d", "a", NodeKind::Folder, 0)],
                &mut Vec::new(),
            )
            .expect("apply");

        assert_eq!(tree.child(ROOT_INO, "d").expect("child"), None);
        assert_eq!(
            changes.invalidations,
            [
                Invalidation::Delete {
                    parent: ROOT_INO,
                    child: 2,
                    name: "d".into(),
                },
                Invalidation::Inode { ino: ROOT_INO },
            ],
            "the kernel forgets d"
        );
        tree.check_invariants().expect("consistent");
    }

    #[test]
    fn a_folder_moved_into_a_folder_that_loses_its_name_keeps_its_subtree() {
        let mut tree = listed_tree(
            &[
                remote("d", "d", NodeKind::Folder, 0),
                remote("x", "x", NodeKind::Folder, 0),
            ],
            2,
        );
        tree.apply_listing(
            3,
            &[
                remote_in("x", "c", "c", NodeKind::File, 1),
                remote_in("x", "e", "e", NodeKind::File, 1),
            ],
            &mut inos(4..6),
        )
        .expect("x");
        tree.apply_listing(2, &[], &mut Vec::new()).expect("d");
        // 2 d, 3 x (moved into d here), 4 x/c (clean), 5 x/e (edited),
        // 9 d/new, 10 a
        tree.get_mut(5).expect("e").gens.content = 1;
        tree.rename(ROOT_INO, "x", 2, "x", RenameMode::NoReplace, now())
            .expect("move x");
        tree.create(2, "new", NodeKind::File, 0o644, now(), 9)
            .expect("create");
        tree.create(ROOT_INO, "a", NodeKind::File, 0o644, now(), 10)
            .expect("create");
        // Another device renamed d to a.
        let changes = tree
            .apply_listing(
                ROOT_INO,
                &[
                    remote("d", "a", NodeKind::Folder, 0),
                    remote("x", "x", NodeKind::Folder, 0),
                ],
                &mut Vec::new(),
            )
            .expect("apply");

        assert!(changes.removed.is_empty(), "removed {:?}", changes.removed);
        assert_eq!(tree.by_link(&LinkId::new("x")), Some(3));
        assert!(tree.placement_pending(3));
        // Both children are still in the remote folder x.
        assert_eq!(tree.by_link(&LinkId::new("c")), Some(4));
        assert_eq!(tree.by_link(&LinkId::new("e")), Some(5));
        assert_eq!(tree.get(5).expect("e").gens.content, 1, "still to upload");
        tree.check_invariants().expect("consistent");
    }

    #[test]
    fn a_node_never_follows_the_remote_into_its_own_subtree() {
        let mut tree = listed_tree(&[remote("d", "d", NodeKind::Folder, 0)], 2);
        tree.apply_listing(
            2,
            &[remote_in("d", "s", "s", NodeKind::Folder, 0)],
            &mut inos(3..4),
        )
        .expect("d");
        // A stale or hostile listing of `s` shows `d` inside it.
        tree.apply_listing(
            3,
            &[remote_in("s", "d", "d", NodeKind::Folder, 0)],
            &mut Vec::new(),
        )
        .expect("s");
        assert_eq!(tree.child(ROOT_INO, "d").expect("child"), Some(2));
        assert_eq!(tree.child(3, "d").expect("child"), None);
        tree.check_invariants().expect("consistent");
    }

    #[test]
    fn yield_name_picks_a_free_conflict_name() {
        let mut tree = listed_tree(&[remote("a", "a.txt", NodeKind::File, 1)], 2);
        tree.create(
            ROOT_INO,
            &copy_name("a.txt", 1),
            NodeKind::File,
            0o644,
            now(),
            10,
        )
        .expect("create");

        let (changes, from, to) = tree.yield_name(2, now()).expect("yield");

        assert_eq!(from, "a.txt");
        assert_eq!(to, copy_name("a.txt", 2));
        assert_eq!(changes.changed, [2, ROOT_INO]);
        assert_eq!(
            changes.invalidations,
            [
                Invalidation::Entry {
                    parent: ROOT_INO,
                    name: "a.txt".into()
                },
                Invalidation::Entry {
                    parent: ROOT_INO,
                    name: to.clone()
                },
                Invalidation::Inode { ino: ROOT_INO },
            ]
        );
        assert_eq!(tree.child(ROOT_INO, "a.txt").expect("child"), None);
        assert_eq!(tree.child(ROOT_INO, &to).expect("child"), Some(2));
        assert!(tree.placement_pending(2), "the rename is still to upload");
        assert!(tree.yield_name(ROOT_INO, now()).is_none());
        tree.check_invariants().expect("invariants");
    }
}
