//! Inode table (DESIGN.md §4.3, §8.8).
//!
//! The tree in RAM is authoritative for FUSE. A directory is `listed` once its
//! children were fetched; from then on lookups in it, including negative
//! ones, are answered from RAM. Listings are applied as diffs: they move,
//! rename, update and remove nodes, and report what the store must persist
//! and which kernel caches must be invalidated.
//!
//! Inode numbers are allocated from a persistent counter, one per `link_id`,
//! and never reused. A node removed remotely while the kernel still holds
//! references (`nlookup > 0`) stays in RAM, detached, until it is forgotten.

use pdmount_backend::{LinkId, NodeKind, RemoteNode, RevisionId};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub type Ino = u64;
pub const ROOT_INO: Ino = 1;
pub const MAX_NAME_BYTES: usize = 255;
const DEFAULT_FILE_MODE: u32 = 0o644;
const DEFAULT_DIR_MODE: u32 = 0o755;

/// Attributes reported to the kernel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attr {
    pub ino: Ino,
    pub kind: NodeKind,
    pub size: u64,
    pub mtime: SystemTime,
    /// Permission bits only.
    pub perm: u32,
    /// Directories report 1 ("unknown"), so tools never infer the number of
    /// subdirectories from it.
    pub nlink: u32,
}

/// Remote state of a node, as last listed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeData {
    pub link: LinkId,
    pub kind: NodeKind,
    pub rev: Option<RevisionId>,
    pub size: u64,
    pub mtime: SystemTime,
    pub mode: Option<u32>,
}

impl NodeData {
    pub fn from_remote(node: &RemoteNode) -> Self {
        Self {
            link: node.link_id.clone(),
            kind: node.kind,
            rev: node.active_revision.clone(),
            size: node.size.unwrap_or(0),
            mtime: node.mtime.unwrap_or(UNIX_EPOCH),
            mode: node.mode.map(|mode| mode & 0o7777),
        }
    }
}

#[derive(Debug)]
pub struct Node {
    pub ino: Ino,
    pub parent: Ino,
    pub name: String,
    pub data: NodeData,
    pub listed: bool,
    /// Children by name; complete only when `listed`.
    pub children: BTreeMap<String, Ino>,
    pub nlookup: u64,
    /// `false` once removed remotely; kept only while the kernel holds it.
    pub attached: bool,
    /// Revision whose content the kernel last cached (`FOPEN_KEEP_CACHE`).
    pub served_rev: Option<RevisionId>,
}

impl Node {
    pub fn attr(&self) -> Attr {
        let (default_mode, nlink) = match self.data.kind {
            NodeKind::File => (DEFAULT_FILE_MODE, 1),
            NodeKind::Folder => (DEFAULT_DIR_MODE, 1),
        };
        Attr {
            ino: self.ino,
            kind: self.data.kind,
            size: match self.data.kind {
                NodeKind::File => self.data.size,
                NodeKind::Folder => 0,
            },
            mtime: self.data.mtime,
            perm: self.data.mode.unwrap_or(default_mode),
            nlink,
        }
    }

    pub fn is_dir(&self) -> bool {
        self.data.kind == NodeKind::Folder
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

/// One row to persist for a node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRow {
    pub ino: Ino,
    pub parent: Ino,
    pub name: String,
    pub data: NodeData,
    pub listed: bool,
}

/// Effects of applying a listing.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ListingDiff {
    /// The listed directory's children, to be rewritten in the store.
    pub rows: Vec<NodeRow>,
    /// Inodes (with their links) removed with their subtrees.
    pub removed: Vec<(Ino, LinkId)>,
    pub invalidations: Vec<Invalidation>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ApplyError {
    /// The directory no longer exists or is not a directory.
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

#[derive(Debug, Default)]
pub struct Tree {
    nodes: HashMap<Ino, Node>,
    by_link: HashMap<LinkId, Ino>,
}

/// Checks a name from the kernel (§8.8).
pub fn validate_name(name: &[u8]) -> Result<&str, crate::errors::CoreError> {
    if name.len() > MAX_NAME_BYTES {
        return Err(crate::errors::CoreError::NameTooLong);
    }
    std::str::from_utf8(name).map_err(|_| crate::errors::CoreError::InvalidName)
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
    pub fn with_root(root: NodeData) -> Self {
        let mut tree = Self::default();
        tree.insert(Node {
            ino: ROOT_INO,
            parent: ROOT_INO,
            name: String::new(),
            data: root,
            listed: false,
            children: BTreeMap::new(),
            nlookup: 0,
            attached: true,
            served_rev: None,
        });
        tree
    }

    /// Rebuilds the tree from stored rows. Rows whose parent is missing or not
    /// listed are dropped and returned so the store can delete them.
    pub fn from_rows(rows: Vec<NodeRow>) -> (Self, Vec<(Ino, LinkId)>) {
        let mut tree = Self::default();
        for row in rows {
            tree.insert(Node {
                ino: row.ino,
                parent: row.parent,
                name: row.name,
                data: row.data,
                listed: row.listed,
                children: BTreeMap::new(),
                nlookup: 0,
                attached: true,
                served_rev: None,
            });
        }
        // Attach children to listed parents; anything unreachable is stale.
        let mut reachable = HashSet::from([ROOT_INO]);
        let mut queue = vec![ROOT_INO];
        let mut by_parent: HashMap<Ino, Vec<Ino>> = HashMap::new();
        for node in tree.nodes.values() {
            if node.ino != ROOT_INO {
                by_parent.entry(node.parent).or_default().push(node.ino);
            }
        }
        while let Some(dir) = queue.pop() {
            let listed = tree.nodes.get(&dir).is_some_and(|node| node.listed);
            if !listed {
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
        let stale: Vec<(Ino, LinkId)> = tree
            .nodes
            .values()
            .filter(|node| !reachable.contains(&node.ino))
            .map(|node| (node.ino, node.data.link.clone()))
            .collect();
        for (ino, link) in &stale {
            tree.nodes.remove(ino);
            tree.by_link.remove(link);
        }
        (tree, stale)
    }

    fn insert(&mut self, node: Node) {
        self.by_link.insert(node.data.link.clone(), node.ino);
        self.nodes.insert(node.ino, node);
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

    /// Listed directories, root first (breadth-first).
    pub fn listed_dirs(&self) -> Vec<Ino> {
        let mut out = Vec::new();
        let mut queue = std::collections::VecDeque::from([ROOT_INO]);
        while let Some(ino) = queue.pop_front() {
            let Some(node) = self.nodes.get(&ino) else {
                continue;
            };
            if node.is_dir() && node.listed {
                out.push(ino);
                queue.extend(node.children.values().copied());
            }
        }
        out
    }

    /// Hot-path lookup. Increments `nlookup` on success.
    pub fn lookup(&mut self, parent: Ino, name: &str) -> Result<Lookup, crate::errors::CoreError> {
        use crate::errors::CoreError;
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

    /// Adds kernel references, e.g. for entries returned by readdirplus.
    pub fn add_lookup(&mut self, ino: Ino) -> Option<Attr> {
        let node = self.nodes.get_mut(&ino)?;
        node.nlookup += 1;
        Some(node.attr())
    }

    /// Drops kernel references; detached nodes go away at zero.
    pub fn forget(&mut self, ino: Ino, count: u64) {
        let Some(node) = self.nodes.get_mut(&ino) else {
            return;
        };
        node.nlookup = node.nlookup.saturating_sub(count);
        if node.nlookup == 0 && !node.attached && ino != ROOT_INO {
            self.nodes.remove(&ino);
        }
    }

    /// Snapshot of a listed directory for readdir, with `.` and `..`.
    pub fn dir_entries(&self, ino: Ino) -> Result<Option<Vec<DirEntry>>, crate::errors::CoreError> {
        use crate::errors::CoreError;
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
                    kind: node.data.kind,
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

    /// Applies a full listing of `dir`, taking new inode numbers from
    /// `new_inos`. Nothing changes when an error is returned.
    pub fn apply_listing(
        &mut self,
        dir: Ino,
        children: &[RemoteNode],
        new_inos: &mut Vec<Ino>,
    ) -> Result<ListingDiff, ApplyError> {
        let dir_node = self.nodes.get(&dir).ok_or(ApplyError::NotADirectory)?;
        if !dir_node.is_dir() || !dir_node.attached {
            return Err(ApplyError::NotADirectory);
        }
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
            if child.link_id == self.nodes[&dir].data.link
                || !seen_names.insert(child.name.as_str())
            {
                tracing::warn!(name = ?child.name, "skipping a duplicate remote name");
                continue;
            }
            incoming.push(child);
        }
        let unknown = incoming
            .iter()
            .filter(|child| !self.by_link.contains_key(&child.link_id))
            .count();
        if unknown > new_inos.len() {
            return Err(ApplyError::NeedInos(unknown - new_inos.len()));
        }

        let mut diff = ListingDiff::default();
        let old_children = std::mem::take(&mut self.nodes.get_mut(&dir).expect("checked").children);
        let incoming_links: HashSet<&LinkId> =
            incoming.iter().map(|child| &child.link_id).collect();

        // Removals: children no longer in the listing.
        for (name, &ino) in &old_children {
            let link = &self.nodes[&ino].data.link;
            if !incoming_links.contains(link) {
                if was_listed {
                    diff.invalidations.push(Invalidation::Delete {
                        parent: dir,
                        child: ino,
                        name: name.clone(),
                    });
                }
                self.remove_subtree(ino, &mut diff.removed);
            }
        }

        let mut new_children = BTreeMap::new();
        for child in incoming {
            let data = NodeData::from_remote(child);
            let ino = match self.by_link.get(&child.link_id) {
                Some(&ino) => {
                    let node = self.nodes.get_mut(&ino).expect("by_link is consistent");
                    let (old_parent, old_name) = (node.parent, node.name.clone());
                    let content_changed = node.data.rev != data.rev
                        || node.data.size != data.size
                        || node.data.mtime != data.mtime
                        || node.data.mode != data.mode;
                    if content_changed {
                        diff.invalidations.push(Invalidation::Inode { ino });
                    }
                    node.parent = dir;
                    node.name = child.name.clone();
                    node.data = data;
                    if old_parent != dir {
                        // Moved in from another directory.
                        if let Some(old_dir) = self.nodes.get_mut(&old_parent)
                            && old_dir.children.get(&old_name) == Some(&ino)
                        {
                            old_dir.children.remove(&old_name);
                            diff.invalidations.push(Invalidation::Entry {
                                parent: old_parent,
                                name: old_name,
                            });
                        }
                    }
                    ino
                }
                None => {
                    let ino = new_inos.pop().expect("counted above");
                    self.insert(Node {
                        ino,
                        parent: dir,
                        name: child.name.clone(),
                        data,
                        listed: false,
                        children: BTreeMap::new(),
                        nlookup: 0,
                        attached: true,
                        served_rev: None,
                    });
                    ino
                }
            };
            new_children.insert(child.name.clone(), ino);
        }

        if was_listed {
            for (name, ino) in &old_children {
                if new_children.get(name) != Some(ino)
                    && self.nodes.get(ino).is_some_and(|n| n.attached)
                {
                    diff.invalidations.push(Invalidation::Entry {
                        parent: dir,
                        name: name.clone(),
                    });
                }
            }
            for (name, ino) in &new_children {
                if old_children.get(name) != Some(ino) {
                    diff.invalidations.push(Invalidation::Entry {
                        parent: dir,
                        name: name.clone(),
                    });
                }
            }
            diff.invalidations.dedup();
            // Directory pages are cached (`FOPEN_CACHE_DIR`); drop them too.
            let entries_changed = diff
                .invalidations
                .iter()
                .any(|i| matches!(i, Invalidation::Entry { parent, .. } | Invalidation::Delete { parent, .. } if *parent == dir));
            if entries_changed {
                diff.invalidations.push(Invalidation::Inode { ino: dir });
            }
        }

        for (name, &ino) in &new_children {
            let node = &self.nodes[&ino];
            diff.rows.push(NodeRow {
                ino,
                parent: dir,
                name: name.clone(),
                data: node.data.clone(),
                listed: node.listed,
            });
        }
        let dir_node = self.nodes.get_mut(&dir).expect("checked");
        dir_node.children = new_children;
        dir_node.listed = true;
        Ok(diff)
    }

    /// Updates the directory node itself (e.g. the root after a fresh fetch).
    pub fn update_data(&mut self, ino: Ino, data: NodeData) -> bool {
        let Some(node) = self.nodes.get_mut(&ino) else {
            return false;
        };
        let changed = node.data != data;
        node.data = data;
        changed
    }

    pub fn row(&self, ino: Ino) -> Option<NodeRow> {
        let node = self.nodes.get(&ino)?;
        Some(NodeRow {
            ino,
            parent: node.parent,
            name: node.name.clone(),
            data: node.data.clone(),
            listed: node.listed,
        })
    }

    fn remove_subtree(&mut self, ino: Ino, removed: &mut Vec<(Ino, LinkId)>) {
        let mut stack = vec![ino];
        while let Some(current) = stack.pop() {
            let Some(node) = self.nodes.get_mut(&current) else {
                continue;
            };
            stack.extend(node.children.values().copied());
            node.children.clear();
            node.attached = false;
            node.listed = false;
            let link = node.data.link.clone();
            let keep = node.nlookup > 0;
            if self.by_link.get(&link) == Some(&current) {
                self.by_link.remove(&link);
            }
            removed.push((current, link));
            if !keep {
                self.nodes.remove(&current);
            }
        }
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

    fn root_data() -> NodeData {
        NodeData {
            link: LinkId::new("root"),
            kind: NodeKind::Folder,
            rev: None,
            size: 0,
            mtime: UNIX_EPOCH,
            mode: None,
        }
    }

    fn remote(link: &str, name: &str, kind: NodeKind, size: u64) -> RemoteNode {
        RemoteNode {
            link_id: LinkId::new(link),
            parent_link_id: Some(LinkId::new("root")),
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

    fn inos(range: std::ops::Range<Ino>) -> Vec<Ino> {
        range.rev().collect()
    }

    #[test]
    fn first_listing_creates_children_without_invalidations() {
        let mut tree = Tree::with_root(root_data());
        let listing = [
            remote("a", "a.txt", NodeKind::File, 3),
            remote("d", "dir", NodeKind::Folder, 0),
        ];
        let diff = tree
            .apply_listing(ROOT_INO, &listing, &mut inos(2..4))
            .expect("apply");
        assert_eq!(diff.rows.len(), 2);
        assert!(diff.invalidations.is_empty());
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
        let mut tree = Tree::with_root(root_data());
        let first = [
            remote("a", "a.txt", NodeKind::File, 3),
            remote("b", "b.txt", NodeKind::File, 3),
        ];
        tree.apply_listing(ROOT_INO, &first, &mut inos(2..4))
            .expect("apply");
        let a = tree.get(2).expect("a").ino;
        let mut renamed = remote("a", "renamed.txt", NodeKind::File, 5);
        renamed.active_revision = Some(RevisionId::new("a-r2"));
        let diff = tree
            .apply_listing(ROOT_INO, &[renamed], &mut Vec::new())
            .expect("apply");
        assert!(diff.invalidations.contains(&Invalidation::Inode { ino: a }));
        assert!(diff.invalidations.contains(&Invalidation::Entry {
            parent: ROOT_INO,
            name: "a.txt".into()
        }));
        assert!(diff.invalidations.contains(&Invalidation::Delete {
            parent: ROOT_INO,
            child: 3,
            name: "b.txt".into()
        }));
        assert_eq!(diff.removed, vec![(3, LinkId::new("b"))]);
        // The inode number survives the rename.
        assert!(
            matches!(tree.lookup(ROOT_INO, "renamed.txt"), Ok(Lookup::Found(attr)) if attr.ino == a && attr.size == 5)
        );
    }

    #[test]
    fn a_move_between_listed_directories_keeps_the_inode() {
        let mut tree = Tree::with_root(root_data());
        tree.apply_listing(
            ROOT_INO,
            &[
                remote("d1", "d1", NodeKind::Folder, 0),
                remote("d2", "d2", NodeKind::Folder, 0),
            ],
            &mut inos(2..4),
        )
        .expect("root");
        tree.apply_listing(2, &[remote("f", "f", NodeKind::File, 1)], &mut inos(4..5))
            .expect("d1");
        tree.apply_listing(3, &[], &mut Vec::new()).expect("d2");
        let diff = tree
            .apply_listing(3, &[remote("f", "f", NodeKind::File, 1)], &mut Vec::new())
            .expect("move");
        assert!(diff.invalidations.contains(&Invalidation::Entry {
            parent: 2,
            name: "f".into()
        }));
        assert_eq!(tree.lookup(2, "f").expect("old"), Lookup::Negative);
        assert!(matches!(tree.lookup(3, "f"), Ok(Lookup::Found(attr)) if attr.ino == 4));
    }

    #[test]
    fn removed_nodes_with_kernel_references_live_until_forgotten() {
        let mut tree = Tree::with_root(root_data());
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
        tree.forget(2, 1);
        assert!(tree.get(2).is_none());
    }

    #[test]
    fn unrepresentable_and_duplicate_names_are_skipped() {
        let mut tree = Tree::with_root(root_data());
        let listing = [
            remote("a", "ok", NodeKind::File, 1),
            remote("b", "ok", NodeKind::File, 1),
            remote("c", "..", NodeKind::File, 1),
            remote("d", &"x".repeat(256), NodeKind::File, 1),
        ];
        let diff = tree
            .apply_listing(ROOT_INO, &listing, &mut inos(2..6))
            .expect("apply");
        assert_eq!(diff.rows.len(), 1);
    }

    #[test]
    fn missing_inode_numbers_are_requested_without_changes() {
        let mut tree = Tree::with_root(root_data());
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
    fn rows_round_trip_and_unreachable_rows_are_dropped() {
        let mut tree = Tree::with_root(root_data());
        tree.apply_listing(
            ROOT_INO,
            &[remote("d", "d", NodeKind::Folder, 0)],
            &mut inos(2..3),
        )
        .expect("apply");
        let mut rows: Vec<NodeRow> = [ROOT_INO, 2]
            .iter()
            .filter_map(|&ino| tree.row(ino))
            .collect();
        rows.push(NodeRow {
            ino: 9,
            parent: 2, // d is not listed, so this row is stale
            name: "stale".into(),
            data: NodeData::from_remote(&remote("s", "stale", NodeKind::File, 1)),
            listed: false,
        });
        let (mut loaded, stale) = Tree::from_rows(rows);
        assert_eq!(stale, vec![(9, LinkId::new("s"))]);
        assert!(matches!(loaded.lookup(ROOT_INO, "d"), Ok(Lookup::Found(attr)) if attr.ino == 2));
    }

    #[test]
    fn name_validation() {
        assert!(validate_name(b"ok").is_ok());
        assert!(matches!(
            validate_name(&[0xff]),
            Err(crate::errors::CoreError::InvalidName)
        ));
        assert!(matches!(
            validate_name(&[b'x'; 256]),
            Err(crate::errors::CoreError::NameTooLong)
        ));
    }

    #[test]
    fn time_conversion_round_trips() {
        let time = UNIX_EPOCH + Duration::from_nanos(1_700_000_000_123_456_789);
        assert_eq!(ns_to_system_time(system_time_to_ns(time)), time);
        let before = UNIX_EPOCH - Duration::from_secs(5);
        assert_eq!(ns_to_system_time(system_time_to_ns(before)), before);
    }
}
