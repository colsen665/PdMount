//! Staging of files being written: quota, staged entries, materialization of base blocks.

use super::*;

/// [`Engine::remove_staging_after_commit`]: takes the queue before the flush,
/// so every inode taken had its rows queued before the flush.
pub(super) async fn remove_committed_staging(
    store: Arc<Store>,
    staging: StagingDir,
    staged: Arc<lock_order::Mutex<StagedMap>>,
    removals: Arc<parking_lot::Mutex<Vec<Ino>>>,
) {
    let inos = std::mem::take(&mut *removals.lock());
    if inos.is_empty() {
        return;
    }
    if let Err(error) = store.flush().await {
        // Startup deletes the staging of nodes that are gone.
        tracing::warn!(%error, count = inos.len(), "staging kept: the store did not commit");
        return;
    }
    // Locked per inode: `create` waits for one removal, not the batch.
    for ino in inos {
        let staged = staged.lock();
        if !staged.contains_key(&ino) {
            staging.remove(ino);
        }
    }
}

impl Engine {
    /// ENOSPC when growing by `growth` bytes would exceed the quota, counting
    /// what is still waiting for upload (§7.8). Unknown quota passes.
    pub(super) fn check_quota(&self, growth: u64) -> Result<()> {
        if growth == 0 {
            return Ok(());
        }
        let Some(storage) = *self.storage.lock() else {
            return Ok(());
        };
        let mut cached = self.pending_bytes.lock();
        let (summed_at, summed, since) = match *cached {
            Some(entry) if entry.0.elapsed() < PENDING_BYTES_TTL => entry,
            _ => (std::time::Instant::now(), self.pending_upload_bytes(), 0),
        };
        if storage
            .used
            .saturating_add(summed)
            .saturating_add(since)
            .saturating_add(growth)
            > storage.max
        {
            return Err(CoreError::NoSpace);
        }
        *cached = Some((summed_at, summed, since + growth));
        Ok(())
    }

    pub(super) fn pending_upload_bytes(&self) -> u64 {
        let inos = self.journal.inos();
        let tree = self.tree.read();
        inos.iter()
            .filter_map(|ino| tree.get(*ino))
            .filter(|node| node.content_pending())
            .map(|node| node.size)
            .sum()
    }

    pub(crate) fn staged_entry(&self, ino: Ino) -> Option<Arc<StagingLock>> {
        self.staged.lock().get(&ino).cloned()
    }

    /// Whether `staged` still is the staging of `ino`. A [`Staged`] goes
    /// through the path and the descriptor of its inode, which by then may
    /// be a newer staging's: whoever held one across a wait checks this
    /// with its lock held before touching the file. An upload commit or
    /// `queue drop` replaces it under the same lock.
    pub(super) fn is_current_staging(&self, ino: Ino, staged: &Arc<StagingLock>) -> bool {
        self.staged
            .lock()
            .get(&ino)
            .is_some_and(|entry| Arc::ptr_eq(entry, staged))
    }

    /// The staging of a file, created from its remote content if needed
    /// (local disk only).
    pub(crate) fn ensure_staged(&self, ino: Ino) -> Result<Arc<StagingLock>> {
        self.stage(ino, false)
    }

    /// Runs `change` on the current staging of `ino` with its lock held
    /// (`empty`: a new staging starts empty, nothing of the old content is
    /// used, for `O_TRUNC`),
    /// so that a change to the staging and to the node is one step for an
    /// upload commit, which drops a clean staging under the same lock: a
    /// write between its check and the drop would be lost (fsx, M2.9).
    pub(super) fn with_staged<T>(
        &self,
        ino: Ino,
        empty: bool,
        change: impl FnOnce(&mut Staged) -> Result<T>,
    ) -> Result<T> {
        for _ in 0..8 {
            let staged = self.stage(ino, empty)?;
            let mut guard = staged.lock();
            if self.is_current_staging(ino, &staged) {
                return change(&mut guard);
            }
        }
        Err(CoreError::Internal(format!(
            "the staging of inode {ino} kept changing"
        )))
    }

    pub(super) fn stage(&self, ino: Ino, empty: bool) -> Result<Arc<StagingLock>> {
        let mut map = self.staged.lock();
        if let Some(staged) = map.get(&ino) {
            return Ok(Arc::clone(staged));
        }
        let (base_rev, base, size) = {
            let tree = self.tree.read();
            let node = tree.get(ino).ok_or(CoreError::NotFound)?;
            if node.is_dir() {
                return Err(CoreError::IsADirectory);
            }
            let remote_rev = node.remote.as_ref().and_then(|r| r.rev.clone());
            match &node.content {
                Content::Remote {
                    link,
                    rev: Some(rev),
                } if !empty && node.size > 0 => (
                    remote_rev,
                    Some(Base {
                        link: link.clone(),
                        rev: rev.clone(),
                        rev_size: node.size,
                        size: node.size,
                    }),
                    node.size,
                ),
                _ => (remote_rev, None, if empty { 0 } else { node.size }),
            }
        };
        // The size comes from the uploader's claim (§7.1); the sparse file
        // and the materialization must stay within reason.
        if size > MAX_STAGED_SIZE {
            return Err(CoreError::FileTooLarge);
        }
        let staged = Arc::new(StagingLock::new(
            Rank::Staging,
            self.staging.create(ino, base_rev, base, size)?,
        ));
        map.insert(ino, Arc::clone(&staged));
        drop(map);
        if let Some(node) = self.tree.write().get_mut(ino) {
            node.content = Content::Staged;
        }
        Ok(staged)
    }

    /// Downloads base blocks into staging.
    pub(crate) async fn materialize(
        &self,
        ino: Ino,
        blocks: &[u64],
        priority: Priority,
    ) -> Result<()> {
        let Some(staged) = self.staged_entry(ino) else {
            return Ok(());
        };
        for &index in blocks {
            let (base, range) = {
                let staged = staged.lock();
                (staged.base().cloned(), staged.base_range(index))
            };
            let (Some(base), Some((start, end))) = (base, range) else {
                continue;
            };
            let data = self
                .read_revision(&base, start, end - start, priority)
                .await?;
            let mut guard = staged.lock();
            if !self.is_current_staging(ino, &staged) {
                return Ok(());
            }
            guard.materialize(index, &base, &data)?;
        }
        Ok(())
    }

    /// Bytes `[offset, offset + len)` of a base revision, from the cache.
    pub(super) fn read_revision_local(
        &self,
        base: &Base,
        offset: u64,
        len: u64,
    ) -> Option<ReadData> {
        let layout = self
            .cache
            .layout_local(&base.link, &base.rev, base.rev_size)?;
        self.read_blocks_local(&base.link, &base.rev, &layout, offset, len)
            .map(|(data, _)| data)
    }

    /// Bytes `[offset, offset + len)` of a revision from RAM or disk: a
    /// slice of the block it lies in, or assembled in a scratch slot
    /// (§6.2). `None` if a block is missing.
    pub(super) fn read_blocks_local(
        &self,
        link: &LinkId,
        rev: &RevisionId,
        layout: &Layout,
        offset: u64,
        len: u64,
    ) -> Option<(ReadData, ReadFrom)> {
        let key = |index| BlockKey::new(link, rev, index);
        match layout.spans(offset, len) {
            [] => Some((ReadData::empty(), ReadFrom::Ram)),
            [span] => {
                let (data, from) = self.cache.get_local_from(&key(span.index))?;
                Some((ReadData::of_block(span, data, offset, len), from))
            }
            spans => {
                let mut out = self.cache.read_buf_now(offset, len);
                let mut from = ReadFrom::Ram;
                for span in spans {
                    let (data, source) = self.cache.get_local_from(&key(span.index))?;
                    out.append(span, &data);
                    from = from.max(source);
                }
                Some((out.finish(), from))
            }
        }
    }

    /// Bytes `[offset, offset + len)` of a base revision, downloading
    /// missing blocks.
    pub(crate) async fn read_revision(
        &self,
        base: &Base,
        offset: u64,
        len: u64,
        priority: Priority,
    ) -> Result<ReadData> {
        let layout = self
            .cache
            .layout(&base.link, &base.rev, base.rev_size)
            .await?;
        self.read_layout(&base.link, &base.rev, &layout, offset, len, priority)
            .await
    }

    /// [`Engine::read_blocks_local`], downloading missing blocks. A read
    /// across blocks takes its scratch slot once the first block is here.
    pub(super) async fn read_layout(
        &self,
        link: &LinkId,
        rev: &RevisionId,
        layout: &Layout,
        offset: u64,
        len: u64,
        priority: Priority,
    ) -> Result<ReadData> {
        let key = |index| BlockKey::new(link, rev, index);
        let spans = match layout.spans(offset, len) {
            [] => return Ok(ReadData::empty()),
            [span] => {
                let data = self.cache.get(&key(span.index), span.len, priority).await?;
                return Ok(ReadData::of_block(span, data, offset, len));
            }
            spans => spans,
        };
        // Futures that own their inputs, so the stream is `Send`.
        let fetches: Vec<_> = spans
            .iter()
            .map(|span| {
                let (cache, key, block_len) = (Arc::clone(&self.cache), key(span.index), span.len);
                async move { cache.get(&key, block_len, priority).await }
            })
            .collect();
        let mut blocks = futures::stream::iter(fetches).buffered(READ_BLOCKS_HELD);
        let mut out = None;
        for span in spans {
            let data = blocks.next().await.expect("one block per span")?;
            let out = match &mut out {
                Some(out) => out,
                None => out.insert(self.cache.read_buf(offset, len).await),
            };
            out.append(span, &data);
        }
        Ok(out.map_or_else(ReadData::empty, |out| out.finish()))
    }

    pub(super) fn read_source(&self, ino: Ino) -> Result<ReadSource> {
        let (content, size) = {
            let tree = self.tree.read();
            let node = tree.get(ino).ok_or(CoreError::NotFound)?;
            (node.content.clone(), node.size)
        };
        match content {
            Content::Remote { link, rev } => Ok(ReadSource::Remote { link, rev, size }),
            Content::Staged => self
                .staged_entry(ino)
                .map(ReadSource::Staged)
                .ok_or_else(|| CoreError::Internal(format!("inode {ino} has no staging"))),
        }
    }

    /// Removes a detached node nobody references any more, with its
    /// staging.
    pub(crate) fn drop_detached(&self, ino: Ino) {
        let dropped = {
            let mut tree = self.tree.write();
            if tree.is_droppable(ino) && !self.journal.in_flight(ino) {
                tree.drop_node(ino);
                true
            } else {
                false
            }
        };
        if dropped {
            self.unstage(ino);
            self.persist(&[ino]);
            self.remove_staging_after_commit(ino);
        }
    }

    /// Drops the staged entry of `ino`. Its file is closed after the map
    /// lock is released: on btrfs a close can wait for a commit in progress,
    /// and `create` needs the lock.
    pub(crate) fn unstage(&self, ino: Ino) {
        let staged = self.staged.lock().remove(&ino);
        drop(staged);
    }

    /// Deletes the staging of `ino` once the store rows queued so far are
    /// durable, unless the inode has a new staging by then: a crash before
    /// that finds the stored rows still naming it. Queue the rows first.
    /// Removals within [`STAGING_REMOVAL_DELAY`] share one fsync.
    pub(crate) fn remove_staging_after_commit(&self, ino: Ino) {
        let first = {
            let mut removals = self.staging_removals.lock();
            removals.push(ino);
            removals.len() == 1
        };
        if first {
            let removal = remove_committed_staging(
                Arc::clone(&self.store),
                self.staging.clone(),
                Arc::clone(&self.staged),
                Arc::clone(&self.staging_removals),
            );
            self.runtime.spawn(async move {
                tokio::time::sleep(STAGING_REMOVAL_DELAY).await;
                removal.await;
            });
        }
    }

    /// Queues the current rows of `inos` (deleting rows of nodes that no
    /// longer need one).
    pub(crate) fn persist(&self, inos: &[Ino]) {
        let rows: Vec<(Ino, Option<NodeRow>)> = {
            let tree = self.tree.read();
            let mut seen = HashSet::new();
            inos.iter()
                .filter(|ino| seen.insert(**ino))
                .map(|&ino| {
                    let row = tree.persisted(ino).then(|| tree.row(ino)).flatten();
                    (ino, row)
                })
                .collect()
        };
        if rows.is_empty() {
            return;
        }
        self.store.write(move |conn| write_rows(conn, &rows));
    }
}
