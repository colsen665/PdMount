//! Staging files: the local working copy of changed files (DESIGN.md §7.1,
//! §7.2).
//!
//! Each staged inode has `staging/<ino>`, a sparse file whose length is the
//! file's size, and `staging/<ino>.state`, a small sidecar with the base
//! revision and which 4 MiB blocks of it are materialized. Ranges of the
//! base that are not materialized are read from the base revision through
//! the block cache. A write that only partly covers such a block needs the
//! block materialized first; one that covers it completely just marks it.
//!
//! A file with neither base nor base revision (a new file) has no sidecar
//! (D067): a data file without one never had a base. A file that has one
//! gets its sidecar before its data file, and keeps it from the first save
//! on. Startup lists the data files; a sidecar without one is removed.
//!
//! The sidecar is replaced before a write that depends on it is
//! acknowledged, so a killed daemon finds staging data and bookkeeping in
//! agreement. Both live in the page cache until `sync`, which flushes the
//! sidecar before the data.
//!
//! Staging files are opened on demand: an [`FdCache`] keeps the recently
//! used descriptors, so the number of staged files is not bounded by the
//! open-file limit (D063).

use crate::cache::BLOCK_SIZE;
use crate::vfs::Ino;
use pdmount_backend::{LinkId, RevisionId};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};
use std::fs::File;
use std::io::{self, Write};
use std::num::NonZeroUsize;
use std::os::unix::fs::{DirBuilderExt, FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Materialization granularity: Proton's block size.
pub const GRID: u64 = BLOCK_SIZE;
const SIDECAR_VERSION: u32 = 1;
/// Staging descriptors kept open at most (D063).
pub const OPEN_FILES_MAX: usize = 1024;
/// Fewer are never kept, whatever the open-file limit.
const OPEN_FILES_MIN: usize = 16;

/// Descriptors the staging may keep open: [`OPEN_FILES_MAX`], or a quarter
/// of the soft open-file limit when that is lower, which leaves the rest to
/// the store, the block cache, FUSE and the network.
pub fn open_files_capacity() -> usize {
    use nix::sys::resource::{Resource, getrlimit};
    let soft = getrlimit(Resource::RLIMIT_NOFILE).map_or(OPEN_FILES_MAX as u64, |(soft, _)| soft);
    usize::try_from(soft / 4)
        .unwrap_or(OPEN_FILES_MAX)
        .clamp(OPEN_FILES_MIN, OPEN_FILES_MAX)
}

/// Open staging files by inode, least recently used closed first (D063).
///
/// A descriptor is shared while an operation uses it: an evicted file stays
/// open until that operation drops its `Arc`. Descriptors are opened and
/// closed outside the lock, since a close can make btrfs flush the file.
pub struct FdCache {
    files: parking_lot::Mutex<lru::LruCache<Ino, Arc<File>>>,
}

impl FdCache {
    pub fn new(capacity: usize) -> Self {
        let capacity = NonZeroUsize::new(capacity).unwrap_or(NonZeroUsize::MIN);
        Self {
            files: parking_lot::Mutex::new(lru::LruCache::new(capacity)),
        }
    }

    /// The descriptor of `ino`, opened read-write from `path` on a miss
    /// (std opens with `O_CLOEXEC`).
    pub fn get(&self, ino: Ino, path: &Path) -> io::Result<Arc<File>> {
        // Opened under the lock: a descriptor opened before a `remove` and
        // stored after a new `insert` would hide the new file, and writes
        // through it would go to an unlinked one.
        let mut files = self.files.lock();
        if let Some(file) = files.get(&ino) {
            return Ok(Arc::clone(file));
        }
        let file = Arc::new(
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)?,
        );
        let evicted = files.push(ino, Arc::clone(&file));
        drop(files);
        // Closed after the lock, as in `insert`.
        drop(evicted);
        Ok(file)
    }

    /// Makes `file` the descriptor of `ino`, replacing any other.
    fn insert(&self, ino: Ino, file: File) -> Arc<File> {
        let file = Arc::new(file);
        // The replaced or evicted descriptor is closed after the lock.
        let evicted = self.files.lock().push(ino, Arc::clone(&file));
        drop(evicted);
        file
    }

    /// Closes the descriptor of `ino`, if open.
    pub fn remove(&self, ino: Ino) {
        let file = self.files.lock().pop(&ino);
        drop(file);
    }

    /// Descriptors open now.
    pub fn len(&self) -> usize {
        self.files.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn capacity(&self) -> usize {
        self.files.lock().cap().get()
    }
}

impl std::fmt::Debug for FdCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let files = self.files.lock();
        f.debug_struct("FdCache")
            .field("open", &files.len())
            .field("capacity", &files.cap())
            .finish()
    }
}

/// The remote revision whose content a staged file still partly uses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Base {
    pub link: LinkId,
    pub rev: RevisionId,
    /// Size of that revision.
    pub rev_size: u64,
    /// Bytes `[0, size)` of the file may still come from the base; the file
    /// was truncated below the rest.
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Sidecar {
    version: u32,
    /// Revision the local edits derive from: the precondition of the next
    /// upload (§7.4). `None` for files not uploaded yet.
    base_rev: Option<RevisionId>,
    base: Option<Base>,
    /// Grid blocks of the base already copied into the staging file.
    materialized: BTreeSet<u64>,
}

impl Default for Sidecar {
    /// What a missing sidecar stands for: no base, no base revision.
    fn default() -> Self {
        Self {
            version: SIDECAR_VERSION,
            base_rev: None,
            base: None,
            materialized: BTreeSet::new(),
        }
    }
}

impl Sidecar {
    fn is_default(&self) -> bool {
        self.base_rev.is_none() && self.base.is_none() && self.materialized.is_empty()
    }
}

/// Where the bytes of a range come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Piece {
    Staging { offset: u64, len: u64 },
    Base { offset: u64, len: u64 },
}

/// The revisions staged files still read from the block cache, which the
/// cache must not evict (§6.2). A [`Staged`] holds its entry while it has a
/// base, however it ends: every way out of the staged map drops the entry.
#[derive(Debug, Clone, Default)]
pub struct Protections(Arc<parking_lot::Mutex<HashMap<(LinkId, RevisionId), u32>>>);

impl Protections {
    fn hold(&self, base: &Base) -> Protection {
        let key = (base.link.clone(), base.rev.clone());
        *self.0.lock().entry(key.clone()).or_default() += 1;
        Protection {
            protections: self.clone(),
            key,
        }
    }

    /// The protected revisions, for a pass over the cache's entries. Takes
    /// nothing while held.
    pub fn lock(&self) -> parking_lot::MutexGuard<'_, HashMap<(LinkId, RevisionId), u32>> {
        self.0.lock()
    }

    /// Revisions protected now.
    pub fn len(&self) -> usize {
        self.0.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// One staged file's hold on its base revision.
#[derive(Debug)]
struct Protection {
    protections: Protections,
    key: (LinkId, RevisionId),
}

impl Drop for Protection {
    fn drop(&mut self) {
        let mut held = self.protections.0.lock();
        if let Some(count) = held.get_mut(&self.key) {
            *count -= 1;
            if *count == 0 {
                held.remove(&self.key);
            }
        }
    }
}

/// Staging directory of a profile. Clones share the open descriptors.
#[derive(Debug, Clone)]
pub struct StagingDir {
    dir: PathBuf,
    files: Arc<FdCache>,
    protections: Protections,
}

impl StagingDir {
    /// Opens with [`open_files_capacity`] descriptors at most.
    pub fn open(dir: &Path) -> io::Result<Self> {
        Self::with_capacity(dir, open_files_capacity())
    }

    pub fn with_capacity(dir: &Path, open_files: usize) -> io::Result<Self> {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        Ok(Self {
            dir: dir.to_owned(),
            files: Arc::new(FdCache::new(open_files)),
            protections: Protections::default(),
        })
    }

    /// The base revisions of this directory's staged files.
    pub fn protections(&self) -> &Protections {
        &self.protections
    }

    /// Staging descriptors open now (the `staged_open_files` gauge).
    pub fn open_files(&self) -> usize {
        self.files.len()
    }

    pub fn data_path(&self, ino: Ino) -> PathBuf {
        self.dir.join(ino.to_string())
    }

    fn sidecar_path(&self, ino: Ino) -> PathBuf {
        self.dir.join(format!("{ino}.state"))
    }

    pub fn snapshot_path(&self, ino: Ino) -> PathBuf {
        self.dir.join(format!("{ino}.snap"))
    }

    /// Inodes with staging files, for recovery: those with a data file (a
    /// new file has no sidecar). Removes sidecars left without a data file
    /// (a crash between the two in `create` or `remove`).
    pub fn staged_inos(&self) -> io::Result<Vec<Ino>> {
        let mut inos = Vec::new();
        let mut sidecars = Vec::new();
        for entry in std::fs::read_dir(&self.dir)? {
            let name = entry?.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if let Ok(ino) = name.parse::<Ino>() {
                inos.push(ino);
            } else if let Some(ino) = name.strip_suffix(".state").and_then(|n| n.parse().ok()) {
                sidecars.push(ino);
            }
        }
        inos.sort_unstable();
        for ino in sidecars {
            if inos.binary_search(&ino).is_err() {
                let _ = std::fs::remove_file(self.sidecar_path(ino));
            }
        }
        Ok(inos)
    }

    /// Starts staging an inode: `size` bytes, the first `base.size` of them
    /// from `base`. The sidecar, if the file needs one, comes first.
    pub fn create(
        &self,
        ino: Ino,
        base_rev: Option<RevisionId>,
        base: Option<Base>,
        size: u64,
    ) -> io::Result<Staged> {
        let base = base.filter(|b| b.size > 0);
        let mut staged = Staged {
            ino,
            dir: self.clone(),
            path: self.data_path(ino),
            protection: base.as_ref().map(|base| self.protections.hold(base)),
            sidecar: Sidecar {
                base_rev,
                base,
                ..Sidecar::default()
            },
            size,
            sidecar_on_disk: false,
            sidecar_synced: false,
        };
        if staged.sidecar.is_default() {
            // One left from earlier staging whose removal is still queued
            // would give the new data its base.
            match std::fs::remove_file(self.sidecar_path(ino)) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
                _ => {}
            }
        } else {
            staged.save()?;
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(self.data_path(ino))?;
        // Not for an empty file: truncating to 0 makes btrfs flush the file
        // when it is closed, which waits for a commit in progress.
        if size > 0 {
            file.set_len(size)?;
        }
        self.files.insert(ino, file);
        Ok(staged)
    }

    /// Reopens a staged inode after a restart. A missing sidecar means no
    /// base (a new file).
    pub fn reopen(&self, ino: Ino) -> io::Result<Staged> {
        let (sidecar, sidecar_on_disk) = match std::fs::read(self.sidecar_path(ino)) {
            Ok(json) => (
                serde_json::from_slice(&json)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?,
                true,
            ),
            Err(e) if e.kind() == io::ErrorKind::NotFound => (Sidecar::default(), false),
            Err(e) => return Err(e),
        };
        // No descriptor yet: the first I/O opens one.
        let path = self.data_path(ino);
        let size = std::fs::metadata(&path)?.len();
        let mut staged = Staged {
            ino,
            dir: self.clone(),
            path,
            protection: sidecar
                .base
                .as_ref()
                .map(|base| self.protections.hold(base)),
            sidecar,
            size,
            sidecar_on_disk,
            // Not known: the next `sync` flushes it.
            sidecar_synced: false,
        };
        // A truncate that cut the file but crashed before its sidecar save
        // ([`Staged::truncate`]): the file is the truth, finish the cut.
        if let Some(base) = &mut staged.sidecar.base
            && base.size > size
        {
            base.size = size;
            staged.drop_base_if_complete();
            staged.save()?;
        }
        Ok(staged)
    }

    /// Deletes the staging files of an inode, closing its descriptor first.
    /// The data file goes first, so a crash leaves at most a sidecar, which
    /// startup removes. Missing files (no sidecar, no snapshot) are fine.
    pub fn remove(&self, ino: Ino) {
        self.files.remove(ino);
        for path in [
            self.data_path(ino),
            self.sidecar_path(ino),
            self.snapshot_path(ino),
        ] {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// One staged inode. Callers serialize access (one lock per inode).
#[derive(Debug)]
pub struct Staged {
    ino: Ino,
    dir: StagingDir,
    /// The staging file, opened through the directory's [`FdCache`].
    path: PathBuf,
    /// Held while the sidecar has a base.
    protection: Option<Protection>,
    sidecar: Sidecar,
    /// Always equal to the staging file's length.
    size: u64,
    /// The sidecar file exists (D067: not for a new file until it gets a
    /// base revision).
    sidecar_on_disk: bool,
    /// The sidecar on disk was flushed since its last save.
    sidecar_synced: bool,
}

impl Staged {
    pub fn ino(&self) -> Ino {
        self.ino
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    /// The staging file's descriptor, for one operation.
    fn file(&self) -> io::Result<Arc<File>> {
        self.dir.files.get(self.ino, &self.path)
    }

    pub fn base_rev(&self) -> Option<&RevisionId> {
        self.sidecar.base_rev.as_ref()
    }

    pub fn base(&self) -> Option<&Base> {
        self.sidecar.base.as_ref()
    }

    /// Changes the upload precondition (after an upload, or when a new file
    /// takes over an existing node, §7.6).
    pub fn set_base_rev(&mut self, rev: Option<RevisionId>) -> io::Result<()> {
        if self.sidecar.base_rev != rev {
            self.sidecar.base_rev = rev;
            self.save()?;
        }
        Ok(())
    }

    /// Bytes of the base that a grid block holds.
    pub fn base_range(&self, index: u64) -> Option<(u64, u64)> {
        let base = self.sidecar.base.as_ref()?;
        let start = index * GRID;
        (start < base.size && !self.sidecar.materialized.contains(&index))
            .then(|| (start, (start + GRID).min(base.size)))
    }

    /// Grid blocks of `[offset, offset + len)` still served by the base.
    fn blocks(offset: u64, len: u64) -> std::ops::Range<u64> {
        if len == 0 {
            return 0..0;
        }
        offset / GRID..(offset + len).div_ceil(GRID)
    }

    /// Where `[offset, offset + len)` (clamped to the size) comes from.
    pub fn plan_read(&self, offset: u64, len: u64) -> Vec<Piece> {
        let end = (offset + len).min(self.size);
        if offset >= end {
            return Vec::new();
        }
        let mut pieces: Vec<Piece> = Vec::new();
        let mut push = |piece: Piece| match (pieces.last_mut(), piece) {
            (
                Some(Piece::Staging { offset, len }),
                Piece::Staging {
                    offset: next,
                    len: more,
                },
            ) if *offset + *len == next => *len += more,
            (
                Some(Piece::Base { offset, len }),
                Piece::Base {
                    offset: next,
                    len: more,
                },
            ) if *offset + *len == next => *len += more,
            _ => pieces.push(piece),
        };
        for index in Self::blocks(offset, end - offset) {
            let start = offset.max(index * GRID);
            let stop = end.min((index + 1) * GRID);
            match self.base_range(index) {
                Some((_, base_end)) if start < base_end => {
                    let mid = stop.min(base_end);
                    push(Piece::Base {
                        offset: start,
                        len: mid - start,
                    });
                    if mid < stop {
                        push(Piece::Staging {
                            offset: mid,
                            len: stop - mid,
                        });
                    }
                }
                _ => push(Piece::Staging {
                    offset: start,
                    len: stop - start,
                }),
            }
        }
        pieces
    }

    /// Base blocks a write of `[offset, offset + len)` covers only partly;
    /// they must be materialized before the write.
    pub fn blocks_to_materialize_for_write(&self, offset: u64, len: u64) -> Vec<u64> {
        let end = offset + len;
        Self::blocks(offset, len)
            .filter(|&index| {
                self.base_range(index).is_some_and(|(start, stop)| {
                    let overlaps = offset < stop && end > start;
                    overlaps && (offset > start || end < stop)
                })
            })
            .collect()
    }

    /// Base blocks a truncation to `size` must materialize first: the one
    /// cut in the middle.
    pub fn blocks_to_materialize_for_truncate(&self, size: u64) -> Vec<u64> {
        let index = size / GRID;
        match self.base_range(index) {
            Some((start, end)) if size > start && size < end => vec![index],
            _ => Vec::new(),
        }
    }

    /// The first `limit` base blocks not materialized yet (before a
    /// snapshot). Callers work in batches, so a large base never becomes one
    /// list.
    pub fn missing_blocks(&self, limit: usize) -> Vec<u64> {
        let Some(base) = &self.sidecar.base else {
            return Vec::new();
        };
        (0..base.size.div_ceil(GRID))
            .filter(|index| !self.sidecar.materialized.contains(index))
            .take(limit)
            .collect()
    }

    /// Copies base content into a missing block. `data` is the base's bytes
    /// for [`Staged::base_range`] at the time it was read; a block that was
    /// materialized meanwhile, or a base that changed, is left alone.
    pub fn materialize(&mut self, index: u64, base: &Base, data: &[u8]) -> io::Result<()> {
        if self.sidecar.base.as_ref().map(|b| (&b.link, &b.rev)) != Some((&base.link, &base.rev)) {
            return Ok(());
        }
        let Some((start, end)) = self.base_range(index) else {
            return Ok(());
        };
        let len = (end - start) as usize;
        if data.len() < len {
            // A short read would become zeros in the upload.
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("base block {index} has {} of {len} bytes", data.len()),
            ));
        }
        self.file()?.write_all_at(&data[..len], start)?;
        self.sidecar.materialized.insert(index);
        self.drop_base_if_complete();
        self.save()
    }

    /// Writes into staging. Partly covered base blocks must have been
    /// materialized ([`Staged::blocks_to_materialize_for_write`]).
    pub fn write(&mut self, offset: u64, data: &[u8]) -> io::Result<()> {
        let len = data.len() as u64;
        debug_assert!(self.blocks_to_materialize_for_write(offset, len).is_empty());
        let end = offset + len;
        let covered: Vec<u64> = Self::blocks(offset, len)
            .filter(|&index| {
                self.base_range(index)
                    .is_some_and(|(start, stop)| offset <= start && end >= stop)
            })
            .collect();
        // Data first: a crash before the sidecar save leaves the block on its
        // base (the write was never acknowledged), never a hole of zeros.
        self.file()?.write_all_at(data, offset)?;
        self.size = self.size.max(offset + len);
        if !covered.is_empty() {
            self.sidecar.materialized.extend(covered);
            self.drop_base_if_complete();
            self.save()?;
        }
        Ok(())
    }

    /// Reads staging bytes; the caller merges base pieces.
    pub fn read_at(&self, offset: u64, buffer: &mut [u8]) -> io::Result<()> {
        self.file()?.read_exact_at(buffer, offset)
    }

    /// Changes the size. A cut inside a missing base block must have been
    /// materialized ([`Staged::blocks_to_materialize_for_truncate`]).
    pub fn truncate(&mut self, size: u64) -> io::Result<()> {
        // The file first: reopening takes the size from it, so a crash before
        // the sidecar save never exposes the base tail as zeros.
        self.file()?.set_len(size)?;
        self.size = size;
        if let Some(base) = &mut self.sidecar.base
            && size < base.size
        {
            base.size = size;
            self.drop_base_if_complete();
            self.save()?;
        }
        Ok(())
    }

    fn drop_base_if_complete(&mut self) {
        if let Some(base) = &self.sidecar.base
            && (0..base.size.div_ceil(GRID)).all(|i| self.sidecar.materialized.contains(&i))
        {
            self.sidecar.base = None;
            self.sidecar.materialized.clear();
            self.protection = None;
        }
    }

    pub fn fully_materialized(&self) -> bool {
        self.sidecar.base.is_none()
    }

    /// Copies the staging file to its snapshot path (a reflink on btrfs and
    /// XFS, through `copy_file_range`). Only valid when fully materialized.
    pub fn snapshot(&self) -> io::Result<PathBuf> {
        debug_assert!(self.fully_materialized());
        let path = self.dir.snapshot_path(self.ino);
        let _ = std::fs::remove_file(&path);
        std::fs::copy(self.dir.data_path(self.ino), &path)?;
        Ok(path)
    }

    /// `fdatasync` of sidecar and data (fsync, §5.6): the sidecar first, if
    /// it changed since its last flush, so durable data never comes with a
    /// lost sidecar that held its base. The descriptor may have been
    /// reopened since the writes: fsync flushes the inode, through
    /// whichever descriptor.
    pub fn sync(&mut self) -> io::Result<()> {
        if self.sidecar_on_disk && !self.sidecar_synced {
            File::open(self.dir.sidecar_path(self.ino))?.sync_data()?;
            self.sidecar_synced = true;
        }
        self.file()?.sync_data()
    }

    /// Replaces the sidecar. A file that never had one gets none while it
    /// is the default; once written it is kept, default or not.
    fn save(&mut self) -> io::Result<()> {
        if !self.sidecar_on_disk && self.sidecar.is_default() {
            return Ok(());
        }
        let path = self.dir.sidecar_path(self.ino);
        let tmp = path.with_extension("state.tmp");
        let json = serde_json::to_vec(&self.sidecar).map_err(io::Error::other)?;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(&json)?;
        drop(file);
        std::fs::rename(&tmp, &path)?;
        (self.sidecar_on_disk, self.sidecar_synced) = (true, false);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base(size: u64) -> Base {
        Base {
            link: LinkId::new("l"),
            rev: RevisionId::new("r"),
            rev_size: size,
            size,
        }
    }

    fn dir() -> (tempfile::TempDir, StagingDir) {
        let temp = tempfile::tempdir().expect("tempdir");
        let staging = StagingDir::open(&temp.path().join("staging")).expect("open");
        (temp, staging)
    }

    #[test]
    fn reads_merge_base_and_staging() {
        let (_temp, staging) = dir();
        let mut staged = staging
            .create(
                2,
                Some(RevisionId::new("r")),
                Some(base(3 * GRID)),
                3 * GRID,
            )
            .expect("create");
        assert_eq!(
            staged.plan_read(GRID - 1, 2),
            vec![Piece::Base {
                offset: GRID - 1,
                len: 2
            }]
        );
        // A whole-block write needs no materialization.
        assert!(
            staged
                .blocks_to_materialize_for_write(GRID, GRID)
                .is_empty()
        );
        staged.write(GRID, &vec![7; GRID as usize]).expect("write");
        assert_eq!(
            staged.plan_read(GRID - 1, GRID + 2),
            vec![
                Piece::Base {
                    offset: GRID - 1,
                    len: 1
                },
                Piece::Staging {
                    offset: GRID,
                    len: GRID
                },
                Piece::Base {
                    offset: 2 * GRID,
                    len: 1
                },
            ]
        );
        // A partial write needs its block first.
        assert_eq!(staged.blocks_to_materialize_for_write(10, 5), vec![0]);
        assert_eq!(staged.missing_blocks(16), vec![0, 2]);
        assert_eq!(staged.missing_blocks(1), vec![0], "one batch at a time");
    }

    #[test]
    fn appends_past_the_base_go_to_staging() {
        let (_temp, staging) = dir();
        let mut staged = staging.create(2, None, Some(base(10)), 10).expect("create");
        assert!(staged.blocks_to_materialize_for_write(10, 5).is_empty());
        staged.write(10, b"hello").expect("append");
        assert_eq!(staged.size(), 15);
        assert_eq!(
            staged.plan_read(0, 100),
            vec![
                Piece::Base { offset: 0, len: 10 },
                Piece::Staging { offset: 10, len: 5 }
            ]
        );
    }

    #[test]
    fn truncation_cuts_the_base_and_materializes_the_cut_block() {
        let (_temp, staging) = dir();
        let mut staged = staging
            .create(2, None, Some(base(2 * GRID)), 2 * GRID)
            .expect("create");
        assert_eq!(
            staged.blocks_to_materialize_for_truncate(GRID),
            Vec::<u64>::new()
        );
        assert_eq!(staged.blocks_to_materialize_for_truncate(GRID + 1), vec![1]);
        staged.truncate(GRID).expect("truncate at a boundary");
        assert_eq!(staged.missing_blocks(16), vec![0]);
        staged.truncate(GRID + 100).expect("extend");
        assert_eq!(
            staged.plan_read(GRID, 100),
            vec![Piece::Staging {
                offset: GRID,
                len: 100
            }]
        );
        staged.truncate(0).expect("truncate to zero");
        assert!(staged.fully_materialized());
    }

    #[test]
    fn materialized_blocks_and_base_survive_reopening() {
        let (_temp, staging) = dir();
        {
            let mut staged = staging
                .create(
                    5,
                    Some(RevisionId::new("r")),
                    Some(base(GRID + 10)),
                    GRID + 10,
                )
                .expect("create");
            staged
                .materialize(1, &base(GRID + 10), &[1; 10])
                .expect("materialize");
            staged.write(GRID + 2, b"xy").expect("write");
        }
        assert_eq!(staging.staged_inos().expect("list"), vec![5]);
        let staged = staging.reopen(5).expect("reopen");
        assert_eq!(staged.size(), GRID + 10);
        assert_eq!(staged.missing_blocks(16), vec![0]);
        assert_eq!(staged.base_rev(), Some(&RevisionId::new("r")));
        let mut buffer = [0u8; 4];
        staged.read_at(GRID, &mut buffer).expect("read");
        assert_eq!(buffer, [1, 1, b'x', b'y']);
    }

    #[test]
    fn a_complete_materialization_drops_the_base_and_allows_snapshots() {
        let (_temp, staging) = dir();
        let mut staged = staging.create(3, None, Some(base(4)), 4).expect("create");
        assert!(!staged.fully_materialized());
        // A stale read of another base is ignored.
        let mut other = base(4);
        other.rev = RevisionId::new("other");
        staged.materialize(0, &other, b"zzzz").expect("ignored");
        assert!(!staged.fully_materialized());
        staged
            .materialize(0, &base(4), b"abcd")
            .expect("materialize");
        assert!(staged.fully_materialized());
        let snapshot = staged.snapshot().expect("snapshot");
        assert_eq!(std::fs::read(snapshot).expect("read"), b"abcd");
        staging.remove(3);
        assert!(staging.staged_inos().expect("list").is_empty());
    }

    #[test]
    fn a_short_base_read_is_not_materialized() {
        let (_temp, staging) = dir();
        let mut staged = staging.create(4, None, Some(base(4)), 4).expect("create");
        assert!(staged.materialize(0, &base(4), b"ab").is_err());
        assert_eq!(staged.missing_blocks(16), vec![0]);
    }

    #[test]
    fn many_staged_files_share_a_few_descriptors() {
        let temp = tempfile::tempdir().expect("tempdir");
        let staging = StagingDir::with_capacity(&temp.path().join("staging"), 4).expect("open");
        let content = |ino: Ino, round: u64| format!("file {ino} round {round};").into_bytes();
        let mut files: Vec<Staged> = (1..=50)
            .map(|ino| staging.create(ino, None, None, 0).expect("create"))
            .collect();
        assert!(staging.open_files() <= 4);
        for round in 0..3 {
            for staged in &mut files {
                let data = content(staged.ino(), round);
                let offset = round * 64;
                staged.write(offset, &data).expect("write");
                assert!(staging.open_files() <= 4);
            }
        }
        // A truncate and a sync go through reopened descriptors too.
        files[0].truncate(10).expect("truncate");
        files[1].sync().expect("sync");
        for staged in files.iter().skip(1).rev() {
            for round in 0..3 {
                let expected = content(staged.ino(), round);
                let mut buffer = vec![0; expected.len()];
                staged.read_at(round * 64, &mut buffer).expect("read");
                assert_eq!(buffer, expected);
            }
            assert!(staging.open_files() <= 4);
        }
        assert_eq!(files[0].size(), 10);
        assert_eq!(
            std::fs::read(staging.data_path(1)).expect("read"),
            &content(1, 0)[..10]
        );
        // Removal closes the descriptor (2 was read last).
        let open = staging.open_files();
        staging.remove(2);
        assert_eq!(staging.open_files(), open - 1);
    }

    #[test]
    fn open_file_capacity_follows_the_limit() {
        let capacity = open_files_capacity();
        assert!((OPEN_FILES_MIN..=OPEN_FILES_MAX).contains(&capacity));
    }

    #[test]
    fn a_new_file_has_no_sidecar_until_it_gets_a_base_revision() {
        let (_temp, staging) = dir();
        let mut staged = staging.create(7, None, None, 0).expect("create");
        staged.write(0, b"new").expect("write");
        assert!(!staging.sidecar_path(7).exists());
        // `sync` without a sidecar flushes the data only.
        staged.sync().expect("sync");
        assert_eq!(staging.staged_inos().expect("list"), vec![7]);
        let reopened = staging.reopen(7).expect("reopen");
        assert_eq!(reopened.size(), 3);
        assert_eq!(reopened.base(), None);
        assert_eq!(reopened.base_rev(), None);
        assert_eq!(
            reopened.plan_read(0, 3),
            vec![Piece::Staging { offset: 0, len: 3 }]
        );
        // After its upload it has a base revision, kept on disk.
        staged
            .set_base_rev(Some(RevisionId::new("r")))
            .expect("base rev");
        assert!(staging.sidecar_path(7).exists());
        staged.sync().expect("sync");
        assert_eq!(
            staging.reopen(7).expect("reopen").base_rev(),
            Some(&RevisionId::new("r"))
        );
        staging.remove(7);
        assert!(staging.staged_inos().expect("list").is_empty());
    }

    #[test]
    fn a_file_with_a_base_has_its_sidecar_before_its_data() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("staging");
        let staging = StagingDir::open(&path).expect("open");
        // The data file cannot be created: the sidecar must exist already.
        std::fs::create_dir(staging.data_path(8)).expect("block the data path");
        assert!(
            staging
                .create(8, Some(RevisionId::new("r")), Some(base(10)), 10)
                .is_err()
        );
        assert!(staging.sidecar_path(8).exists());
        std::fs::remove_dir(staging.data_path(8)).expect("unblock");
        // Without a data file, startup removes it.
        assert!(staging.staged_inos().expect("list").is_empty());
        assert!(!staging.sidecar_path(8).exists());
        let staged = staging
            .create(8, Some(RevisionId::new("r")), Some(base(10)), 10)
            .expect("create");
        assert!(staging.sidecar_path(8).exists() && staging.data_path(8).exists());
        drop(staged);
        assert_eq!(staging.reopen(8).expect("reopen").base(), Some(&base(10)));
    }

    #[test]
    fn restaging_without_a_base_drops_an_old_sidecar() {
        let (_temp, staging) = dir();
        drop(
            staging
                .create(9, Some(RevisionId::new("r")), Some(base(10)), 10)
                .expect("create"),
        );
        // Created again before the old staging was removed.
        let mut staged = staging.create(9, None, None, 0).expect("create again");
        staged.write(0, b"ab").expect("write");
        let reopened = staging.reopen(9).expect("reopen");
        assert_eq!(reopened.base(), None);
        assert_eq!(
            reopened.plan_read(0, 2),
            vec![Piece::Staging { offset: 0, len: 2 }]
        );
    }

    #[test]
    fn reopening_finishes_a_truncate_cut_short_by_a_crash() {
        let (_temp, staging) = dir();
        drop(
            staging
                .create(6, None, Some(base(2 * GRID)), 2 * GRID)
                .expect("create"),
        );
        // The file was cut, the sidecar save never happened.
        std::fs::OpenOptions::new()
            .write(true)
            .open(staging.data_path(6))
            .and_then(|file| file.set_len(GRID))
            .expect("cut");
        let mut staged = staging.reopen(6).expect("reopen");
        assert_eq!(staged.size(), GRID);
        assert_eq!(staged.missing_blocks(16), vec![0]);
        // Growing again reads zeros, not the old base tail.
        staged.truncate(2 * GRID).expect("extend");
        assert_eq!(
            staged.plan_read(GRID, 10),
            vec![Piece::Staging {
                offset: GRID,
                len: 10
            }]
        );
    }
}
