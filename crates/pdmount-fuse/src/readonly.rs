//! A mount over a read-only tree that is not an engine (M5.13, D113): the
//! Photos tree. [`RoFs`] maps fuser's callbacks onto a [`ReadOnlyTree`];
//! every change answers EROFS without asking the tree, and the kernel mount
//! is read-only too (`-o ro`), since such a tree never opens for writes.
//!
//! The tree is a mount of its own (a `photos` root has one tree), so this
//! adapter has no router and no slots. Hot operations are answered on the
//! fuser thread; a lookup or listing whose folder's entries are still
//! arriving waits on a tokio task for [`ReadOnlyTree::resolve`], bounded by
//! the metadata deadline, and a read that misses a block downloads it
//! there, bounded by the block deadline, as in [`crate::PdFs`]. Every
//! callback runs inside `catch_unwind`.

use super::convert::{errno, file_attr, fill_dir, xattr_reply};
use super::mount::{Mounted, fs_name, notify_loop};
use super::{BLKSIZE, FUSER_THREADS, SLOW_PATH_LIMIT, deadline_for};
use fuser::{
    Errno, FileAttr, FileHandle, Filesystem, FopenFlags, Generation, INodeNo, InitFlags,
    KernelConfig, MountOption, OpenAccMode, OpenFlags, RenameFlags, ReplyAttr, ReplyCreate,
    ReplyData, ReplyDirectory, ReplyDirectoryPlus, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs,
    ReplyWrite, ReplyXattr, Request, TimeOrNow,
};
use pdmount_backend::NodeKind;
use pdmount_core::cache::ReadFrom;
use pdmount_core::engine::MAY_WRITE;
use pdmount_core::errors::{CoreError, Result};
use pdmount_core::metrics::{self, Metrics, Op, Stopwatch};
use pdmount_core::readonly::ReadOnlyTree;
use pdmount_core::vfs::{Attr, Invalidation, Lookup};
use std::ffi::OsStr;
use std::future::Future;
use std::os::unix::ffi::OsStrExt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::{Semaphore, mpsc};
use tracing::Instrument;

/// How often a request asks the tree again after waiting for its entries,
/// at most; the deadline usually ends it first.
const RESOLVE_ROUNDS: usize = 10_000;

/// Mounts `tree` read-only at `mountpoint` and starts the thread that sends
/// its kernel invalidations. Blocks until the kernel handshake is done;
/// call from a plain thread or `spawn_blocking`.
pub fn mount_read_only(
    tree: Arc<dyn ReadOnlyTree>,
    invalidations: mpsc::UnboundedReceiver<Invalidation>,
    mountpoint: &Path,
) -> std::io::Result<Mounted> {
    let mut config = fuser::Config::default();
    config.mount_options = vec![
        fs_name(),
        MountOption::Subtype("pdmount".into()),
        MountOption::RO,
        MountOption::NoAtime,
        MountOption::NoDev,
        MountOption::NoSuid,
    ];
    config.n_threads = Some(FUSER_THREADS);
    config.clone_fd = true;
    let fs = RoFs::new(tree);
    let session = fuser::Session::new(fs, mountpoint, &config)?;
    let notifier = session.notifier();
    std::thread::Builder::new()
        .name("pdmount-notify".into())
        .spawn(move || notify_loop(notifier, invalidations, Ok))?;
    Ok(Mounted::of(session.spawn()?))
}

/// The `Filesystem` of a read-only tree.
pub struct RoFs {
    tree: Arc<dyn ReadOnlyTree>,
    slow: Arc<Semaphore>,
    uid: u32,
    gid: u32,
    attr_ttl: Duration,
    entry_ttl: Duration,
    negative_ttl: Duration,
    metadata_deadline: Duration,
    block_deadline: Duration,
}

impl RoFs {
    pub fn new(tree: Arc<dyn ReadOnlyTree>) -> Self {
        let config = tree.config();
        Self {
            attr_ttl: config.kernel.attr_ttl.0,
            entry_ttl: config.kernel.entry_ttl.0,
            negative_ttl: config.kernel.negative_ttl.0,
            metadata_deadline: config.timeouts.metadata.0,
            block_deadline: config.timeouts.block.0,
            slow: Arc::new(Semaphore::new(SLOW_PATH_LIMIT)),
            uid: nix::unistd::getuid().as_raw(),
            gid: nix::unistd::getgid().as_raw(),
            tree,
        }
    }

    fn deadline(&self, normal: Duration) -> Duration {
        deadline_for(
            self.tree.health().state(),
            normal,
            self.tree.config().timeouts.offline_read.0,
        )
    }

    fn file_attr(&self, attr: &Attr) -> FileAttr {
        file_attr(attr, self.uid, self.gid)
    }

    /// Runs a callback body as a hot-path handler, catching panics.
    fn handle(&self, op: Op, req: &Request, ino: u64, body: impl FnOnce(Stopwatch)) {
        let metrics = Arc::clone(self.tree.metrics());
        let watch = Stopwatch::new(Arc::clone(&metrics), op, req.unique().into(), Some(ino));
        if catch_unwind(AssertUnwindSafe(|| metrics::hot(|| body(watch)))).is_err() {
            Metrics::inc(&metrics.handler_panics);
            tracing::error!(op = op.as_str(), "FUSE handler panicked; answered EIO");
        }
    }

    /// Moves the rest of a request to a tokio task with a deadline.
    fn slow<T, F>(
        &self,
        mut watch: Stopwatch,
        deadline: Duration,
        work: F,
        reply: impl FnOnce(Result<T>) + Send + 'static,
    ) where
        T: Send + 'static,
        F: Future<Output = Result<T>> + Send + 'static,
    {
        let permits = Arc::clone(&self.slow);
        let metrics = Arc::clone(self.tree.metrics());
        let span = watch.slow_path();
        let op = watch.op().as_str();
        let task = async move {
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
                && error.errno() == nix::libc::EIO
            {
                tracing::warn!(op, %error, "request failed; answered EIO");
            }
            reply(result);
            drop(watch);
        };
        self.tree.runtime().spawn(task.instrument(span));
    }

    /// Runs `attempt` on the hot path; while it needs a folder's entries,
    /// waits for them on the slow path and asks again.
    fn retrying<T, A>(
        &self,
        watch: Stopwatch,
        attempt: A,
        reply: impl FnOnce(Result<T>) + Send + 'static,
    ) where
        T: Send + 'static,
        A: Fn(&dyn ReadOnlyTree) -> Result<T> + Send + 'static,
    {
        match attempt(self.tree.as_ref()) {
            Err(error @ CoreError::NeedsListing(_)) => {
                let tree = Arc::clone(&self.tree);
                self.slow(
                    watch,
                    self.deadline(self.metadata_deadline),
                    async move {
                        let mut error = error;
                        for _ in 0..RESOLVE_ROUNDS {
                            tree.resolve(&error).await?;
                            match attempt(tree.as_ref()) {
                                Err(next @ CoreError::NeedsListing(_)) => error = next,
                                result => return result,
                            }
                        }
                        Err(error)
                    },
                    reply,
                );
            }
            result => reply(result),
        }
    }

    fn lookup_reply(&self) -> impl FnOnce(Result<Lookup>, ReplyEntry) + Send + 'static {
        let (uid, gid) = (self.uid, self.gid);
        let (attr_ttl, entry_ttl, negative_ttl) =
            (self.attr_ttl, self.entry_ttl, self.negative_ttl);
        move |result, reply| match result {
            Ok(Lookup::Found(attr)) => reply.entry_with_ttls(
                &attr_ttl,
                &entry_ttl,
                &file_attr(&attr, uid, gid),
                Generation(0),
            ),
            // A zero node id with a TTL is a cached negative entry (§5.4).
            Ok(Lookup::Negative) => {
                let negative = Attr {
                    ino: 0,
                    kind: NodeKind::File,
                    size: 0,
                    mtime: std::time::UNIX_EPOCH,
                    ctime: std::time::UNIX_EPOCH,
                    atime: std::time::UNIX_EPOCH,
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
            Err(error) => reply.error(errno(&error)),
        }
    }
}

/// What a request that would change the tree gets.
fn read_only() -> Errno {
    Errno::EROFS
}

impl Filesystem for RoFs {
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> std::io::Result<()> {
        for flag in [
            InitFlags::FUSE_DO_READDIRPLUS,
            InitFlags::FUSE_READDIRPLUS_AUTO,
            InitFlags::FUSE_PARALLEL_DIROPS,
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

    fn lookup(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        self.handle(Op::Lookup, req, parent.0, |watch| {
            let respond = self.lookup_reply();
            let name = name.to_owned();
            self.retrying(
                watch,
                move |tree| tree.lookup(parent.0, name.as_bytes()),
                move |result| respond(result, reply),
            );
        });
    }

    fn getattr(&self, req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        self.handle(Op::Getattr, req, ino.0, |_watch| {
            match self.tree.getattr(ino.0) {
                Ok(attr) => reply.attr(&self.attr_ttl, &self.file_attr(&attr)),
                Err(error) => reply.error(errno(&error)),
            }
        });
    }

    fn setattr(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        _size: Option<u64>,
        _atime: Option<TimeOrNow>,
        _mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        reply.error(read_only());
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
        reply.error(read_only());
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
        reply.error(read_only());
    }

    fn unlink(&self, _req: &Request, _parent: INodeNo, _name: &OsStr, reply: ReplyEmpty) {
        reply.error(read_only());
    }

    fn rmdir(&self, _req: &Request, _parent: INodeNo, _name: &OsStr, reply: ReplyEmpty) {
        reply.error(read_only());
    }

    fn symlink(
        &self,
        _req: &Request,
        _parent: INodeNo,
        _link_name: &OsStr,
        _target: &Path,
        reply: ReplyEntry,
    ) {
        reply.error(read_only());
    }

    fn rename(
        &self,
        _req: &Request,
        _parent: INodeNo,
        _name: &OsStr,
        _newparent: INodeNo,
        _newname: &OsStr,
        _flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        reply.error(read_only());
    }

    fn link(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _newparent: INodeNo,
        _newname: &OsStr,
        reply: ReplyEntry,
    ) {
        reply.error(read_only());
    }

    fn open(&self, req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        self.handle(Op::Open, req, ino.0, |_watch| {
            if flags.acc_mode() != OpenAccMode::O_RDONLY || flags.0 & nix::libc::O_TRUNC != 0 {
                return reply.error(read_only());
            }
            match self.tree.open(ino.0) {
                Ok((handle, keep_cache)) => {
                    let flags = if keep_cache {
                        FopenFlags::FOPEN_KEEP_CACHE
                    } else {
                        FopenFlags::empty()
                    };
                    reply.opened(FileHandle(handle), flags);
                }
                Err(error) => reply.error(errno(&error)),
            }
        });
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
        reply.error(read_only());
    }

    fn read(
        &self,
        req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        reply: ReplyData,
    ) {
        self.handle(Op::ReadRam, req, ino.0, |mut watch| {
            match self.tree.read_local(fh.0, offset, size) {
                Ok(Some((data, from))) => {
                    watch.set_op(match from {
                        ReadFrom::Ram => Op::ReadRam,
                        ReadFrom::Disk => Op::ReadDisk,
                        ReadFrom::Staging => Op::ReadStaging,
                    });
                    reply.data(&data);
                }
                Ok(None) => {
                    watch.set_op(Op::ReadSlow);
                    let tree = Arc::clone(&self.tree);
                    self.slow(
                        watch,
                        self.deadline(self.block_deadline),
                        async move { tree.read(fh.0, offset, size).await },
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
        reply.error(read_only());
    }

    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _lock_owner: fuser::LockOwner,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn release(
        &self,
        req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        self.handle(Op::Release, req, ino.0, |_watch| {
            self.tree.release(fh.0);
            reply.ok();
        });
    }

    fn fsync(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn opendir(&self, req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        self.handle(Op::Opendir, req, ino.0, |watch| {
            self.retrying(
                watch,
                move |tree| tree.opendir(ino.0),
                move |result| match result {
                    Ok((handle, keep_cache)) => {
                        let flags = if keep_cache {
                            FopenFlags::FOPEN_CACHE_DIR | FopenFlags::FOPEN_KEEP_CACHE
                        } else {
                            FopenFlags::FOPEN_CACHE_DIR
                        };
                        reply.opened(FileHandle(handle), flags);
                    }
                    Err(error) => reply.error(errno(&error)),
                },
            );
        });
    }

    fn readdir(
        &self,
        req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        self.handle(Op::Readdir, req, ino.0, |_watch| {
            match self.tree.dir_snapshot(fh.0) {
                Ok(entries) => {
                    fill_dir(&entries, offset, &mut reply);
                    reply.ok();
                }
                Err(error) => reply.error(errno(&error)),
            }
        });
    }

    fn readdirplus(
        &self,
        req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectoryPlus,
    ) {
        self.handle(Op::Readdirplus, req, ino.0, |_watch| {
            let entries = match self.tree.dir_snapshot(fh.0) {
                Ok(entries) => entries,
                Err(error) => return reply.error(errno(&error)),
            };
            for (index, entry) in entries.iter().enumerate().skip(offset as usize) {
                // A photo gone since the open is left out.
                let Ok(attr) = self.tree.getattr(entry.ino) else {
                    continue;
                };
                // No lookup reference for `.` and `..`.
                let ttl = if entry.name == "." || entry.name == ".." {
                    Duration::ZERO
                } else {
                    self.entry_ttl
                };
                if reply.add(
                    INodeNo(entry.ino),
                    index as u64 + 1,
                    &entry.name,
                    &ttl,
                    &self.file_attr(&attr),
                    Generation(0),
                ) {
                    break;
                }
            }
            reply.ok();
        });
    }

    fn releasedir(
        &self,
        req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        reply: ReplyEmpty,
    ) {
        self.handle(Op::Releasedir, req, ino.0, |_watch| {
            self.tree.releasedir(fh.0);
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
        reply.ok();
    }

    fn statfs(&self, req: &Request, ino: INodeNo, reply: ReplyStatfs) {
        self.handle(Op::Statfs, req, ino.0, |_watch| {
            let s = self.tree.statfs();
            reply.statfs(
                s.blocks,
                s.bfree,
                s.bavail,
                s.files,
                s.ffree,
                s.bsize.clamp(1, BLKSIZE),
                s.namelen,
                s.bsize.clamp(1, BLKSIZE),
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
        reply.error(Errno::NO_XATTR);
    }

    fn listxattr(&self, _req: &Request, _ino: INodeNo, size: u32, reply: ReplyXattr) {
        xattr_reply(reply, &[], size);
    }

    fn access(&self, req: &Request, ino: INodeNo, mask: fuser::AccessFlags, reply: ReplyEmpty) {
        // Without `default_permissions` the daemon checks modes (D031).
        self.handle(Op::Access, req, ino.0, |_watch| {
            if mask.bits() as u32 & MAY_WRITE != 0 {
                return reply.error(read_only());
            }
            match self.tree.getattr(ino.0) {
                Ok(_) => reply.ok(),
                Err(error) => reply.error(errno(&error)),
            }
        });
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
        reply.error(read_only());
    }

    fn removexattr(&self, _req: &Request, _ino: INodeNo, _name: &OsStr, reply: ReplyEmpty) {
        reply.error(read_only());
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
        reply.error(read_only());
    }
}
