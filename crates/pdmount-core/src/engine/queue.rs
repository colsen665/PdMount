//! Queue tooling (DESIGN.md §8.2, D061): the views and actions behind
//! `pdmount queue` and `pdmount sync`. Entries are named by inode.

use super::{Engine, Result};
use crate::errors::CoreError;
use crate::vfs::{Changes, Ino, Invalidation, Tree};
use pdmount_backend::{LinkId, NodeKind};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// Which entries a command acts on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueueTarget {
    Id(Ino),
    All,
    /// Relative to the mount root: its entry or, for a folder, every entry
    /// under it.
    Path(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueKind {
    Upload,
    CreateFolder,
    Move,
    Trash,
    Metadata,
    /// The planner has not looked at it yet.
    Nothing,
}

impl QueueKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Upload => "upload",
            Self::CreateFolder => "create-folder",
            Self::Move => "move",
            Self::Trash => "trash",
            Self::Metadata => "metadata",
            Self::Nothing => "-",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueueState {
    Waiting,
    Running,
    Backoff { until: SystemTime },
    Parked { reason: String },
}

/// One entry of `queue list` and `queue show`.
#[derive(Debug, Clone)]
pub struct QueueEntry {
    pub id: Ino,
    pub path: String,
    pub kind: QueueKind,
    pub state: QueueState,
    pub attempts: u32,
    pub age: Duration,
    pub last_error: Option<String>,
    pub base_rev: Option<String>,
    /// Paths of what the entry waits for.
    pub dependencies: Vec<String>,
}

/// An entry a command acted on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueItem {
    pub id: Ino,
    pub path: String,
}

/// What `queue drop` does to one node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DropItem {
    pub id: Ino,
    pub path: String,
    pub effect: &'static str,
    /// Why it was not dropped; it stays queued.
    pub refused: Option<String>,
}

/// The outcome of `sync`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncOutcome {
    /// Entries still to do: the timeout passed, or a parked entry holds
    /// them up.
    pub pending: usize,
    /// Path and reason.
    pub parked: Vec<(String, String)>,
}

/// Where an entry is: the node's path or, for a tombstone, where the remote
/// file it is about to trash lives.
fn entry_path(tree: &Tree, ino: Ino) -> String {
    if let Some(path) = tree.path(ino) {
        return path;
    }
    let remote_place = tree
        .get(ino)
        .and_then(|node| node.remote.as_ref())
        .and_then(|remote| {
            let parent = tree.by_link(remote.parent_link.as_ref()?)?;
            Some(format!(
                "{}/{}",
                entry_path(tree, parent).trim_end_matches('/'),
                remote.name
            ))
        });
    remote_place.unwrap_or_else(|| format!("(inode {ino})"))
}

/// Whether the entry is at or below `root`, tombstones at their remote place.
fn entry_within(tree: &Tree, mut ino: Ino, root: Ino) -> bool {
    // Bounded: a chain of tombstones ends at an attached node or an unknown
    // link, and no remote parent chain has a cycle longer than the tree.
    for _ in 0..=tree.len() {
        let Some(node) = tree.get(ino) else {
            return false;
        };
        if node.attached {
            return tree.is_within(ino, root);
        }
        let parent = node
            .remote
            .as_ref()
            .and_then(|remote| remote.parent_link.as_ref())
            .and_then(|link| tree.by_link(link));
        match parent {
            Some(parent) => ino = parent,
            None => return false,
        }
    }
    false
}

fn kind_of(tree: &Tree, ino: Ino) -> QueueKind {
    let Some(node) = tree.get(ino) else {
        return QueueKind::Nothing;
    };
    if node.deleted {
        return QueueKind::Trash;
    }
    if !node.attached || (node.local_only && node.remote.is_none()) {
        return QueueKind::Nothing;
    }
    match (node.kind, node.remote.is_some()) {
        (NodeKind::Folder, false) => QueueKind::CreateFolder,
        (NodeKind::File, false) => QueueKind::Upload,
        (_, true) if tree.placement_pending(ino) => QueueKind::Move,
        (NodeKind::File, true) if node.content_pending() => QueueKind::Upload,
        (NodeKind::File, true) if node.meta_pending() => QueueKind::Metadata,
        _ => QueueKind::Nothing,
    }
}

/// How `queue drop` undoes a node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Undo {
    /// The remote file comes back.
    Delete,
    /// Local content, attributes and placement go back to the remote ones.
    Revert,
    /// Never uploaded: removed with its subtree.
    Remove,
}

impl Undo {
    fn effect(self) -> &'static str {
        match self {
            Self::Delete => "undo the delete; the remote file comes back",
            Self::Revert => "discard the local change; back to the remote version",
            Self::Remove => "remove it: never uploaded, no other copy exists",
        }
    }
}

impl Engine {
    /// The entries `target` names, without looking at the network for `Id`
    /// and `All`.
    async fn queue_select(self: &Arc<Self>, target: &QueueTarget) -> Result<Vec<Ino>> {
        match target {
            QueueTarget::All => Ok(self.journal.inos()),
            QueueTarget::Id(id) => {
                if self.journal.entry_state(*id).is_some() {
                    Ok(vec![*id])
                } else {
                    Err(CoreError::Refused(format!("no queue entry {id}")))
                }
            }
            QueueTarget::Path(path) => match self.resolve(path).await {
                Ok(root) => Ok(self.entries_under(root)),
                Err(CoreError::NotFound) => {
                    // A deleted file has no node at its path; its entry does.
                    let wanted = format!("/{}", path.trim_matches('/'));
                    let tree = self.tree.read();
                    let found: Vec<Ino> = self
                        .journal
                        .inos()
                        .into_iter()
                        .filter(|&ino| {
                            tree.get(ino).is_some_and(|node| node.deleted)
                                && entry_path(&tree, ino) == wanted
                        })
                        .collect();
                    if found.is_empty() {
                        Err(CoreError::NotFound)
                    } else {
                        Ok(found)
                    }
                }
                Err(error) => Err(error),
            },
        }
    }

    fn entries_under(&self, root: Ino) -> Vec<Ino> {
        let inos = self.journal.inos();
        let tree = self.tree.read();
        inos.into_iter()
            .filter(|&ino| entry_within(&tree, ino, root))
            .collect()
    }

    /// The selected entries, by inode. Those with nothing to do leave at
    /// the planner's next look and are left out, unless parked.
    pub async fn queue_entries(self: &Arc<Self>, target: &QueueTarget) -> Result<Vec<QueueEntry>> {
        let selected: BTreeSet<Ino> = self.queue_select(target).await?.into_iter().collect();
        let now = SystemTime::now();
        let mut entries = Vec::new();
        for view in self.journal.entries() {
            if !selected.contains(&view.ino) {
                continue;
            }
            let dependency = self.pending_dependency(view.ino);
            let tree = self.tree.read();
            let kind = kind_of(&tree, view.ino);
            if kind == QueueKind::Nothing && view.parked.is_none() && !view.in_flight {
                continue;
            }
            let state = match (view.parked, view.in_flight, view.backoff) {
                (Some(reason), _, _) => QueueState::Parked { reason },
                (None, true, _) => QueueState::Running,
                (None, false, Some(left)) => QueueState::Backoff { until: now + left },
                (None, false, None) => QueueState::Waiting,
            };
            let dependencies: BTreeSet<Ino> =
                view.blocked_on.into_iter().chain(dependency).collect();
            entries.push(QueueEntry {
                id: view.ino,
                path: entry_path(&tree, view.ino),
                kind,
                state,
                attempts: view.attempts,
                age: view.age,
                last_error: view.last_error,
                base_rev: tree
                    .get(view.ino)
                    .and_then(|node| node.remote.as_ref())
                    .and_then(|remote| remote.rev.as_ref())
                    .map(|rev| rev.0.clone()),
                dependencies: dependencies
                    .into_iter()
                    .map(|ino| entry_path(&tree, ino))
                    .collect(),
            });
        }
        Ok(entries)
    }

    fn queue_items(&self, inos: &[Ino]) -> Vec<QueueItem> {
        let tree = self.tree.read();
        inos.iter()
            .map(|&id| QueueItem {
                id,
                path: entry_path(&tree, id),
            })
            .collect()
    }

    /// Clears parks and back-offs and uploads now.
    pub async fn queue_retry(self: &Arc<Self>, target: &QueueTarget) -> Result<Vec<QueueItem>> {
        let selected = self.queue_select(target).await?;
        self.journal.retry(&selected);
        Ok(self.queue_items(&selected))
    }

    /// Holds entries until `queue retry` or the next mount (D061).
    pub async fn queue_park(self: &Arc<Self>, target: &QueueTarget) -> Result<Vec<QueueItem>> {
        let selected = self.queue_select(target).await?;
        self.journal.park_by_user(&selected);
        Ok(self.queue_items(&selected))
    }

    /// `sync [<path>]`: forces every entry under the path, or all, and waits
    /// until none is left (DESIGN.md §5.6).
    pub async fn sync_path(
        self: &Arc<Self>,
        path: Option<&str>,
        timeout: Duration,
    ) -> Result<SyncOutcome> {
        let root = match path {
            Some(path) => Some(self.resolve(path).await?),
            None => None,
        };
        let round = self
            .sync_entries(
                || match root {
                    Some(root) => self.entries_under(root),
                    None => self.journal.inos(),
                },
                timeout,
            )
            .await;
        let parked: BTreeMap<Ino, String> = self
            .journal
            .parked()
            .into_iter()
            .filter(|(ino, _)| round.parked.contains(ino))
            .collect();
        let tree = self.tree.read();
        Ok(SyncOutcome {
            pending: round.pending,
            parked: parked
                .into_iter()
                .map(|(ino, reason)| (entry_path(&tree, ino), reason))
                .collect(),
        })
    }

    /// The nodes `queue drop` touches with what it does to them: the
    /// selected entries, and the subtrees of folders that never reached the
    /// server, which leave with them.
    fn drop_plan(&self, selected: &[Ino]) -> BTreeMap<Ino, Undo> {
        let tree = self.tree.read();
        let undo_of = |ino: Ino| -> Option<Undo> {
            let node = tree.get(ino)?;
            if node.deleted {
                Some(Undo::Delete)
            } else if !node.attached {
                None
            } else if node.remote.is_none() {
                Some(Undo::Remove)
            } else {
                (kind_of(&tree, ino) != QueueKind::Nothing).then_some(Undo::Revert)
            }
        };
        let mut plan = BTreeMap::new();
        for &ino in selected {
            // Nothing queued to undo, and no other copy (`kind_of`).
            if tree
                .get(ino)
                .is_some_and(|node| node.local_only && node.remote.is_none())
            {
                continue;
            }
            let Some(undo) = undo_of(ino) else {
                continue;
            };
            plan.insert(ino, undo);
            if undo == Undo::Remove {
                for inner in tree.subtree(ino) {
                    if let Some(undo) = undo_of(inner) {
                        plan.insert(inner, undo);
                    }
                }
            }
        }
        plan
    }

    /// `queue drop` (D061): discards the local intent of the selected
    /// entries. Without `yes` it only says what it would do. Refused as a
    /// whole while a step of one of them runs. Otherwise a node that cannot
    /// be undone is reported and stays queued.
    pub async fn queue_drop(
        self: &Arc<Self>,
        target: &QueueTarget,
        yes: bool,
    ) -> Result<Vec<DropItem>> {
        let selected = self.queue_select(target).await?;
        let plan = self.drop_plan(&selected);
        let items = |refused: &BTreeMap<Ino, String>, tree: &Tree| -> Vec<DropItem> {
            plan.iter()
                .map(|(&id, undo)| DropItem {
                    id,
                    path: entry_path(tree, id),
                    effect: undo.effect(),
                    refused: refused.get(&id).cloned(),
                })
                .collect()
        };
        let inos: Vec<Ino> = plan.keys().copied().collect();
        // Named before anything changes: undoing a delete drops the node.
        let before = {
            let tree = self.tree.read();
            items(&BTreeMap::new(), &tree)
        };
        if !yes {
            let running: BTreeMap<Ino, String> = inos
                .iter()
                .filter(|&&ino| self.journal.in_flight(ino))
                .map(|&ino| (ino, "a step of it is running; retry later".to_owned()))
                .collect();
            let tree = self.tree.read();
            return Ok(items(&running, &tree));
        }
        if let Err(running) = self.journal.hold(&inos) {
            let path = entry_path(&self.tree.read(), running);
            return Err(CoreError::Refused(format!(
                "{path}: a step of it is running; retry later"
            )));
        }
        for (&ino, undo) in &plan {
            if *undo != Undo::Revert {
                continue;
            }
            let dir = {
                let tree = self.tree.read();
                tree.get(ino)
                    .and_then(|node| node.remote.as_ref())
                    .and_then(|remote| remote.parent_link.as_ref())
                    .and_then(|link| tree.by_link(link))
            };
            if let Some(dir) = dir
                && let Err(error) = self.ensure_listed(dir).await
            {
                tracing::debug!(dir, %error, "listing the old folder of a dropped move failed");
            }
        }
        let mut dirs = BTreeSet::new();
        let mut left: BTreeMap<Ino, Undo> = plan.clone();
        let mut refused = BTreeMap::new();
        // An undo can need another first: a file can go back to its name
        // once the new file holding that name is gone.
        loop {
            refused.clear();
            let before_round = left.len();
            for (ino, undo) in std::mem::take(&mut left) {
                match self.undo_entry(ino, undo, &plan, &mut dirs) {
                    Ok(()) => {}
                    Err(why) => {
                        refused.insert(ino, why);
                        left.insert(ino, undo);
                    }
                }
            }
            if left.is_empty() || left.len() == before_round {
                break;
            }
        }
        self.journal.release(&inos);
        for dir in dirs {
            let dir = self.tree.read().by_link(&dir);
            if let Some(dir) = dir
                && self.tree.read().get(dir).is_some_and(|node| node.attached)
                && let Err(error) = self.list(dir).await
            {
                tracing::debug!(dir, %error, "listing after a drop failed; listing later");
                self.list_later(vec![dir]);
            }
        }
        Ok(before
            .into_iter()
            .map(|item| DropItem {
                refused: refused.get(&item.id).cloned(),
                ..item
            })
            .collect())
    }

    /// Undoes one node; `dirs` gets the remote folders to list again. `Err`:
    /// why not (now).
    fn undo_entry(
        &self,
        ino: Ino,
        undo: Undo,
        plan: &BTreeMap<Ino, Undo>,
        dirs: &mut BTreeSet<LinkId>,
    ) -> std::result::Result<(), String> {
        match undo {
            Undo::Delete => {
                let (parent_link, changes) = {
                    let mut tree = self.tree.write();
                    let Some(node) = tree.get(ino) else {
                        return Ok(());
                    };
                    let parent_link = node
                        .remote
                        .as_ref()
                        .and_then(|remote| remote.parent_link.clone());
                    // A folder trashed with it would take it along again.
                    let parent = parent_link.as_ref().and_then(|link| tree.by_link(link));
                    if let Some(parent) = parent
                        && tree.get(parent).is_some_and(|dir| dir.deleted)
                        && !plan.contains_key(&parent)
                    {
                        return Err("its folder is deleted too; drop that entry".to_owned());
                    }
                    (parent_link, tree.unbind_tombstone(ino))
                };
                dirs.extend(parent_link);
                self.journal.forget(ino);
                if let Some(changes) = changes {
                    self.apply_changes(changes);
                }
            }
            Undo::Remove => {
                let changes = {
                    let mut tree = self.tree.write();
                    let Some(node) = tree.get(ino) else {
                        self.journal.forget(ino);
                        return Ok(());
                    };
                    if !node.attached {
                        self.journal.forget(ino);
                        return Ok(());
                    }
                    let parent = node.parent;
                    let linked = tree
                        .subtree(ino)
                        .find(|&inner| tree.link_of(inner).is_some());
                    if let Some(linked) = linked {
                        return Err(format!(
                            "{} inside has a remote copy and cannot go back yet",
                            entry_path(&tree, linked)
                        ));
                    }
                    let inner: Vec<Ino> = tree.subtree(ino).collect();
                    for inner in &inner {
                        self.journal.forget(*inner);
                    }
                    let mut changes = tree.remove_never_uploaded(ino);
                    changes.changed.push(parent);
                    let relist = tree.take_relist();
                    (changes, relist, tree.link_of(parent).cloned())
                };
                let (changes, relist, parent_link) = changes;
                self.list_later(relist);
                dirs.extend(parent_link);
                self.apply_changes(changes);
            }
            Undo::Revert => {
                let (parent_link, moved) = {
                    let mut tree = self.tree.write();
                    let Some(remote) = tree.get(ino).and_then(|node| node.remote.clone()) else {
                        return Ok(());
                    };
                    // The name is a step of the executor, not a place to go back to.
                    if remote.name.starts_with(".pdmount-swap-") {
                        return Err("it is in the middle of a name swap; retry later".to_owned());
                    }
                    let Some(moved) = tree.adopt_remote_placement(ino) else {
                        return Err(format!(
                            "its old place ({}) is taken or not available",
                            remote.name
                        ));
                    };
                    (remote.parent_link, moved)
                };
                dirs.extend(parent_link);
                self.discard_local_content(ino);
                self.journal.forget(ino);
                self.apply_changes(moved);
            }
        }
        Ok(())
    }

    /// The file goes back to its remote revision: the staging goes, what
    /// the kernel cached too (`readopt_remote_content` for local changes).
    fn discard_local_content(&self, ino: Ino) {
        let staged = self.staged_entry(ino);
        let staged_guard = staged.as_ref().map(|s| s.lock());
        let reverted = self.tree.write().revert_to_remote(ino);
        if !reverted {
            return;
        }
        let unstaged = staged.as_ref().and_then(|staged| {
            let mut map = self.staged.lock();
            map.get(&ino)
                .is_some_and(|entry| Arc::ptr_eq(entry, staged))
                .then(|| map.remove(&ino))
                .flatten()
        });
        drop(staged_guard);
        drop(unstaged);
        self.apply_changes(Changes {
            changed: vec![ino],
            invalidations: vec![Invalidation::Inode { ino }],
            ..Changes::default()
        });
        self.remove_staging_after_commit(ino);
    }
}
