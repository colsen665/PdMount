//! File content: open, read, write, truncate, flush and fsync (DESIGN.md §5, §7).

use super::*;

/// `[offset, offset + len)` from a fetched base piece that covers it.
pub(super) fn covering(fetched: &[(u64, ReadData)], offset: u64, len: u64) -> Option<&[u8]> {
    fetched.iter().find_map(|(start, data)| {
        let from = offset.checked_sub(*start)?;
        let to = from.checked_add(len)?;
        (to <= data.len() as u64).then(|| &data[from as usize..to as usize])
    })
}

/// The revision and clamped length of a read, or `None` at or past EOF.
pub(super) fn read_range(
    rev: Option<RevisionId>,
    file_size: u64,
    offset: u64,
    size: u32,
) -> Option<(RevisionId, u64)> {
    if offset >= file_size {
        return None;
    }
    Some((rev?, u64::from(size).min(file_size - offset)))
}

impl Engine {
    /// Opens a file. Returns the handle and whether the kernel may keep its
    /// cached pages (nothing replaced the content since the last open). The
    /// open counts for the bulk-read guard (D111).
    pub fn open_file(&self, ino: Ino, mode: OpenMode) -> Result<(u64, bool)> {
        self.open_handle(ino, mode, true)
    }

    /// [`Engine::open_file`]; `counted`: the open counts for the bulk-read
    /// guard (not the engine's own reads).
    pub(super) fn open_handle(
        &self,
        ino: Ino,
        mode: OpenMode,
        counted: bool,
    ) -> Result<(u64, bool)> {
        {
            let tree = self.tree.read();
            let node = tree.get(ino).ok_or(CoreError::NotFound)?;
            if node.is_dir() {
                return Err(CoreError::IsADirectory);
            }
        }
        if mode.write {
            self.ensure_writable(ino)?;
        }
        self.ensure_verified(ino)?;
        let want = if mode.read { MAY_READ } else { 0 } | if mode.write { MAY_WRITE } else { 0 };
        self.ensure_allowed(ino, want)?;
        if mode.write && mode.truncate {
            self.truncate(ino, 0)?;
        }
        let keep_cache = {
            let mut tree = self.tree.write();
            let node = tree.get_mut(ino).ok_or(CoreError::NotFound)?;
            let keep = node.served_epoch == Some(node.content_epoch) && !mode.truncate;
            node.served_epoch = Some(node.content_epoch);
            node.open_handles += 1;
            if mode.write {
                node.open_writers += 1;
            }
            keep
        };
        let scan = counted && self.opens_as_scan(ino, mode);
        Ok((self.add_handle(ino, mode.write, scan), keep_cache))
    }

    pub(super) fn add_handle(&self, ino: Ino, writable: bool, scan: bool) -> u64 {
        let handle = self.next_handle.fetch_add(1, Ordering::Relaxed);
        self.files.lock().insert(
            handle,
            OpenFile {
                ino,
                writable,
                sequential: Default::default(),
                scan,
                read_bytes: 0,
            },
        );
        handle
    }

    pub fn release_file(&self, handle: u64) {
        let Some(file) = self.files.lock().remove(&handle) else {
            return;
        };
        let droppable = {
            let mut tree = self.tree.write();
            let Some(node) = tree.get_mut(file.ino) else {
                return;
            };
            node.open_handles = node.open_handles.saturating_sub(1);
            if file.writable {
                node.open_writers = node.open_writers.saturating_sub(1);
            }
            tree.is_droppable(file.ino)
        };
        if file.writable {
            self.persist(&[file.ino]);
            self.journal.touch_content(file.ino);
            if self.is_root_rules(file.ino) {
                self.refresh_ignore();
            }
        }
        if droppable {
            self.drop_detached(file.ino);
        }
    }

    /// `flush` (close): returns an upload error deferred on this inode
    /// once (§7.8). Never waits for uploads.
    pub fn flush(&self, handle: u64) -> Result<()> {
        let ino = self.file(handle)?.ino;
        let error = self
            .tree
            .write()
            .get_mut(ino)
            .and_then(|node| node.deferred_error.take());
        match error {
            Some(errno) => {
                self.persist(&[ino]);
                Err(CoreError::Errno(errno))
            }
            None => Ok(()),
        }
    }

    /// Writes into staging. `NeedsMaterialize` means a partly overwritten
    /// base block must be downloaded first ([`Engine::write`]).
    pub fn write_local(&self, handle: u64, offset: u64, data: &[u8]) -> Result<u32> {
        let file = self.file(handle)?;
        if !file.writable {
            return Err(CoreError::Errno(nix::libc::EBADF));
        }
        // Handles opened before a switch to `ro` are refused too (§11.4).
        self.ensure_writable(file.ino)?;
        // Changes nothing (POSIX): not the size, not the times.
        if data.is_empty() {
            return Ok(0);
        }
        let current = self.tree.read().get(file.ino).map_or(0, |n| n.size);
        self.check_quota((offset + data.len() as u64).saturating_sub(current))?;
        let (first_change, back) = self.with_staged(file.ino, false, |staged| {
            if !staged
                .blocks_to_materialize_for_write(offset, data.len() as u64)
                .is_empty()
            {
                return Err(CoreError::NeedsMaterialize(file.ino));
            }
            staged.write(offset, data)?;
            let mut tree = self.tree.write();
            let node = tree.get_mut(file.ino).ok_or(CoreError::NotFound)?;
            let first = !node.content_pending();
            node.content = Content::Staged;
            node.size = staged.size();
            let time = now();
            node.mtime = time;
            node.ctime = time;
            node.gens.content += 1;
            Ok((first, tree.bring_back(file.ino, time)))
        })?;
        if first_change {
            self.persist(&[file.ino]);
        }
        self.brought_back(back);
        self.journal.touch_content(file.ino);
        Ok(data.len() as u32)
    }

    /// A file removed remotely and written through a handle opened before
    /// came back (D128): its rows, the kernel's entries, the journal, and
    /// a `RemoteDeleteLocalEdit` record.
    fn brought_back(&self, back: Option<(Ino, Changes)>) {
        let Some((top, changes)) = back else {
            return;
        };
        self.apply_changes(changes);
        // Not inside the call: `record_conflict` reads the tree too.
        let name = self
            .tree
            .read()
            .get(top)
            .map(|node| node.name.clone())
            .unwrap_or_default();
        self.record_conflict(ConflictNote {
            detail: Some("written after the remote delete; brought back".to_owned()),
            ..ConflictNote::new(ConflictKind::RemoteDeleteLocalEdit, top, name)
        });
    }

    /// Slow-path write: materializes what the write needs, then writes.
    pub async fn write(self: &Arc<Self>, handle: u64, offset: u64, data: Vec<u8>) -> Result<u32> {
        let _wait = self.user_wait();
        let ino = self.file(handle)?.ino;
        for _ in 0..8 {
            match self.write_local(handle, offset, &data) {
                Err(CoreError::NeedsMaterialize(_)) => {
                    let blocks = match self.staged_entry(ino) {
                        Some(staged) => staged
                            .lock()
                            .blocks_to_materialize_for_write(offset, data.len() as u64),
                        None => Vec::new(),
                    };
                    self.materialize(ino, &blocks, Priority::Interactive)
                        .await?;
                }
                result => return result,
            }
        }
        Err(CoreError::Internal(
            "write kept needing materialization".into(),
        ))
    }

    /// Changes a file's size; `NeedsMaterialize` when the cut falls inside a
    /// base block not downloaded yet.
    pub(super) fn truncate(&self, ino: Ino, size: u64) -> Result<()> {
        let current = self.tree.read().get(ino).map_or(0, |n| n.size);
        self.check_quota(size.saturating_sub(current))?;
        self.with_staged(ino, size == 0, |staged| {
            if !staged.blocks_to_materialize_for_truncate(size).is_empty() {
                return Err(CoreError::NeedsMaterialize(ino));
            }
            staged.truncate(size)?;
            let mut tree = self.tree.write();
            let node = tree.get_mut(ino).ok_or(CoreError::NotFound)?;
            node.content = Content::Staged;
            node.size = staged.size();
            let time = now();
            node.mtime = time;
            node.ctime = time;
            node.gens.content += 1;
            Ok(tree.bring_back(ino, time))
        })
        .map(|back| {
            self.persist(&[ino]);
            self.brought_back(back);
        })?;
        self.journal.touch_content(ino);
        Ok(())
    }

    /// Slow-path `setattr`: materializes the block a truncation cuts.
    pub async fn setattr_slow(self: &Arc<Self>, ino: Ino, change: SetAttr) -> Result<Attr> {
        let _wait = self.user_wait();
        for _ in 0..8 {
            match self.setattr(ino, &change) {
                Err(CoreError::NeedsMaterialize(_)) => {
                    let size = change.size.unwrap_or(0);
                    let blocks = match self.staged_entry(ino) {
                        Some(staged) => staged.lock().blocks_to_materialize_for_truncate(size),
                        None => Vec::new(),
                    };
                    self.materialize(ino, &blocks, Priority::Interactive)
                        .await?;
                }
                result => return result,
            }
        }
        Err(CoreError::Internal(
            "truncate kept needing materialization".into(),
        ))
    }

    /// Serves a read from RAM, staging or the disk cache, and says which.
    /// `None` means a block is missing and the slow path ([`Engine::read`])
    /// must run.
    pub fn read_local(
        self: &Arc<Self>,
        handle: u64,
        offset: u64,
        size: u32,
    ) -> Result<Option<(ReadData, ReadFrom)>> {
        let file = self.file(handle)?;
        match self.read_source(file.ino)? {
            ReadSource::Remote {
                link,
                rev,
                size: file_size,
            } => {
                let Some((rev, len)) = read_range(rev, file_size, offset, size) else {
                    return Ok(Some((ReadData::empty(), ReadFrom::Ram)));
                };
                let Some(layout) = self.cache.layout_local(&link, &rev, file_size) else {
                    return Ok(None);
                };
                let Some(read) = self.read_blocks_local(&link, &rev, &layout, offset, len) else {
                    return Ok(None);
                };
                self.readahead(handle, &link, &rev, &layout, offset, len);
                Ok(Some(read))
            }
            ReadSource::Staged(staged) => {
                let staged = staged.lock();
                let pieces = staged.plan_read(offset, u64::from(size));
                let base = staged.base().cloned();
                let mut out = Vec::new();
                for piece in pieces {
                    match piece {
                        Piece::Staging { offset, len } => {
                            let start = out.len();
                            out.resize(start + len as usize, 0);
                            staged.read_at(offset, &mut out[start..])?;
                        }
                        Piece::Base { offset, len } => {
                            let base = base.as_ref().expect("base pieces have a base");
                            match self.read_revision_local(base, offset, len) {
                                Some(data) => out.extend_from_slice(&data),
                                None => return Ok(None),
                            }
                        }
                    }
                }
                Ok(Some((ReadData::Owned(out), ReadFrom::Staging)))
            }
        }
    }

    /// Reads a range, downloading missing blocks. A scan read (D111) waits
    /// for its turn of the bulk-read guard and downloads at scan priority;
    /// any other read is one a user waits on, reported when it waits long
    /// ([`crate::slow`], D137).
    pub async fn read(self: &Arc<Self>, handle: u64, offset: u64, size: u32) -> Result<ReadData> {
        let file = self.file(handle)?;
        let scan = self.is_scan_read(&file);
        if scan {
            return self.read_through(handle, &file, scan, offset, size).await;
        }
        let ino = file.ino;
        let describe = || {
            let tree = self.tree.read();
            let node = tree.get(ino)?;
            Some((node.name.clone(), node.size))
        };
        self.scan
            .slow_reads()
            .watch(
                (self.scan_tree, ino),
                describe,
                self.read_through(handle, &file, scan, offset, size),
            )
            .await
    }

    /// [`Engine::read`]'s work: the remote content's layout and blocks, or
    /// the staging and its base.
    async fn read_through(
        self: &Arc<Self>,
        handle: u64,
        file: &OpenFile,
        scan: bool,
        offset: u64,
        size: u32,
    ) -> Result<ReadData> {
        let mut wait = (!scan).then(|| self.user_wait());
        for _ in 0..STAGED_READ_ATTEMPTS {
            let staged = match self.read_source(file.ino)? {
                ReadSource::Remote {
                    link,
                    rev,
                    size: file_size,
                } => {
                    let Some((rev, len)) = read_range(rev, file_size, offset, size) else {
                        return Ok(ReadData::empty());
                    };
                    let (priority, _turn) = if scan {
                        Metrics::inc(&self.metrics.scan_reads);
                        let turn = self.scan_turn(&link, &rev, file_size, offset, len).await;
                        (Priority::Scan, turn)
                    } else {
                        (Priority::Interactive, None)
                    };
                    let layout = self.cache.layout(&link, &rev, file_size).await?;
                    // Before its own download: a read that crosses into the
                    // next block starts the readahead beside it (D135).
                    self.readahead(handle, &link, &rev, &layout, offset, len);
                    let data = self
                        .read_layout(&link, &rev, &layout, offset, len, priority)
                        .await?;
                    return Ok(data);
                }
                ReadSource::Staged(staged) => staged,
            };
            // Local changes: read as a user's request.
            wait.get_or_insert_with(|| self.user_wait());
            if let Some(data) = self.read_staged(file.ino, &staged, offset, size).await? {
                return Ok(data);
            }
        }
        Err(CoreError::Internal(
            "the file's staging kept changing while reading".into(),
        ))
    }

    /// A read through `staged`; `None` when that is no longer the inode's
    /// staging, and the source has to be looked up again.
    pub(super) async fn read_staged(
        &self,
        ino: Ino,
        staged: &Arc<StagingLock>,
        offset: u64,
        size: u32,
    ) -> Result<Option<ReadData>> {
        // Fetch base pieces without holding the lock, then assemble. A
        // write or truncate meanwhile can split or move the base
        // pieces: plan again and fetch only what is still missing.
        let mut fetched: Vec<(u64, ReadData)> = Vec::new();
        let mut fetched_from: Option<Base> = None;
        for _ in 0..STAGED_READ_ATTEMPTS {
            let (pieces, base) = {
                let staged = staged.lock();
                (
                    staged.plan_read(offset, u64::from(size)),
                    staged.base().cloned(),
                )
            };
            if base != fetched_from {
                fetched.clear();
                fetched_from.clone_from(&base);
            }
            for piece in &pieces {
                if let (Piece::Base { offset, len }, Some(base)) = (piece, &base)
                    && covering(&fetched, *offset, *len).is_none()
                {
                    let data = self
                        .read_revision(base, *offset, *len, Priority::Interactive)
                        .await?;
                    fetched.push((*offset, data));
                }
            }
            let guard = staged.lock();
            if !self.is_current_staging(ino, staged) {
                return Ok(None);
            }
            let staged = guard;
            if staged.base() != fetched_from.as_ref() {
                continue;
            }
            let mut out = Vec::new();
            let mut complete = true;
            for piece in staged.plan_read(offset, u64::from(size)) {
                match piece {
                    Piece::Staging { offset, len } => {
                        let start = out.len();
                        out.resize(start + len as usize, 0);
                        staged.read_at(offset, &mut out[start..])?;
                    }
                    Piece::Base { offset, len } => match covering(&fetched, offset, len) {
                        Some(data) => out.extend_from_slice(data),
                        None => {
                            complete = false;
                            break;
                        }
                    },
                }
            }
            if complete {
                return Ok(Some(ReadData::Owned(out)));
            }
        }
        Err(CoreError::Internal(
            "staged read kept changing while fetching".into(),
        ))
    }

    /// `fsync` in `local` mode (§5.6): staging data and the store are on
    /// disk when it returns.
    pub async fn fsync(&self, ino: Ino) -> Result<()> {
        if let Some(staged) = self.staged_entry(ino) {
            let map = Arc::clone(&self.staged);
            let store = Arc::clone(&self.store);
            tokio::task::spawn_blocking(move || {
                let mut guard = staged.lock();
                // One replaced meanwhile was clean or dropped: nothing of it
                // needs to reach the disk.
                let current = map
                    .lock()
                    .get(&ino)
                    .is_some_and(|entry| Arc::ptr_eq(entry, &staged));
                if current {
                    store.outside_wal_sync(|| guard.sync())
                } else {
                    Ok(())
                }
            })
            .await
            .map_err(|e| CoreError::Internal(e.to_string()))??;
        }
        // Taken before the commit, so the cleared error is on disk too.
        let error = self
            .tree
            .write()
            .get_mut(ino)
            .and_then(|node| node.deferred_error.take());
        self.persist(&[ino]);
        self.store.flush().await?;
        match error {
            Some(errno) => Err(CoreError::Errno(errno)),
            None => Ok(()),
        }
    }

    /// Whether `fsync` of `ino` waits for the upload (§5.6).
    pub fn fsync_is_remote(&self, ino: Ino) -> bool {
        self.config.fsync.mode == crate::config::FsyncMode::Remote
            || (!self.fsync_remote.is_empty()
                && self
                    .tree
                    .read()
                    .path(ino)
                    .is_some_and(|path| self.fsync_remote.is_match(path)))
    }

    /// `fsyncdir`: the store is on disk when it returns.
    pub async fn fsyncdir(&self) -> Result<()> {
        self.store.flush().await?;
        Ok(())
    }

    /// The first `limit` bytes of a file.
    pub(super) async fn read_whole(self: &Arc<Self>, ino: Ino, limit: u64) -> Result<Vec<u8>> {
        let (handle, _) = self.open_handle(ino, OpenMode::default(), false)?;
        let size = self.getattr(ino).map_or(0, |attr| attr.size).min(limit);
        let result = async {
            let mut out = Vec::new();
            while (out.len() as u64) < size {
                let want = (size - out.len() as u64).min(1 << 20) as u32;
                // Not `read`: no user waits on this, so it reports no slow
                // download (D137).
                let file = self.file(handle)?;
                let scan = self.is_scan_read(&file);
                let chunk = self
                    .read_through(handle, &file, scan, out.len() as u64, want)
                    .await?;
                if chunk.is_empty() {
                    break;
                }
                out.extend_from_slice(&chunk);
            }
            Ok(out)
        }
        .await;
        self.release_file(handle);
        result
    }

    pub(super) fn file(&self, handle: u64) -> Result<OpenFile> {
        self.files
            .lock()
            .get(&handle)
            .cloned()
            .ok_or(CoreError::NotFound)
    }
}
