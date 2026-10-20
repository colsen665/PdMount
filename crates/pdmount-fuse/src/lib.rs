//! FUSE adapter (DESIGN.md §4): maps fuser callbacks onto [`Engine`].
//!
//! Hot operations are answered on the fuser thread inside a
//! [`metrics::hot`] scope. Operations that need the network (first listing of
//! a directory, a block that is not cached) move their Reply into a tokio
//! task, bounded by a semaphore and a deadline; on expiry the request gets
//! EIO while the download continues and warms the cache. Every callback runs
//! inside `catch_unwind`: a panic drops the Reply, which answers EIO, and the
//! mount stays up.
//!
//! M1 is read-only: the mount uses `ro` and every write-side operation
//! returns EROFS.

use fuser::{
    Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo, InitFlags,
    KernelConfig, MountOption, Notifier, OpenFlags, ReplyAttr, ReplyCreate, ReplyData,
    ReplyDirectory, ReplyDirectoryPlus, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite,
    ReplyXattr, Request,
};
use pdmount_backend::NodeKind;
use pdmount_core::engine::Engine;
use pdmount_core::errors::{CoreError, Result};
use pdmount_core::metrics::{self, Metrics};
use pdmount_core::vfs::{Attr, Invalidation, Lookup};
use std::ffi::OsStr;
use std::future::Future;
use std::os::unix::ffi::OsStrExt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Semaphore, mpsc};

/// Concurrent slow-path requests (§4.2); matches `max_background`.
const SLOW_PATH_LIMIT: usize = 64;
const FUSER_THREADS: usize = 4;
const BLKSIZE: u32 = 128 * 1024;

pub struct PdFs {
    engine: Arc<Engine>,
    slow: Arc<Semaphore>,
    uid: u32,
    gid: u32,
    attr_ttl: Duration,
    entry_ttl: Duration,
    negative_ttl: Duration,
    metadata_deadline: Duration,
    block_deadline: Duration,
}

impl PdFs {
    pub fn new(engine: Arc<Engine>, uid: u32, gid: u32) -> Self {
        let config = engine.config();
        Self {
            attr_ttl: config.kernel.attr_ttl.0,
            entry_ttl: config.kernel.entry_ttl.0,
            negative_ttl: config.kernel.negative_ttl.0,
            metadata_deadline: config.timeouts.metadata.0,
            block_deadline: config.timeouts.block.0,
            engine,
            slow: Arc::new(Semaphore::new(SLOW_PATH_LIMIT)),
            uid,
            gid,
        }
    }

    /// Runs a callback body as a hot-path handler, catching panics.
    fn handle(&self, op: &'static str, body: impl FnOnce()) {
        if catch_unwind(AssertUnwindSafe(|| metrics::hot(body))).is_err() {
            Metrics::inc(&self.engine.metrics().handler_panics);
            tracing::error!(op, "FUSE handler panicked; answered EIO");
        }
    }

    /// Moves the rest of a request to a tokio task with a deadline.
    fn slow<T, F>(
        &self,
        op: &'static str,
        deadline: Duration,
        work: F,
        reply: impl FnOnce(Result<T>) + Send + 'static,
    ) where
        T: Send + 'static,
        F: Future<Output = Result<T>> + Send + 'static,
    {
        let permits = Arc::clone(&self.slow);
        let metrics = Arc::clone(self.engine.metrics());
        self.engine.runtime().spawn(async move {
            let result = tokio::time::timeout(deadline, async move {
                let _permit = permits
                    .acquire_owned()
                    .await
                    .map_err(|_| CoreError::Internal("slow path closed".into()))?;
                work.await
            })
            .await
            .unwrap_or_else(|_| {
                Metrics::inc(&metrics.deadline_expired);
                tracing::warn!(op, ?deadline, "slow-path deadline expired; answered EIO");
                Err(CoreError::Timeout)
            });
            if let Err(error) = &result
                && !matches!(error, CoreError::NotFound)
            {
                tracing::debug!(op, %error, "request failed");
            }
            reply(result);
        });
    }

    fn file_attr(&self, attr: &Attr) -> FileAttr {
        file_attr(attr, self.uid, self.gid)
    }

    fn reply_lookup(&self, lookup: Lookup, reply: ReplyEntry) {
        reply_lookup(
            lookup,
            reply,
            self.uid,
            self.gid,
            self.attr_ttl,
            self.entry_ttl,
            self.negative_ttl,
        );
    }
}

fn errno(error: &CoreError) -> Errno {
    Errno::from_i32(error.errno())
}

fn file_attr(attr: &Attr, uid: u32, gid: u32) -> FileAttr {
    FileAttr {
        ino: INodeNo(attr.ino),
        size: attr.size,
        blocks: attr.size.div_ceil(512),
        atime: attr.mtime,
        mtime: attr.mtime,
        ctime: attr.mtime,
        crtime: attr.mtime,
        kind: match attr.kind {
            NodeKind::File => FileType::RegularFile,
            NodeKind::Folder => FileType::Directory,
        },
        perm: attr.perm as u16,
        nlink: attr.nlink,
        uid,
        gid,
        rdev: 0,
        blksize: BLKSIZE,
        flags: 0,
    }
}

fn file_type(kind: NodeKind) -> FileType {
    match kind {
        NodeKind::File => FileType::RegularFile,
        NodeKind::Folder => FileType::Directory,
    }
}

fn reply_lookup(
    lookup: Lookup,
    reply: ReplyEntry,
    uid: u32,
    gid: u32,
    attr_ttl: Duration,
    entry_ttl: Duration,
    negative_ttl: Duration,
) {
    match lookup {
        Lookup::Found(attr) => reply.entry_with_ttls(
            &attr_ttl,
            &entry_ttl,
            &file_attr(&attr, uid, gid),
            Generation(0),
        ),
        // A zero node id with a TTL is a cached negative entry (§5.4).
        Lookup::Negative => {
            let negative = Attr {
                ino: 0,
                kind: NodeKind::File,
                size: 0,
                mtime: std::time::UNIX_EPOCH,
                perm: 0,
                nlink: 0,
            };
            reply.entry_with_ttls(
                &Duration::ZERO,
                &negative_ttl,
                &file_attr(&negative, uid, gid),
                Generation(0),
            );
        }
        Lookup::NeedsListing => reply.error(Errno::EIO),
    }
}

/// Adds directory entries from `offset` on; returns when the buffer is full.
fn fill_dir(entries: &[pdmount_core::vfs::DirEntry], offset: u64, reply: &mut ReplyDirectory) {
    for (index, entry) in entries.iter().enumerate().skip(offset as usize) {
        if reply.add(
            INodeNo(entry.ino),
            index as u64 + 1,
            file_type(entry.kind),
            &entry.name,
        ) {
            break;
        }
    }
}

impl Filesystem for PdFs {
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> std::io::Result<()> {
        for flag in [
            InitFlags::FUSE_DO_READDIRPLUS,
            InitFlags::FUSE_READDIRPLUS_AUTO,
            InitFlags::FUSE_PARALLEL_DIROPS,
            InitFlags::FUSE_ATOMIC_O_TRUNC,
        ] {
            if let Err(unsupported) = config.add_capabilities(flag) {
                tracing::info!(
                    ?unsupported,
                    "kernel does not support a requested FUSE capability"
                );
            }
        }
        if let Err(max) = config.set_max_readahead(u32::MAX) {
            let _ = config.set_max_readahead(max);
        }
        if let Err(max) = config.set_max_background(SLOW_PATH_LIMIT as u16) {
            let _ = config.set_max_background(max);
        }
        Ok(())
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        self.handle("lookup", || {
            match self.engine.lookup(parent.0, name.as_bytes()) {
                Ok(Lookup::NeedsListing) => {
                    let engine = Arc::clone(&self.engine);
                    let name = name.to_owned();
                    let (uid, gid) = (self.uid, self.gid);
                    let ttls = (self.attr_ttl, self.entry_ttl, self.negative_ttl);
                    self.slow(
                        "lookup",
                        self.metadata_deadline,
                        async move {
                            engine.ensure_listed(parent.0).await?;
                            engine.lookup(parent.0, name.as_bytes())
                        },
                        move |result| match result {
                            Ok(lookup) => {
                                reply_lookup(lookup, reply, uid, gid, ttls.0, ttls.1, ttls.2)
                            }
                            Err(error) => reply.error(errno(&error)),
                        },
                    );
                }
                Ok(lookup) => self.reply_lookup(lookup, reply),
                Err(error) => reply.error(errno(&error)),
            }
        });
    }

    fn forget(&self, _req: &Request, ino: INodeNo, nlookup: u64) {
        self.handle("forget", || self.engine.forget(ino.0, nlookup));
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        self.handle("getattr", || match self.engine.getattr(ino.0) {
            Ok(attr) => reply.attr(&self.attr_ttl, &self.file_attr(&attr)),
            Err(error) => reply.error(errno(&error)),
        });
    }

    fn open(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        self.handle("open", || match self.engine.open_file(ino.0) {
            Ok((handle, keep_cache)) => {
                let flags = if keep_cache {
                    FopenFlags::FOPEN_KEEP_CACHE
                } else {
                    FopenFlags::empty()
                };
                reply.opened(FileHandle(handle), flags);
            }
            Err(error) => reply.error(errno(&error)),
        });
    }

    fn read(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        reply: ReplyData,
    ) {
        self.handle("read", || {
            match self.engine.read_local(fh.0, offset, size) {
                Ok(Some(data)) => reply.data(&data),
                Ok(None) => {
                    let engine = Arc::clone(&self.engine);
                    self.slow(
                        "read",
                        self.block_deadline,
                        async move { engine.read(fh.0, offset, size).await },
                        move |result| match result {
                            Ok(data) => reply.data(&data),
                            Err(error) => reply.error(errno(&error)),
                        },
                    );
                }
                Err(error) => reply.error(errno(&error)),
            }
        });
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        self.handle("release", || {
            self.engine.release_file(fh.0);
            reply.ok();
        });
    }

    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _lock_owner: fuser::LockOwner,
        reply: ReplyEmpty,
    ) {
        self.handle("flush", || reply.ok());
    }

    fn fsync(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        self.handle("fsync", || reply.ok());
    }

    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        self.handle("opendir", || match self.engine.opendir(ino.0) {
            Ok(Some(handle)) => reply.opened(FileHandle(handle), FopenFlags::FOPEN_CACHE_DIR),
            Ok(None) => {
                let engine = Arc::clone(&self.engine);
                self.slow(
                    "opendir",
                    self.metadata_deadline,
                    async move {
                        engine.ensure_listed(ino.0).await?;
                        engine
                            .opendir(ino.0)?
                            .ok_or(CoreError::Internal("listing vanished".into()))
                    },
                    move |result| match result {
                        Ok(handle) => reply.opened(FileHandle(handle), FopenFlags::FOPEN_CACHE_DIR),
                        Err(error) => reply.error(errno(&error)),
                    },
                );
            }
            Err(error) => reply.error(errno(&error)),
        });
    }

    fn readdir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        self.handle("readdir", || match self.engine.dir_snapshot(fh.0) {
            Ok(entries) => {
                fill_dir(&entries, offset, &mut reply);
                reply.ok();
            }
            Err(error) => reply.error(errno(&error)),
        });
    }

    fn readdirplus(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectoryPlus,
    ) {
        self.handle("readdirplus", || {
            let entries = match self.engine.dir_snapshot(fh.0) {
                Ok(entries) => entries,
                Err(error) => return reply.error(errno(&error)),
            };
            for (index, entry) in entries.iter().enumerate().skip(offset as usize) {
                let next = index as u64 + 1;
                if entry.name == "." || entry.name == ".." {
                    // No lookup reference for these; the kernel ignores the attributes.
                    let Ok(attr) = self.engine.getattr(entry.ino) else {
                        continue;
                    };
                    let full = reply.add(
                        INodeNo(entry.ino),
                        next,
                        &entry.name,
                        &Duration::ZERO,
                        &self.file_attr(&attr),
                        Generation(0),
                    );
                    if full {
                        break;
                    }
                    continue;
                }
                let Ok(attr) = self.engine.getattr(entry.ino) else {
                    continue;
                };
                if reply.add(
                    INodeNo(entry.ino),
                    next,
                    &entry.name,
                    &self.entry_ttl,
                    &self.file_attr(&attr),
                    Generation(0),
                ) {
                    break;
                }
                // Only entries that fit count as kernel references.
                self.engine.readdirplus_entry(entry.ino);
            }
            reply.ok();
        });
    }

    fn releasedir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        reply: ReplyEmpty,
    ) {
        self.handle("releasedir", || {
            self.engine.releasedir(fh.0);
            reply.ok();
        });
    }

    fn fsyncdir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        self.handle("fsyncdir", || reply.ok());
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        self.handle("statfs", || {
            let s = self.engine.statfs();
            reply.statfs(
                s.blocks, s.bfree, s.bavail, s.files, s.ffree, s.bsize, s.namelen, s.bsize,
            );
        });
    }

    fn getxattr(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _name: &OsStr,
        _size: u32,
        reply: ReplyXattr,
    ) {
        self.handle("getxattr", || reply.error(Errno::NO_XATTR));
    }

    fn listxattr(&self, _req: &Request, _ino: INodeNo, size: u32, reply: ReplyXattr) {
        self.handle("listxattr", || {
            if size == 0 {
                reply.size(0);
            } else {
                reply.data(&[]);
            }
        });
    }

    fn access(&self, _req: &Request, _ino: INodeNo, _mask: fuser::AccessFlags, reply: ReplyEmpty) {
        // `default_permissions` makes the kernel check modes itself.
        self.handle("access", || reply.ok());
    }

    // ---- write side: read-only in M1 ----

    fn setattr(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        _size: Option<u64>,
        _atime: Option<fuser::TimeOrNow>,
        _mtime: Option<fuser::TimeOrNow>,
        _ctime: Option<std::time::SystemTime>,
        _fh: Option<FileHandle>,
        _crtime: Option<std::time::SystemTime>,
        _chgtime: Option<std::time::SystemTime>,
        _bkuptime: Option<std::time::SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        reply.error(Errno::EROFS);
    }

    fn mknod(
        &self,
        _req: &Request,
        _parent: INodeNo,
        _name: &OsStr,
        _mode: u32,
        _umask: u32,
        _rdev: u32,
        reply: ReplyEntry,
    ) {
        reply.error(Errno::EROFS);
    }

    fn mkdir(
        &self,
        _req: &Request,
        _parent: INodeNo,
        _name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        reply.error(Errno::EROFS);
    }

    fn unlink(&self, _req: &Request, _parent: INodeNo, _name: &OsStr, reply: ReplyEmpty) {
        reply.error(Errno::EROFS);
    }

    fn rmdir(&self, _req: &Request, _parent: INodeNo, _name: &OsStr, reply: ReplyEmpty) {
        reply.error(Errno::EROFS);
    }

    fn symlink(
        &self,
        _req: &Request,
        _parent: INodeNo,
        _link_name: &OsStr,
        _target: &Path,
        reply: ReplyEntry,
    ) {
        reply.error(Errno::EPERM);
    }

    fn rename(
        &self,
        _req: &Request,
        _parent: INodeNo,
        _name: &OsStr,
        _newparent: INodeNo,
        _newname: &OsStr,
        _flags: fuser::RenameFlags,
        reply: ReplyEmpty,
    ) {
        reply.error(Errno::EROFS);
    }

    fn link(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _newparent: INodeNo,
        _newname: &OsStr,
        reply: ReplyEntry,
    ) {
        reply.error(Errno::EPERM);
    }

    fn write(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _offset: u64,
        _data: &[u8],
        _write_flags: fuser::WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        reply: ReplyWrite,
    ) {
        reply.error(Errno::EROFS);
    }

    fn create(
        &self,
        _req: &Request,
        _parent: INodeNo,
        _name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        reply.error(Errno::EROFS);
    }

    fn setxattr(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _name: &OsStr,
        _value: &[u8],
        _flags: i32,
        _position: u32,
        reply: ReplyEmpty,
    ) {
        reply.error(Errno::EROFS);
    }

    fn removexattr(&self, _req: &Request, _ino: INodeNo, _name: &OsStr, reply: ReplyEmpty) {
        reply.error(Errno::EROFS);
    }

    fn fallocate(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _offset: u64,
        _length: u64,
        _mode: i32,
        reply: ReplyEmpty,
    ) {
        reply.error(Errno::EROFS);
    }

    fn copy_file_range(
        &self,
        _req: &Request,
        _ino_in: INodeNo,
        _fh_in: FileHandle,
        _offset_in: u64,
        _ino_out: INodeNo,
        _fh_out: FileHandle,
        _offset_out: u64,
        _len: u64,
        _flags: fuser::CopyFileRangeFlags,
        reply: ReplyWrite,
    ) {
        reply.error(Errno::EROFS);
    }
}

/// A running mount.
pub struct Mounted {
    session: fuser::BackgroundSession,
}

impl Mounted {
    /// Whether the session ended, e.g. after an external `fusermount3 -u`.
    pub fn is_finished(&self) -> bool {
        self.session.guard.is_finished()
    }

    pub fn unmount(self) -> std::io::Result<()> {
        self.session.umount_and_join()
    }

    /// Waits until the session ends without unmounting.
    pub fn join(self) -> std::io::Result<()> {
        self.session.join()
    }
}

/// Mounts `engine` read-only at `mountpoint` and starts the notify task.
/// Blocks until the kernel handshake is done; call from a plain thread or
/// `spawn_blocking`, not from an async task.
pub fn mount(
    engine: Arc<Engine>,
    invalidations: mpsc::UnboundedReceiver<Invalidation>,
    mountpoint: &Path,
) -> std::io::Result<Mounted> {
    let uid = nix::unistd::getuid().as_raw();
    let gid = nix::unistd::getgid().as_raw();
    let mut config = fuser::Config::default();
    config.mount_options = vec![
        MountOption::FSName("pdmount".into()),
        MountOption::Subtype("pdmount".into()),
        MountOption::RO,
        MountOption::DefaultPermissions,
        MountOption::NoAtime,
        MountOption::NoDev,
        MountOption::NoSuid,
    ];
    config.n_threads = Some(FUSER_THREADS);
    config.clone_fd = true;
    let session = fuser::Session::new(PdFs::new(engine, uid, gid), mountpoint, &config)?;
    let notifier = session.notifier();
    std::thread::Builder::new()
        .name("pdmount-notify".into())
        .spawn(move || notify_loop(notifier, invalidations))?;
    Ok(Mounted {
        session: session.spawn()?,
    })
}

/// Sends kernel invalidations (§9.5). Runs on its own thread: never on a
/// FUSE thread, never holding engine locks. ENOENT means the kernel had
/// nothing cached.
fn notify_loop(notifier: Notifier, mut invalidations: mpsc::UnboundedReceiver<Invalidation>) {
    while let Some(invalidation) = invalidations.blocking_recv() {
        let result = match &invalidation {
            Invalidation::Entry { parent, name } => {
                notifier.inval_entry(INodeNo(*parent), OsStr::new(name))
            }
            Invalidation::Inode { ino } => notifier.inval_inode(INodeNo(*ino), 0, 0),
            Invalidation::Delete {
                parent,
                child,
                name,
            } => notifier.delete(INodeNo(*parent), INodeNo(*child), OsStr::new(name)),
        };
        match result {
            Ok(()) => tracing::debug!(?invalidation, "kernel cache invalidated"),
            Err(error) if error.raw_os_error() == Some(nix::libc::ENOENT) => {}
            Err(error) => tracing::debug!(?invalidation, %error, "invalidation failed"),
        }
    }
}
