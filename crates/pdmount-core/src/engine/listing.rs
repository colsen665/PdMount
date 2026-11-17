//! Slow-path listings and the events glue: applying remote changes to the tree.

use super::*;

impl Engine {
    /// Lists `dir` if it was never listed, for a request a user waits on.
    pub async fn ensure_listed(self: &Arc<Self>, dir: Ino) -> Result<()> {
        let _wait = self.user_wait();
        self.ensure_listed_now(dir).await
    }

    pub(super) async fn ensure_listed_now(self: &Arc<Self>, dir: Ino) -> Result<()> {
        let listed = {
            let tree = self.tree.read();
            let node = tree.get(dir).ok_or(CoreError::NotFound)?;
            if !node.is_dir() {
                return Err(CoreError::NotADirectory);
            }
            node.listed
        };
        // Any listing will do, also one that started before this call.
        if listed {
            Ok(())
        } else {
            self.list_shared(dir).await?.1
        }
    }

    /// Fetches the children of `dir` and applies them, with a listing that
    /// started after this call (D054): one already in flight may predate the
    /// change the caller wants to see, so after joining it this lists again.
    pub async fn list(self: &Arc<Self>, dir: Ino) -> Result<()> {
        let asked = self.listing_epoch.load(Ordering::SeqCst);
        let (started, result) = self.list_shared(dir).await?;
        if started > asked {
            return result;
        }
        // The flight above has ended, so this one starts after the call.
        self.list_shared(dir).await?.1
    }

    /// Joins the listing of `dir` in flight, or starts one; answers with the
    /// number the listing took when it started.
    pub(super) async fn list_shared(self: &Arc<Self>, dir: Ino) -> Result<(u64, Result<()>)> {
        let this = Arc::clone(self);
        self.listings
            .run(dir, move || async move {
                let started = this.listing_epoch.fetch_add(1, Ordering::SeqCst) + 1;
                Ok((started, this.list_now(dir).await))
            })
            .await
    }

    pub(super) async fn list_now(self: &Arc<Self>, dir: Ino) -> Result<()> {
        let link = {
            let tree = self.tree.read();
            let node = tree.get(dir).ok_or(CoreError::NotFound)?;
            if !node.is_dir() {
                return Err(CoreError::NotADirectory);
            }
            match node.link() {
                Some(link) => link.clone(),
                // Created locally: complete by construction.
                None => return Ok(()),
            }
        };
        check_not_hot(&self.metrics, "list_children");
        Metrics::inc(&self.metrics.listings);
        let fetched_at = self.remote_clock.load(Ordering::SeqCst);
        let children = self.backend.list_children(&link).await?;
        let mut inos = Vec::new();
        loop {
            let unknown = self.tree.read().unknown_count(&children);
            if unknown > inos.len() {
                inos.extend(self.allocate_inos((unknown - inos.len()) as u64).await?);
            }
            let applied = self
                .tree
                .write()
                .apply_listing_at(dir, &children, &mut inos, fetched_at);
            match applied {
                Ok(changes) => {
                    let vanished = self.apply_changes(changes);
                    if !vanished.is_empty()
                        && let Err(error) = self.resolve_vanished(vanished).await
                    {
                        tracing::warn!(dir, %error, "resolving nodes removed remotely failed");
                    }
                    return Ok(());
                }
                Err(ApplyError::NeedInos(missing)) => {
                    // `unknown_count` should have covered it; never spin.
                    inos.extend(self.allocate_inos(missing as u64).await?);
                    continue;
                }
                Err(ApplyError::NotADirectory) => return Err(CoreError::NotFound),
            }
        }
    }

    /// Persists and announces the effects of a tree change. Returns the
    /// nodes whose remote counterpart vanished while they have local
    /// changes; [`Engine::resolve_vanished`] decides what happens to them.
    pub(crate) fn apply_changes(&self, changes: Changes) -> Vec<Ino> {
        let Changes {
            changed,
            removed,
            invalidations,
            vanished,
            clashes,
            notes,
            remote_newer,
        } = changes;
        let mut rows = changed;
        rows.extend(&removed);
        rows.extend(&remote_newer);
        if rows.contains(&ROOT_INO) {
            // The root was listed: `/.pdignore` may have changed.
            self.refresh_ignore();
        }
        self.persist(&rows);
        if !rows.is_empty() {
            self.pins_changed();
        }
        for ino in &rows {
            if self.journal_relevant(*ino) {
                self.journal.touch(*ino);
            }
        }
        for ino in removed {
            if self.tree.read().get(ino).is_none() {
                self.unstage(ino);
                self.remove_staging_after_commit(ino);
            }
        }
        for invalidation in invalidations {
            let _ = self.invalidations.send(invalidation);
        }
        for note in notes {
            self.record_conflict(note);
        }
        // The journal settles a clash when it next looks at the node
        // (identical content never yields, D016).
        for ino in clashes {
            self.journal.touch(ino);
        }
        for ino in remote_newer {
            self.journal.touch(ino);
            let path = self.tree.read().path(ino);
            tracing::info!(
                ino,
                path = path.as_deref().unwrap_or(""),
                "a newer remote revision exists than the one this file is based on"
            );
            // Id 0: the conflict is not registered yet; the upload does that.
            let _ = self.notices.send(ConflictNotice {
                id: 0,
                kind: ConflictKind::EditEdit,
                path,
            });
        }
        vanished
    }

    /// Settles nodes that were removed or moved remotely while they have
    /// local changes (§10.5): a removed subtree is kept as its minimal chain
    /// of pending items and uploaded again; a node that only moved follows
    /// the move at the next listing of its new folder.
    ///
    /// Never awaits a listing (the caller may hold one): listings of other
    /// folders are spawned.
    pub(crate) fn resolve_vanished(
        self: &Arc<Self>,
        inos: Vec<Ino>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>> {
        // Boxed: a listing resolves, and resolving spawns listings.
        let this = Arc::clone(self);
        Box::pin(async move { this.resolve_vanished_now(inos).await })
    }

    pub(super) async fn resolve_vanished_now(self: &Arc<Self>, inos: Vec<Ino>) -> Result<()> {
        let mut seen = HashSet::new();
        let (root, starts) = {
            let tree = self.tree.read();
            let starts: Vec<(Ino, LinkId)> = inos
                .into_iter()
                .filter(|ino| seen.insert(*ino))
                .filter_map(|ino| {
                    let node = tree.get(ino).filter(|node| node.attached)?;
                    Some((ino, node.link()?.clone()))
                })
                .collect();
            (tree.link_of(ROOT_INO).cloned(), starts)
        };
        if starts.is_empty() {
            return Ok(());
        }

        // Fresh state of every link on the way from each node up to the
        // root (trashed nodes keep their parent; a child of a trashed folder
        // is not reported trashed itself).
        let mut fetched: HashMap<LinkId, Option<RemoteNode>> = HashMap::new();
        let mut chains: Vec<Vec<LinkId>> =
            starts.iter().map(|(_, link)| vec![link.clone()]).collect();
        loop {
            let missing: Vec<LinkId> = {
                let mut unique = HashSet::new();
                chains
                    .iter()
                    .flatten()
                    .filter(|link| !fetched.contains_key(*link) && unique.insert((*link).clone()))
                    .cloned()
                    .collect()
            };
            if missing.is_empty() {
                break;
            }
            // No gate turn of its own: this runs inside a listing, which in
            // the background already holds a turn (events, revalidation), and
            // in the foreground blocks a waiting user; a nested turn would
            // wait for itself.
            for batch in missing.chunks(crate::events::NODE_BATCH) {
                check_not_hot(&self.metrics, "get_nodes");
                let nodes = self.backend.get_nodes(batch).await?;
                fetched.extend(batch.iter().cloned().zip(nodes));
            }
            for chain in &mut chains {
                let last = chain.last().expect("chains are not empty");
                let Some(Some(node)) = fetched.get(last) else {
                    continue;
                };
                let Some(parent) = &node.parent_link_id else {
                    continue;
                };
                if Some(parent) != root.as_ref() && !chain.contains(parent) {
                    chain.push(parent.clone());
                }
            }
        }
        let gone = |link: &LinkId| {
            fetched.get(link).is_none_or(|node| node.is_none())
                || fetched
                    .get(link)
                    .and_then(Option::as_ref)
                    .is_some_and(|node| node.trashed)
        };

        let mut targets = Vec::new();
        let mut relist = Vec::new();
        let mut displaced = Vec::new();
        {
            let tree = self.tree.read();
            for ((ino, link), chain) in starts.iter().zip(&chains) {
                // The highest removed element decides how much goes.
                if let Some(highest) = chain.iter().rev().find(|link| gone(link)) {
                    let top = tree
                        .by_link(highest)
                        .filter(|&top| tree.is_within(*ino, top));
                    targets.push(top.unwrap_or(*ino));
                    continue;
                }
                // Nothing is gone: the node moved remotely.
                let Some(Some(fresh)) = fetched.get(link) else {
                    continue;
                };
                let known_parent = tree
                    .get(*ino)
                    .and_then(|node| node.remote.as_ref())
                    .and_then(|remote| remote.parent_link.clone());
                if fresh.parent_link_id == known_parent {
                    // The listing and the node disagree about one folder;
                    // the next listing settles it.
                    tracing::debug!(ino, "a node missing from its listing exists remotely");
                    continue;
                }
                match fresh.parent_link_id.as_ref().and_then(|p| tree.by_link(p)) {
                    Some(dir) => relist.push(dir),
                    None => displaced.push((*ino, fresh.clone())),
                }
            }
        }

        for dir in relist {
            let this = Arc::clone(self);
            self.runtime.spawn(async move {
                if let Err(error) = this.list(dir).await {
                    tracing::warn!(dir, %error, "listing a folder that received a moved node failed");
                }
            });
        }
        for (ino, fresh) in displaced {
            let stamp = self.stamp();
            let mut tree = self.tree.write();
            if tree.get(ino).and_then(|node| node.link()) != Some(&fresh.link_id) {
                continue;
            }
            let orig_name = tree
                .get(ino)
                .map(|node| node.name.clone())
                .unwrap_or_default();
            tree.set_remote_at(ino, Some(RemoteState::from_remote(&fresh)), stamp);
            drop(tree);
            self.persist(&[ino]);
            self.journal.touch(ino);
            self.record_conflict(ConflictNote {
                link: Some(fresh.link_id.clone()),
                remote_rev: fresh.active_revision.clone(),
                detail: Some(
                    "moved remotely to a folder not shown here; local placement kept".to_owned(),
                ),
                ..ConflictNote::new(ConflictKind::RenameRename, ino, orig_name)
            });
        }

        // Ancestors first; a target inside another one is covered by it.
        targets.sort_unstable();
        targets.dedup();
        let covered: HashSet<Ino> = {
            let tree = self.tree.read();
            targets
                .iter()
                .copied()
                .filter(|&target| {
                    targets
                        .iter()
                        .any(|&other| other != target && tree.is_within(target, other))
                })
                .collect()
        };
        for target in targets
            .into_iter()
            .filter(|target| !covered.contains(target))
        {
            let (resurrected, renamed) = {
                let mut tree = self.tree.write();
                // Settled meanwhile (another listing, an event).
                if !tree
                    .get(target)
                    .is_some_and(|node| node.attached && node.remote.is_some())
                {
                    continue;
                }
                let renamed = tree.renamed_in(target);
                (tree.resurrect_minimal(target), renamed)
            };
            let crate::vfs::Resurrected {
                changes,
                kept,
                removed_renamed,
            } = resurrected;
            let removed = changes.removed.len();
            self.apply_changes(changes);
            if !kept.is_empty() {
                // Not inside the call: a guard in its arguments lives until
                // it returns, and `record_conflict` reads the tree too.
                let name = self
                    .tree
                    .read()
                    .get(target)
                    .map(|node| node.name.clone())
                    .unwrap_or_default();
                self.record_conflict(ConflictNote {
                    detail: Some(format!("kept {}, removed {removed}", kept.len())),
                    ..ConflictNote::new(ConflictKind::RemoteDeleteLocalEdit, target, name)
                });
            }
            for ino in removed_renamed {
                let Some((_, name, link)) = renamed.iter().find(|(renamed, ..)| *renamed == ino)
                else {
                    continue;
                };
                self.record_conflict(ConflictNote {
                    link: link.clone(),
                    detail: Some(
                        "renamed here; deleted on another device, the delete won".to_owned(),
                    ),
                    ..ConflictNote::new(ConflictKind::RemoteDeleteLocalRename, ino, name.clone())
                });
            }
        }
        Ok(())
    }

    pub(super) fn journal_relevant(&self, ino: Ino) -> bool {
        self.tree.read().has_pending_work(ino)
    }
}
