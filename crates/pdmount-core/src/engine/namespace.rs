//! Hot-path lookups, attributes and namespace changes (DESIGN.md §8).

use super::*;

impl Engine {
    pub fn lookup(&self, parent: Ino, name: &[u8]) -> Result<Lookup> {
        let name = match crate::vfs::validate_name(name) {
            Ok(name) => name,
            // A name that cannot be valid UTF-8 cannot exist remotely.
            Err(CoreError::InvalidName | CoreError::InvalidArgument) => {
                return Ok(Lookup::Negative);
            }
            Err(error) => return Err(error),
        };
        self.ensure_allowed(parent, MAY_EXEC)?;
        let lookup = self.tree.write().lookup(parent, name)?;
        Ok(match lookup {
            Lookup::Found(attr) => Lookup::Found(self.masked(attr)),
            other => other,
        })
    }

    pub fn getattr(&self, ino: Ino) -> Result<Attr> {
        let attr = self
            .tree
            .read()
            .get(ino)
            .map(|node| node.attr())
            .ok_or(CoreError::NotFound)?;
        Ok(self.masked(attr))
    }

    /// The attributes of `parent`'s child `name` without a kernel
    /// reference: for a virtual folder that lists one entry of a tree (a
    /// shared file, D112). `NeedsListing` when `parent` was never listed.
    pub fn peek_child(&self, parent: Ino, name: &str) -> Result<Option<Attr>> {
        let attr = {
            let tree = self.tree.read();
            tree.child(parent, name)?
                .and_then(|ino| tree.get(ino))
                .map(|node| node.attr())
        };
        Ok(attr.map(|attr| self.masked(attr)))
    }

    /// The inodes the kernel holds a reference to: what a mount drops from
    /// the kernel's caches when the tree leaves it (a revoked share, D112).
    pub fn kernel_inos(&self) -> Vec<Ino> {
        let tree = self.tree.read();
        tree.inos()
            .filter(|&ino| tree.get(ino).is_some_and(|node| node.nlookup > 0))
            .collect()
    }

    pub fn forget(&self, ino: Ino, count: u64) {
        let droppable = self.tree.write().forget(ino, count);
        if droppable {
            self.drop_detached(ino);
        }
    }

    /// Opens a listed directory; `NeedsListing` when it was never listed.
    /// Returns the handle and whether the kernel may keep the directory
    /// pages it cached: the entries are the ones it was given at the last
    /// open, and no other handle of the directory is open, whose older
    /// snapshot could fill the cache after a change (§5.4).
    pub fn opendir(self: &Arc<Self>, ino: Ino) -> Result<(u64, bool)> {
        self.ensure_allowed(ino, MAY_READ)?;
        self.ensure_verified(ino)?;
        let entries = self.tree.read().dir_entries(ino)?;
        let digest = {
            let mut hasher = std::hash::DefaultHasher::new();
            for entry in &entries {
                (entry.ino, entry.kind == NodeKind::Folder, &entry.name).hash(&mut hasher);
            }
            hasher.finish()
        };
        let handle = self.next_handle.fetch_add(1, Ordering::Relaxed);
        let keep_cache = {
            let mut dirs = self.dirs.lock();
            let alone = !dirs.open.values().any(|(open, _)| *open == ino);
            dirs.open.insert(handle, (ino, Arc::new(entries)));
            if alone {
                dirs.served.insert(ino, digest) == Some(digest)
            } else {
                dirs.served.remove(&ino);
                false
            }
        };
        self.prefetch_dir(ino);
        Ok((handle, keep_cache))
    }

    /// The snapshot taken by `opendir`.
    pub fn dir_snapshot(&self, handle: u64) -> Result<Arc<Vec<DirEntry>>> {
        self.dirs
            .lock()
            .open
            .get(&handle)
            .map(|(_, entries)| Arc::clone(entries))
            .ok_or(CoreError::NotFound)
    }

    /// Attributes for a readdirplus entry; adds a kernel reference.
    pub fn readdirplus_entry(&self, ino: Ino) -> Option<Attr> {
        let attr = self.tree.write().add_lookup(ino)?;
        Some(self.masked(attr))
    }

    pub fn releasedir(&self, handle: u64) {
        self.dirs.lock().open.remove(&handle);
    }

    pub fn statfs(&self) -> StatFs {
        const BSIZE: u64 = 4096;
        let storage = *self.storage.lock();
        let (max, used) = storage.map_or((0, 0), |s| (s.max, s.used.min(s.max)));
        let files = self.tree.read().len() as u64;
        StatFs {
            blocks: max / BSIZE,
            bfree: (max - used) / BSIZE,
            bavail: (max - used) / BSIZE,
            files,
            ffree: u64::from(u32::MAX),
            bsize: BSIZE as u32,
            namelen: crate::vfs::MAX_NAME_BYTES as u32,
        }
    }

    /// Creates and opens a file (`O_CREAT`). An existing file is opened
    /// instead, unless `exclusive`.
    pub fn create(
        &self,
        parent: Ino,
        name: &[u8],
        mode: u32,
        exclusive: bool,
        open: OpenMode,
    ) -> Result<(Attr, u64)> {
        let name = crate::vfs::validate_name(name)?;
        let existing = self.tree.read().child(parent, name)?;
        if let Some(ino) = existing {
            if exclusive {
                return Err(CoreError::Exists);
            }
            // `O_CREAT`: not an open the bulk-read guard counts (D111).
            let (handle, _) = self.open_handle(ino, open, false)?;
            let attr = self
                .tree
                .write()
                .add_lookup(ino)
                .ok_or(CoreError::NotFound)?;
            return Ok((self.masked(attr), handle));
        }
        self.ensure_entry_writable(parent, name)?;
        self.ensure_key_verified(parent)?;
        let ino = self.allocate_ino()?;
        let ignored = self.is_ignored(parent, name, false);
        // Staging first: a node never exists without its content.
        let staged = self.staging.create(ino, None, None, 0)?;
        let attr = {
            let mut tree = self.tree.write();
            let attr = match tree.create(parent, name, NodeKind::File, mode, now(), ino) {
                Ok(attr) => attr,
                Err(error) => {
                    drop(tree);
                    self.staging.remove(ino);
                    return Err(error);
                }
            };
            let node = tree.get_mut(ino).expect("created");
            node.local_only = ignored;
            node.open_handles = 1;
            node.open_writers = 1;
            attr
        };
        self.staged
            .lock()
            .insert(ino, Arc::new(StagingLock::new(Rank::Staging, staged)));
        self.persist(&[ino, parent]);
        self.journal.touch_content(ino);
        let handle = self.add_handle(ino, true, false);
        Ok((self.masked(attr), handle))
    }

    pub fn mkdir(&self, parent: Ino, name: &[u8], mode: u32) -> Result<Attr> {
        let name = crate::vfs::validate_name(name)?;
        self.ensure_entry_writable(parent, name)?;
        self.ensure_key_verified(parent)?;
        let ino = self.allocate_ino()?;
        let ignored = self.is_ignored(parent, name, true);
        let attr = {
            let mut tree = self.tree.write();
            let attr = tree.create(parent, name, NodeKind::Folder, mode, now(), ino)?;
            tree.get_mut(ino).expect("created").local_only = ignored;
            attr
        };
        self.persist(&[ino, parent]);
        self.journal.touch(ino);
        Ok(self.masked(attr))
    }

    pub fn unlink(&self, parent: Ino, name: &[u8]) -> Result<()> {
        self.remove(parent, name, false)
    }

    pub fn rmdir(&self, parent: Ino, name: &[u8]) -> Result<()> {
        self.remove(parent, name, true)
    }

    pub(super) fn remove(&self, parent: Ino, name: &[u8], dir: bool) -> Result<()> {
        let name = crate::vfs::validate_name(name)?;
        self.ensure_entry_writable(parent, name)?;
        self.ensure_child_verified(parent, name)?;
        let (ino, droppable, relist) = {
            let mut tree = self.tree.write();
            let ino = tree.remove_entry(parent, name, dir, now())?;
            (ino, tree.is_droppable(ino), tree.take_relist())
        };
        self.list_later(relist);
        self.persist(&[ino, parent]);
        self.journal.touch(ino);
        self.pins_changed();
        if droppable {
            self.drop_detached(ino);
        }
        if parent == ROOT_INO && name == crate::pdignore::ROOT_FILE {
            self.refresh_ignore();
        }
        Ok(())
    }

    /// `rename(2)` and `renameat2` with `RENAME_NOREPLACE` (§8.3, §7.6).
    pub fn rename(
        &self,
        parent: Ino,
        name: &[u8],
        new_parent: Ino,
        new_name: &[u8],
        mode: RenameMode,
    ) -> Result<()> {
        let name = crate::vfs::validate_name(name)?;
        let new_name = crate::vfs::validate_name(new_name)?;
        self.ensure_entry_writable(parent, name)?;
        self.ensure_entry_writable(new_parent, new_name)?;
        let target = self.tree.read().child(new_parent, new_name)?;
        if let Some(target) = target {
            self.ensure_writable(target)?;
        }
        // D132: a refused node is not renamed, moved or replaced, and
        // nothing moves into, or gets a new name in, a folder whose key did
        // not verify (the name is encrypted to it).
        self.ensure_child_verified(parent, name)?;
        if let Some(target) = target {
            self.ensure_verified(target)?;
        }
        self.ensure_key_verified(new_parent)?;
        if parent != new_parent {
            // A directory moved elsewhere has its `..` rewritten (POSIX).
            let source = self.tree.read().child(parent, name)?;
            if let Some(source) = source
                && self
                    .tree
                    .read()
                    .get(source)
                    .is_some_and(|node| node.is_dir())
            {
                self.ensure_allowed(source, MAY_WRITE)?;
            }
        }
        let is_dir = {
            let tree = self.tree.read();
            let source = tree.child(parent, name)?;
            source.is_some_and(|ino| tree.get(ino).is_some_and(|node| node.is_dir()))
        };
        let ignored = self.is_ignored(new_parent, new_name, is_dir);
        let (renamed, adopted_rev, relist) = {
            let mut tree = self.tree.write();
            let renamed = tree.rename(parent, name, new_parent, new_name, mode, now())?;
            let adopted_rev = self.adopt_on_replace(&mut tree, &renamed, ignored);
            // With the tree: the pin task must not see the replaced root
            // detached and still pinned, and delete the pin.
            if let Some(replaced) = renamed.replaced {
                self.move_pin(replaced, renamed.ino);
            }
            (renamed, adopted_rev, tree.take_relist())
        };
        self.list_later(relist);
        let mut reclassified = self.reclassify(renamed.ino);
        if let Some(rev) = adopted_rev
            && let Some(staged) = self.staged_entry(renamed.ino)
        {
            staged.lock().set_base_rev(rev)?;
        }
        let mut rows = vec![renamed.ino, parent, new_parent];
        rows.extend(renamed.replaced);
        rows.append(&mut reclassified);
        self.persist(&rows);
        for &ino in &rows {
            self.journal.touch(ino);
        }
        self.pins_changed();
        if let Some(replaced) = renamed.replaced {
            let droppable = self.tree.read().is_droppable(replaced);
            if droppable {
                self.drop_detached(replaced);
            }
        }
        let rules = crate::pdignore::ROOT_FILE;
        if (parent == ROOT_INO && name == rules) || (new_parent == ROOT_INO && new_name == rules) {
            self.refresh_ignore();
        }
        Ok(())
    }

    /// Atomic save (§7.6): a file that was never uploaded, renamed over a
    /// remote file, takes over that file's remote node, so its content
    /// becomes a new revision of it. Returns the revision the upload must
    /// build on.
    pub(super) fn adopt_on_replace(
        &self,
        tree: &mut Tree,
        renamed: &crate::vfs::Renamed,
        source_ignored: bool,
    ) -> Option<Option<RevisionId>> {
        let replaced = renamed.replaced?;
        let source = tree.get(renamed.ino)?;
        let target = tree.get(replaced)?;
        let adopt = source.kind == NodeKind::File
            && target.kind == NodeKind::File
            && source.remote.is_none()
            && source.content == Content::Staged
            && target.remote.is_some()
            && !source_ignored;
        if !adopt {
            return None;
        }
        let remote = target.remote.clone();
        let rev = remote.as_ref().and_then(|r| r.rev.clone());
        tree.set_remote(replaced, None);
        if let Some(target) = tree.get_mut(replaced) {
            target.deleted = false;
        }
        tree.set_remote(renamed.ino, remote);
        if let Some(source) = tree.get_mut(renamed.ino) {
            // Its own mode and mtime replace the old file's.
            source.gens.meta += 1;
        }
        Some(rev)
    }

    /// `setattr` (§8.5): mode, size, mtime and atime; ownership is checked
    /// by the caller.
    pub fn setattr(&self, ino: Ino, change: &SetAttr) -> Result<Attr> {
        let kind = self.tree.read().get(ino).ok_or(CoreError::NotFound)?.kind;
        if change != &SetAttr::default() {
            self.ensure_writable(ino)?;
        }
        if change.mode.is_some() || change.size.is_some() || change.mtime.is_some() {
            // D132: changing it uploads a revision or its attributes.
            self.ensure_verified(ino)?;
        }
        if change.size.is_some() && !self.writable_handle(ino, change.handle) {
            self.ensure_allowed(ino, MAY_WRITE)?;
        }
        if let Some(size) = change.size {
            if kind == NodeKind::Folder {
                return Err(CoreError::IsADirectory);
            }
            self.truncate(ino, size)?;
        }
        let attr = {
            let mut tree = self.tree.write();
            let node = tree.get_mut(ino).ok_or(CoreError::NotFound)?;
            let mut meta_changed = false;
            if let Some(mode) = change.mode {
                meta_changed |= node.mode != mode & 0o7777;
                node.mode = mode & 0o7777;
            }
            if let Some(mtime) = change.mtime {
                meta_changed |= node.mtime != mtime;
                node.mtime = mtime;
            }
            if let Some(atime) = change.atime {
                node.atime = Some(atime);
            }
            if meta_changed && node.kind == NodeKind::File {
                node.gens.meta += 1;
            }
            node.ctime = now();
            node.attr()
        };
        self.persist(&[ino]);
        if kind == NodeKind::File {
            self.journal.touch_meta(ino);
        }
        Ok(self.masked(attr))
    }
}
