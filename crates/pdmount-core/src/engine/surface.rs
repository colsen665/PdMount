//! Surfacing conflicts (DESIGN.md §10.8) and the `user.pdmount.*` xattrs
//! (§8.7): the views behind `pdmount conflicts` and `getxattr`.

use super::{Engine, Result};
use crate::conflicts::{Conflict, ConflictKind, ConflictState, ConflictView, Resolution};
use crate::errors::CoreError;
use crate::vfs::{Ino, RenameMode};
use std::sync::Arc;
use std::time::Duration;

/// How long `keep-remote` waits for the local copy's upload before it
/// removes the copy.
const RESOLVE_UPLOAD_TIMEOUT: Duration = Duration::from_secs(120);

pub(super) const XATTR_STATE: &str = "user.pdmount.state";
pub(super) const XATTR_LINK: &str = "user.pdmount.link_id";
pub(super) const XATTR_REV: &str = "user.pdmount.rev";
pub(super) const XATTR_CONFLICT: &str = "user.pdmount.conflict";
pub(super) const XATTR_ACCESS: &str = "user.pdmount.access";

fn refused(text: impl Into<String>) -> CoreError {
    CoreError::Refused(text.into())
}

impl Engine {
    fn view(&self, conflict: Conflict) -> ConflictView {
        let (path, other_path) = {
            let tree = self.tree.read();
            let other = self.other_copy_in(&tree, &conflict);
            (
                tree.path(conflict.ino),
                other.and_then(|other| tree.path(other)),
            )
        };
        ConflictView {
            id: conflict.id,
            kind: conflict.kind,
            state: conflict.state,
            path,
            other_path,
            orig_name: conflict.orig_name,
            conflict_name: conflict.conflict_name,
            local_rev: conflict.local_rev,
            remote_rev: conflict.remote_rev,
            local_device: conflict.local_device,
            created_at: conflict.created_at,
            detail: conflict.detail,
        }
    }

    /// The inode of the other copy: the recorded one while it is in place,
    /// else the node now bound to the conflict's link.
    /// Conflicts that are not resolved and have `ino` as either copy,
    /// including the other copy known only by the conflict's link (§10.8).
    pub fn conflicts_for(&self, ino: Ino) -> Vec<Conflict> {
        let link = {
            let tree = self.tree.read();
            tree.get(ino)
                .and_then(|node| node.remote.as_ref())
                .map(|remote| remote.link.clone())
        };
        let candidates: Vec<Conflict> = self
            .conflicts
            .lock()
            .active_for(ino, link.as_ref())
            .into_iter()
            .cloned()
            .collect();
        let named = |c: &Conflict| c.ino == ino || c.other_ino == Some(ino);
        if candidates.iter().all(named) {
            return candidates;
        }
        let tree = self.tree.read();
        candidates
            .into_iter()
            .filter(|c| named(c) || self.other_copy_in(&tree, c) == Some(ino))
            .collect()
    }

    /// Listed folders where a conflict copy gave its name to a remote node
    /// the tree does not show: the listing after the yield (§10.6) did not
    /// happen, e.g. a restart cancelled it. The open conflict is the durable
    /// trace; listing the folder again shows the node (D055).
    pub(crate) fn folders_missing_an_other_copy(&self) -> Vec<Ino> {
        let candidates: Vec<Conflict> = self
            .conflicts
            .lock()
            .all()
            .filter(|c| {
                c.state != ConflictState::Resolved
                    && matches!(c.kind, ConflictKind::CreateCreate | ConflictKind::NameTaken)
                    && c.link.is_some()
            })
            .cloned()
            .collect();
        let tree = self.tree.read();
        let mut dirs: Vec<Ino> = candidates
            .iter()
            .filter(|c| self.other_copy_in(&tree, c).is_none())
            .filter_map(|c| tree.get(c.ino).filter(|node| node.attached))
            .map(|node| node.parent)
            .filter(|dir| tree.is_listed(*dir))
            .collect();
        dirs.sort_unstable();
        dirs.dedup();
        dirs
    }

    fn other_copy_in(&self, tree: &crate::vfs::Tree, conflict: &Conflict) -> Option<Ino> {
        let attached = |ino: &Ino| tree.get(*ino).is_some_and(|node| node.attached);
        conflict
            .other_ino
            .filter(attached)
            .or_else(|| {
                // `local_delete_remote_edit` names the old tombstone; the
                // file came back as a new inode.
                conflict
                    .link
                    .as_ref()
                    .and_then(|link| tree.by_link(link))
                    .filter(|ino| *ino != conflict.ino)
                    .filter(attached)
            })
            .filter(|ino| *ino != conflict.ino)
    }

    /// The conflicts that are not resolved, or all with `all`, at or below
    /// `under` (a path below the mount root), oldest first.
    pub async fn conflicts_list(
        self: &Arc<Self>,
        under: Option<&str>,
        all: bool,
    ) -> Result<Vec<ConflictView>> {
        let under = match under {
            Some(path) => Some(self.resolve(path).await?),
            None => None,
        };
        let conflicts: Vec<Conflict> = self
            .conflicts()
            .into_iter()
            .filter(|c| all || c.state != ConflictState::Resolved)
            .collect();
        let selected: Vec<Conflict> = match under {
            None => conflicts,
            Some(root) => {
                let tree = self.tree.read();
                conflicts
                    .into_iter()
                    .filter(|c| {
                        let other = self.other_copy_in(&tree, c);
                        [Some(c.ino), other]
                            .into_iter()
                            .flatten()
                            .any(|ino| tree.is_within(ino, root))
                    })
                    .collect()
            }
        };
        Ok(selected.into_iter().map(|c| self.view(c)).collect())
    }

    /// `target` is all digits for a conflict id, else a path, which names
    /// the newest unresolved conflict of that file.
    async fn find_conflict(self: &Arc<Self>, target: &str) -> Result<Conflict> {
        if !target.is_empty() && target.bytes().all(|b| b.is_ascii_digit()) {
            let id: i64 = target
                .parse()
                .map_err(|_| refused(format!("{target} is not a conflict id")))?;
            return self
                .conflicts
                .lock()
                .get(id)
                .cloned()
                .ok_or_else(|| refused(format!("no conflict {id}")));
        }
        let ino = self.resolve(target).await?;
        self.conflicts_for(ino)
            .into_iter()
            .max_by_key(|c| c.id)
            .ok_or_else(|| refused(format!("{target} has no unresolved conflict")))
    }

    pub async fn conflict_show(self: &Arc<Self>, target: &str) -> Result<ConflictView> {
        let conflict = self.find_conflict(target).await?;
        Ok(self.view(conflict))
    }

    /// Settles a conflict (§10.8) through the ordinary namespace operations,
    /// so the journal does the remote side, and marks it resolved.
    pub async fn conflict_resolve(
        self: &Arc<Self>,
        target: &str,
        choice: Resolution,
    ) -> Result<ConflictView> {
        let conflict = self.find_conflict(target).await?;
        if conflict.state == ConflictState::Resolved {
            return Err(refused(format!(
                "conflict {} is resolved already",
                conflict.id
            )));
        }
        match (conflict.kind, choice) {
            (_, Resolution::KeepBoth) => {}
            (
                ConflictKind::EditEdit | ConflictKind::CreateCreate | ConflictKind::NameTaken,
                Resolution::KeepLocal,
            ) => self.resolve_keep_local(&conflict)?,
            (
                ConflictKind::EditEdit | ConflictKind::CreateCreate | ConflictKind::NameTaken,
                Resolution::KeepRemote,
            ) => self.resolve_keep_remote(&conflict).await?,
            (ConflictKind::LocalDeleteRemoteEdit, Resolution::KeepLocal) => {
                // The file that came back goes to the trash again.
                let other = {
                    let tree = self.tree.read();
                    self.other_copy_in(&tree, &conflict)
                }
                .ok_or_else(|| {
                    refused("the other copy no longer exists; use keep-both".to_owned())
                })?;
                self.remove_node(other)?;
            }
            (ConflictKind::LocalDeleteRemoteEdit, Resolution::KeepRemote) => {}
            (kind, _) => {
                return Err(refused(format!(
                    "not applicable to {kind}; use keep-both to dismiss"
                )));
            }
        }
        let id = conflict.id;
        let resolved = self
            .conflicts
            .lock()
            .set_state(id, ConflictState::Resolved)
            .cloned()
            .ok_or_else(|| refused(format!("no conflict {id}")))?;
        self.store
            .write_durable(move |conn| {
                crate::conflicts::store_state(conn, id, ConflictState::Resolved)
            })
            .await?;
        Ok(self.view(resolved))
    }

    /// `(parent, name, is directory)` of an attached node.
    fn placement(&self, ino: Ino) -> Option<(Ino, String, bool)> {
        let tree = self.tree.read();
        let node = tree.get(ino).filter(|node| node.attached)?;
        Some((node.parent, node.name.clone(), node.is_dir()))
    }

    /// The local copy takes the original name; the other copy goes to the
    /// trash, or, when the local copy was never uploaded, is replaced by a
    /// new revision of it (`adopt_on_replace`).
    fn resolve_keep_local(&self, conflict: &Conflict) -> Result<()> {
        let (parent, name, _) = self
            .placement(conflict.ino)
            .ok_or_else(|| refused("the local copy no longer exists; use keep-both"))?;
        let other = {
            let tree = self.tree.read();
            self.other_copy_in(&tree, conflict)
        };
        let (new_parent, new_name) = match other {
            Some(other) => {
                let (new_parent, new_name, _) = self
                    .placement(other)
                    .ok_or_else(|| refused("the other copy no longer exists; use keep-both"))?;
                (new_parent, new_name)
            }
            None => return Err(refused("the other copy no longer exists; use keep-both")),
        };
        self.rename(
            parent,
            name.as_bytes(),
            new_parent,
            new_name.as_bytes(),
            RenameMode::Replace,
        )
    }

    /// The other copy stays; the local copy is removed once it is uploaded,
    /// so nothing is lost if the remove is undone from the trash.
    async fn resolve_keep_remote(self: &Arc<Self>, conflict: &Conflict) -> Result<()> {
        let ino = conflict.ino;
        if self.placement(ino).is_none() {
            return Err(refused("the local copy no longer exists; use keep-both"));
        }
        // It never uploads, so it would never reach the trash either.
        if self
            .tree
            .read()
            .get(ino)
            .is_some_and(|node| node.local_only)
        {
            return Err(refused(
                "the local copy is local-only (.pdignore) and has no remote copy to restore from; \
                 delete it yourself or use keep-both",
            ));
        }
        self.sync_remote(ino, RESOLVE_UPLOAD_TIMEOUT).await?;
        self.remove_node(ino)
    }

    fn remove_node(&self, ino: Ino) -> Result<()> {
        let (parent, name, is_dir) = self
            .placement(ino)
            .ok_or_else(|| refused("the copy no longer exists; use keep-both"))?;
        if is_dir {
            self.rmdir(parent, name.as_bytes())
        } else {
            self.unlink(parent, name.as_bytes())
        }
    }

    /// The `user.pdmount.*` names of `ino` that have a value (§8.7).
    pub fn xattr_names(&self, ino: Ino) -> Result<Vec<&'static str>> {
        let mut names = Vec::new();
        for name in [
            XATTR_STATE,
            XATTR_LINK,
            XATTR_REV,
            XATTR_CONFLICT,
            XATTR_ACCESS,
        ] {
            if self.xattr(ino, name)?.is_some() {
                names.push(name);
            }
        }
        Ok(names)
    }

    /// The value of a `user.pdmount.*` xattr; `None` when the name is
    /// unknown or has no value for `ino`. RAM only (hot path).
    pub fn xattr(&self, ino: Ino, name: &str) -> Result<Option<String>> {
        let conflicts = if name == XATTR_STATE || name == XATTR_CONFLICT {
            self.conflicts_for(ino)
        } else {
            Vec::new()
        };
        if name == XATTR_ACCESS {
            let access = self.access.read();
            let tree = self.tree.read();
            tree.get(ino).ok_or(CoreError::NotFound)?;
            if self.is_read_only()
                || !self.write_gate_open()
                || super::marker::in_marker_dir(&tree, ino)
            {
                return Ok(Some("ro".to_owned()));
            }
            return Ok(Some(
                crate::access::mode_str(access.effective(&tree, ino).0).to_owned(),
            ));
        }
        let queued = self.journal.entry_state(ino);
        let tree = self.tree.read();
        let node = tree.get(ino).ok_or(CoreError::NotFound)?;
        Ok(match name {
            XATTR_STATE => Some(
                if node.local_only {
                    "local-only"
                } else if !conflicts.is_empty() {
                    "conflict"
                } else if node.remote_newer {
                    "remote-newer"
                } else if queued.is_some_and(|(parked, _)| parked) {
                    "parked"
                } else if queued.is_some_and(|(_, in_flight)| in_flight) {
                    "uploading"
                } else if queued.is_some()
                    || node.gens.content > node.gens.uploaded_content
                    || node.gens.meta > node.gens.uploaded_meta
                {
                    "pending"
                } else {
                    "clean"
                }
                .to_owned(),
            ),
            XATTR_LINK => node.remote.as_ref().map(|remote| remote.link.0.clone()),
            XATTR_REV => node
                .remote
                .as_ref()
                .and_then(|remote| remote.rev.as_ref())
                .map(|rev| rev.0.clone()),
            XATTR_CONFLICT => conflicts.iter().max_by_key(|c| c.id).map(|c| {
                // The path of the copy that is not `ino`.
                let other = if c.ino == ino {
                    self.other_copy_in(&tree, c)
                } else {
                    Some(c.ino)
                };
                let other = other
                    .and_then(|other| tree.path(other))
                    .unwrap_or_else(|| "-".to_owned());
                format!("{} {} {other}", c.id, c.kind)
            }),
            _ => None,
        })
    }
}
