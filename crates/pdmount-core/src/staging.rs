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
//! The sidecar is replaced before a write that depends on it is
//! acknowledged, so a killed daemon finds staging data and bookkeeping in
//! agreement. Both live in the page cache until `sync`, like the data.

use crate::cache::BLOCK_SIZE;
use crate::vfs::Ino;
use pdmount_backend::{LinkId, RevisionId};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs::File;
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

/// Materialization granularity: Proton's block size.
pub const GRID: u64 = BLOCK_SIZE;
const SIDECAR_VERSION: u32 = 1;

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

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Sidecar {
    version: u32,
    /// Revision the local edits derive from: the precondition of the next
    /// upload (§7.4). `None` for files not uploaded yet.
    base_rev: Option<RevisionId>,
    base: Option<Base>,
    /// Grid blocks of the base already copied into the staging file.
    materialized: BTreeSet<u64>,
}

/// Where the bytes of a range come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Piece {
    Staging { offset: u64, len: u64 },
    Base { offset: u64, len: u64 },
}

/// Staging directory of a profile.
#[derive(Debug, Clone)]
pub struct StagingDir {
    dir: PathBuf,
}

impl StagingDir {
    pub fn open(dir: &Path) -> io::Result<Self> {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        Ok(Self {
            dir: dir.to_owned(),
        })
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

    /// Inodes with staging files, for recovery.
    pub fn staged_inos(&self) -> io::Result<Vec<Ino>> {
        let mut inos = Vec::new();
        for entry in std::fs::read_dir(&self.dir)? {
            let name = entry?.file_name();
            if let Some(ino) = name
                .to_str()
                .and_then(|n| n.strip_suffix(".state"))
                .and_then(|n| n.parse().ok())
            {
                inos.push(ino);
            }
        }
        inos.sort_unstable();
        Ok(inos)
    }

    /// Starts staging an inode: `size` bytes, the first `base.size` of them
    /// from `base`.
    pub fn create(
        &self,
        ino: Ino,
        base_rev: Option<RevisionId>,
        base: Option<Base>,
        size: u64,
    ) -> io::Result<Staged> {
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
        let mut staged = Staged {
            ino,
            dir: self.clone(),
            file,
            sidecar: Sidecar {
                version: SIDECAR_VERSION,
                base_rev,
                base: base.filter(|b| b.size > 0),
                materialized: BTreeSet::new(),
            },
            size,
        };
        staged.save()?;
        Ok(staged)
    }

    /// Reopens a staged inode after a restart.
    pub fn reopen(&self, ino: Ino) -> io::Result<Staged> {
        let json = std::fs::read(self.sidecar_path(ino))?;
        let sidecar: Sidecar = serde_json::from_slice(&json)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(self.data_path(ino))?;
        let size = file.metadata()?.len();
        let mut staged = Staged {
            ino,
            dir: self.clone(),
            file,
            sidecar,
            size,
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

    /// Deletes the staging files of an inode.
    pub fn remove(&self, ino: Ino) {
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
    file: File,
    sidecar: Sidecar,
    /// Always equal to the staging file's length.
    size: u64,
}

impl Staged {
    pub fn ino(&self) -> Ino {
        self.ino
    }

    pub fn size(&self) -> u64 {
        self.size
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

    /// Every base block not materialized yet (before a snapshot).
    pub fn missing_blocks(&self) -> Vec<u64> {
        let Some(base) = &self.sidecar.base else {
            return Vec::new();
        };
        (0..base.size.div_ceil(GRID))
            .filter(|index| !self.sidecar.materialized.contains(index))
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
        self.file.write_all_at(&data[..len], start)?;
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
        self.file.write_all_at(data, offset)?;
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
        self.file.read_exact_at(buffer, offset)
    }

    /// Changes the size. A cut inside a missing base block must have been
    /// materialized ([`Staged::blocks_to_materialize_for_truncate`]).
    pub fn truncate(&mut self, size: u64) -> io::Result<()> {
        // The file first: reopening takes the size from it, so a crash before
        // the sidecar save never exposes the base tail as zeros.
        self.file.set_len(size)?;
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

    /// `fdatasync` of data and sidecar (fsync, §5.6).
    pub fn sync(&self) -> io::Result<()> {
        self.file.sync_data()?;
        File::open(self.dir.sidecar_path(self.ino))?.sync_data()
    }

    fn save(&mut self) -> io::Result<()> {
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
        std::fs::rename(&tmp, &path)
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
        assert_eq!(staged.missing_blocks(), vec![0, 2]);
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
        assert_eq!(staged.missing_blocks(), vec![0]);
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
        assert_eq!(staged.missing_blocks(), vec![0]);
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
        assert_eq!(staged.missing_blocks(), vec![0]);
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
        assert_eq!(staged.missing_blocks(), vec![0]);
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
