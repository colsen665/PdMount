//! Nodes whose signatures do not verify (DESIGN.md §6.3, D132 amended): the
//! tree's record of them, the refusals, and `pdmount signatures list`,
//! `accept` and `check`.

use super::*;
use crate::config::SignatureMode;
use crate::signatures::{CheckCounts, FoundView};

/// The counts of a running `signatures check`, read while it runs.
#[derive(Debug, Default)]
pub struct CheckProgress {
    folders: AtomicU64,
    nodes: AtomicU64,
    unverified: AtomicU64,
    failed: AtomicU64,
}

impl CheckProgress {
    /// Adds counts found elsewhere (the Photos tree).
    pub fn add(&self, counts: CheckCounts) {
        self.folders.fetch_add(counts.folders, Ordering::Relaxed);
        self.nodes.fetch_add(counts.nodes, Ordering::Relaxed);
        self.unverified
            .fetch_add(counts.unverified, Ordering::Relaxed);
        self.failed.fetch_add(counts.failed, Ordering::Relaxed);
    }

    pub fn counts(&self) -> CheckCounts {
        CheckCounts {
            folders: self.folders.load(Ordering::Relaxed),
            nodes: self.nodes.load(Ordering::Relaxed),
            unverified: self.unverified.load(Ordering::Relaxed),
            failed: self.failed.load(Ordering::Relaxed),
        }
    }
}

impl Engine {
    /// Keeps the found nodes in step with rows being persisted (every
    /// change of the tree's remote states comes through here): a remote
    /// state with failed signatures is found, one without is not, and a
    /// link the tree no longer has goes. Announces the newly found ones.
    pub(super) fn observe_signatures(&self, rows: &[(Ino, Option<NodeRow>)]) {
        let mut found = 0;
        let mut unbound = false;
        for (_, row) in rows {
            match row.as_ref().and_then(|row| row.remote.as_ref()) {
                Some(remote) => {
                    if self
                        .signatures
                        .observe(&remote.link, remote.unverified.as_ref())
                    {
                        found += 1;
                    }
                }
                None => unbound = true,
            }
        }
        if unbound && self.signatures.count() > 0 {
            let tree = self.tree.read();
            self.signatures.sweep(|link| tree.by_link(link).is_some());
        }
        if found > 0 {
            tracing::info!(
                found,
                mode = self.signatures.mode().as_str(),
                "nodes whose signatures do not verify; see `pdmount signatures list`"
            );
            self.signatures.announce(found);
        }
    }

    /// The tree's mode for nodes whose signatures do not verify (D132).
    pub fn signature_mode(&self) -> SignatureMode {
        self.signatures.mode()
    }

    /// EACCES when the tree refuses `ino` (D132): in `refuse` mode, a node
    /// whose signatures do not verify, until accepted. For opening,
    /// renaming, moving, deleting and changing it.
    pub(super) fn ensure_verified(&self, ino: Ino) -> Result<()> {
        self.ensure_signatures(ino, false)
    }

    /// Creating in or moving into `dir` (D132): EACCES when its key or
    /// hash key did not verify, in `refuse` mode until accepted, since what
    /// is written there is encrypted to that key.
    pub(super) fn ensure_key_verified(&self, dir: Ino) -> Result<()> {
        self.ensure_signatures(dir, true)
    }

    fn ensure_signatures(&self, ino: Ino, key_only: bool) -> Result<()> {
        if self.signatures.mode() != SignatureMode::Refuse || self.signatures.count() == 0 {
            return Ok(());
        }
        let tree = self.tree.read();
        let Some(remote) = tree.get(ino).and_then(|node| node.remote.as_ref()) else {
            return Ok(());
        };
        let unverified = remote
            .unverified
            .as_ref()
            .filter(|unverified| !key_only || unverified.has(pdmount_backend::SignaturePart::Key));
        if self.signatures.refuses(&remote.link, unverified) {
            tracing::debug!(ino, "refused: its signatures do not verify (D132)");
            return Err(CoreError::AccessDenied);
        }
        Ok(())
    }

    /// [`Engine::ensure_verified`] for `name` in `parent`, if it exists.
    pub(super) fn ensure_child_verified(&self, parent: Ino, name: &str) -> Result<()> {
        let child = self.tree.read().child(parent, name)?;
        match child {
            Some(ino) => self.ensure_verified(ino),
            None => Ok(()),
        }
    }

    /// Newly found nodes, a count per batch (the notification, D132).
    pub fn subscribe_signatures(&self) -> tokio::sync::broadcast::Receiver<u64> {
        self.signatures.subscribe()
    }

    /// Found nodes now, and how many of them are refused.
    pub fn signature_counts(&self) -> (u64, u64) {
        let (found, refused) = self.signatures.counts();
        (found as u64, refused as u64)
    }

    /// `pdmount signatures list`: the found nodes, with their paths now.
    pub async fn signatures_list(&self) -> Result<Vec<FoundView>> {
        // What was persisted until now is in the table.
        self.store.flush().await?;
        let stored = self.store.read(crate::signatures::load)?;
        let tree = self.tree.read();
        Ok(crate::signatures::views(&self.signatures, stored, |link| {
            tree.by_link(link).and_then(|ino| tree.path(ino))
        }))
    }

    /// `pdmount signatures accept <id>`: the node is handled as verified on
    /// this device while its failures stay covered by the ones accepted
    /// now. `None` when no node of this tree has the id.
    pub async fn signature_accept(&self, id: &str) -> Result<Option<FoundView>> {
        let links = self.signatures.links_of(id);
        let link = match links.as_slice() {
            [] => return Ok(None),
            [link] => link.clone(),
            _ => {
                return Err(CoreError::Refused(format!(
                    "the id {id} names {} nodes; nothing was accepted",
                    links.len()
                )));
            }
        };
        let (digest, ino) = {
            let tree = self.tree.read();
            let ino = tree.by_link(&link);
            let digest = ino
                .and_then(|ino| tree.get(ino))
                .and_then(|node| node.remote.as_ref())
                .and_then(|remote| remote.unverified.as_ref())
                .map(|unverified| unverified.digest.clone());
            (digest, ino)
        };
        let Some(digest) = digest else {
            return Ok(None);
        };
        let stored = (link.clone(), digest.clone());
        self.store
            .write_durable(move |conn| {
                crate::signatures::store_accept(conn, &stored.0, &stored.1).map(|_| ())
            })
            .await?;
        self.signatures.set_accepted(&link, &digest);
        tracing::info!(
            ino,
            id,
            "a node whose signatures do not verify was accepted"
        );
        Ok(self
            .signatures_list()
            .await?
            .into_iter()
            .find(|view| view.link == link.0))
    }

    /// `pdmount signatures check [<path>]`: lists every folder under
    /// `path` (or the whole tree) from the server, one listing per
    /// background turn as a refresh does (§9.4, D027), so every node in it
    /// is decoded and checked; the folders stay listed. A file's folder is
    /// listed for it. `progress` counts as it goes.
    pub async fn signatures_check(
        self: &Arc<Self>,
        path: Option<&str>,
        progress: &CheckProgress,
    ) -> Result<CheckCounts> {
        let start = match path {
            Some(path) => self.resolve(path).await?,
            None => ROOT_INO,
        };
        let only = {
            let tree = self.tree.read();
            let node = tree.get(start).ok_or(CoreError::NotFound)?;
            (!node.is_dir()).then_some((node.parent, start))
        };
        let mut queue = std::collections::VecDeque::from([only.map_or(start, |(dir, _)| dir)]);
        while let Some(dir) = queue.pop_front() {
            if self.is_retired() {
                break;
            }
            let listed = {
                let _turn = self.gate.background().await;
                self.list(dir).await
            };
            match listed {
                Ok(()) => {}
                // Removed by the listing of an ancestor, or meanwhile.
                Err(CoreError::NotFound) => continue,
                Err(error) => {
                    tracing::warn!(dir, %error, "signatures check: a folder could not be listed");
                    progress.failed.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
            }
            progress.folders.fetch_add(1, Ordering::Relaxed);
            let tree = self.tree.read();
            let Some(node) = tree.get(dir) else {
                continue;
            };
            for &child in node.children.values() {
                if only.is_some_and(|(_, file)| file != child) {
                    continue;
                }
                let Some(child_node) = tree.get(child) else {
                    continue;
                };
                let Some(remote) = child_node.remote.as_ref() else {
                    continue;
                };
                progress.nodes.fetch_add(1, Ordering::Relaxed);
                if remote.unverified.is_some() {
                    progress.unverified.fetch_add(1, Ordering::Relaxed);
                }
                if only.is_none() && child_node.is_dir() {
                    queue.push_back(child);
                }
            }
        }
        let counts = progress.counts();
        tracing::info!(
            folders = counts.folders,
            nodes = counts.nodes,
            unverified = counts.unverified,
            failed = counts.failed,
            "signatures check finished"
        );
        Ok(counts)
    }
}
