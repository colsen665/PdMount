//! FUSE adapter (DESIGN.md §4): maps fuser callbacks onto [`Engine`].
//!
//! Hot operations are answered on the fuser thread inside a
//! [`metrics::hot`] scope. Operations that need the network (a directory
//! that was never listed, a block that is not cached, a base block a write
//! only partly covers) move their Reply into a tokio task, bounded by a
//! semaphore and a deadline; on expiry the request gets EIO while the work
//! continues and warms the cache. Every callback runs inside `catch_unwind`:
//! a panic drops the Reply, which answers EIO, and the mount stays up.
//!
//! Every request carries a [`Stopwatch`] (§5.8) that travels with its Reply
//! and drops right after it: a reply deferred until the store commit, or
//! sent from the slow path, counts that wait.

use fuser::{
    Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo, InitFlags,
    KernelConfig, MountOption, Notifier, OpenAccMode, OpenFlags, RenameFlags, ReplyAttr,
    ReplyCreate, ReplyData, ReplyDirectory, ReplyDirectoryPlus, ReplyEmpty, ReplyEntry, ReplyOpen,
    ReplyStatfs, ReplyWrite, ReplyXattr, Request, TimeOrNow,
};
use pdmount_backend::NodeKind;
use pdmount_core::cache::ReadFrom;
use pdmount_core::engine::{Engine, OpenMode, SetAttr};
use pdmount_core::errors::{CoreError, Result};
use pdmount_core::health::Connectivity;
use pdmount_core::metrics::{self, Metrics, Op, Stopwatch};
use pdmount_core::vfs::{Attr, Invalidation, Lookup, RenameMode};
use std::ffi::OsStr;
use std::future::Future;
use std::os::unix::ffi::OsStrExt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::{Semaphore, mpsc};
use tracing::Instrument;

/// Concurrent slow-path requests (§4.2); matches `max_background`.
const SLOW_PATH_LIMIT: usize = 64;
const FUSER_THREADS: usize = 4;
// Each fuser thread can hold one scratch slot for a reply across blocks.
const _: () = assert!(FUSER_THREADS <= pdmount_core::pool::SCRATCH_SLOTS);
const BLKSIZE: u32 = 128 * 1024;
/// Slow-path attempts before giving up (listing, then inode numbers, ...).
const SLOW_ATTEMPTS: usize = 4;

/// The deadline of a slow-path request given the connectivity at its start.
fn deadline_for(state: Connectivity, normal: Duration, offline_read: Duration) -> Duration {
    if state == Connectivity::Online {
        normal
    } else {
        normal.min(offline_read)
    }
}

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

    /// Deadline of a metadata request: short while the drive cannot be
    /// reached, so uncached listings fail with EIO instead of hanging (§9.6).
    fn metadata_deadline(&self) -> Duration {
        self.deadline(self.metadata_deadline)
    }

    /// Deadline of a request that may fetch blocks.
    fn block_deadline(&self) -> Duration {
        self.deadline(self.block_deadline)
    }

    fn deadline(&self, normal: Duration) -> Duration {
        deadline_for(
            self.engine.health().state(),
            normal,
            self.engine.config().timeouts.offline_read.0,
        )
    }

    /// Runs a callback body as a hot-path handler, catching panics. The
    /// body gets the request's stopwatch and drops it after the reply.
    fn handle(&self, op: Op, req: &Request, ino: Option<u64>, body: impl FnOnce(Stopwatch)) {
        let watch = Stopwatch::new(
            Arc::clone(self.engine.metrics()),
            op,
            req.unique().into(),
            ino,
        );
        if catch_unwind(AssertUnwindSafe(|| metrics::hot(|| body(watch)))).is_err() {
            Metrics::inc(&self.engine.metrics().handler_panics);
            tracing::error!(op = op.as_str(), "FUSE handler panicked; answered EIO");
        }
    }

    /// Moves the rest of a request to a tokio task with a deadline.
    fn slow<T, F>(
        &self,
        mut watch: Stopwatch,
        deadline: Duration,
        work: F,
        reply: impl FnOnce(Result<T>, Stopwatch) + Send + 'static,
    ) where
        T: Send + 'static,
        F: Future<Output = Result<T>> + Send + 'static,
    {
        let permits = Arc::clone(&self.slow);
        let metrics = Arc::clone(self.engine.metrics());
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
                && !matches!(
                    error,
                    CoreError::NotFound | CoreError::Exists | CoreError::NotEmpty
                )
            {
                // EIO says nothing to the caller; the log has to.
                if error.errno() == nix::libc::EIO {
                    tracing::warn!(op, %error, "request failed; answered EIO");
                } else {
                    tracing::debug!(op, %error, "request failed");
                }
            }
            reply(result, watch);
        };
        self.engine.runtime().spawn(task.instrument(span));
    }

    /// Runs `attempt` on the hot path; if it needs a listing or inode
    /// numbers, provides them on the slow path and tries again.
    fn retrying<T, A>(
        &self,
        watch: Stopwatch,
        attempt: A,
        reply: impl FnOnce(Result<T>, Stopwatch) + Send + 'static,
    ) where
        T: Send + 'static,
        A: Fn() -> Result<T> + Send + 'static,
    {
        match attempt() {
            Err(error) if is_resolvable(&error) => {
                let engine = Arc::clone(&self.engine);
                self.slow(
                    watch,
                    self.metadata_deadline(),
                    async move {
                        let mut error = error;
                        for _ in 0..SLOW_ATTEMPTS {
                            resolve(&engine, &error).await?;
                            match attempt() {
                                Err(next) if is_resolvable(&next) => error = next,
                                result => return result,
                            }
                        }
                        Err(error)
                    },
                    reply,
                );
            }
            result => reply(result, watch),
        }
    }

    fn file_attr(&self, attr: &Attr) -> FileAttr {
        file_attr(attr, self.uid, self.gid)
    }

    /// Sends a successful reply only once the store has committed the rows
    /// the request queued (§5.6); errors go out at once. The stopwatch
    /// stops after the reply.
    fn committed<T: Send + 'static>(
        &self,
        reply: impl FnOnce(Result<T>) + Send + 'static,
    ) -> impl FnOnce(Result<T>, Stopwatch) + Send + 'static {
        let engine = Arc::clone(&self.engine);
        move |result, watch| match result {
            Ok(value) => engine.after_commit(move || {
                reply(Ok(value));
                drop(watch);
            }),
            Err(error) => reply(Err(error)),
        }
    }

    fn entry_reply(&self) -> impl FnOnce(Result<Attr>, ReplyEntry) + Send + 'static {
        let (uid, gid, attr_ttl, entry_ttl) = (self.uid, self.gid, self.attr_ttl, self.entry_ttl);
        move |result, reply| match result {
            Ok(attr) => reply.entry_with_ttls(
                &attr_ttl,
                &entry_ttl,
                &file_attr(&attr, uid, gid),
                Generation(0),
            ),
            Err(error) => reply.error(errno(&error)),
        }
    }

    fn lookup_reply(&self) -> impl FnOnce(Result<Lookup>, ReplyEntry) + Send + 'static {
        let entry = self.entry_reply();
        let (uid, gid, negative_ttl) = (self.uid, self.gid, self.negative_ttl);
        move |result, reply| match result {
            Ok(Lookup::Found(attr)) => entry(Ok(attr), reply),
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

/// Answers an empty reply with a result.
fn reply_empty(reply: ReplyEmpty) -> impl FnOnce(Result<()>) + Send + 'static {
    move |result| match result {
        Ok(()) => reply.ok(),
        Err(error) => reply.error(errno(&error)),
    }
}

fn is_resolvable(error: &CoreError) -> bool {
    matches!(error, CoreError::NeedsListing(_) | CoreError::NeedsInos)
}

async fn resolve(engine: &Arc<Engine>, error: &CoreError) -> Result<()> {
    match error {
        CoreError::NeedsListing(dir) => engine.ensure_listed(*dir).await,
        CoreError::NeedsInos => engine.reserve_inos(16).await,
        other => Err(other.clone()),
    }
}

/// The size protocol of `getxattr` and `listxattr`: a zero `size` asks for
/// the length, a smaller buffer is ERANGE.
fn xattr_reply(reply: ReplyXattr, data: &[u8], size: u32) {
    if size == 0 {
        reply.size(data.len() as u32);
    } else if (size as usize) < data.len() {
        reply.error(Errno::ERANGE);
    } else {
        reply.data(data);
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
        atime: attr.atime,
        mtime: attr.mtime,
        ctime: attr.ctime,
        crtime: attr.mtime,
        kind: file_type(attr.kind),
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

fn open_mode(flags: i32) -> OpenMode {
    let access = OpenFlags(flags).acc_mode();
    OpenMode {
        read: access != OpenAccMode::O_WRONLY,
        write: access != OpenAccMode::O_RDONLY,
        truncate: flags & nix::libc::O_TRUNC != 0,
    }
}

fn time(value: TimeOrNow) -> SystemTime {
    match value {
        TimeOrNow::SpecificTime(time) => time,
        TimeOrNow::Now => SystemTime::now(),
    }
}

impl Filesystem for PdFs {
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> std::io::Result<()> {
        let flags = [
            InitFlags::FUSE_DO_READDIRPLUS,
            InitFlags::FUSE_READDIRPLUS_AUTO,
            InitFlags::FUSE_PARALLEL_DIROPS,
            InitFlags::FUSE_ATOMIC_O_TRUNC,
        ];
        for flag in flags {
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
        if let Err(max) = config.set_max_write(1 << 20) {
            let _ = config.set_max_write(max);
        }
        Ok(())
    }

    fn lookup(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        self.handle(Op::Lookup, req, Some(parent.0), |watch| {
            let engine = Arc::clone(&self.engine);
            let name = name.to_owned();
            let respond = self.lookup_reply();
            self.retrying(
                watch,
                move || engine.lookup(parent.0, name.as_bytes()),
                move |result, _watch| respond(result, reply),
            );
        });
    }

    fn forget(&self, req: &Request, ino: INodeNo, nlookup: u64) {
        self.handle(Op::Forget, req, Some(ino.0), |_watch| {
            self.engine.forget(ino.0, nlookup);
        });
    }

    fn getattr(&self, req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        self.handle(Op::Getattr, req, Some(ino.0), |_watch| {
            match self.engine.getattr(ino.0) {
                Ok(attr) => reply.attr(&self.attr_ttl, &self.file_attr(&attr)),
                Err(error) => reply.error(errno(&error)),
            }
        });
    }

    fn setattr(
        &self,
        req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        self.handle(Op::Setattr, req, Some(ino.0), |watch| {
            // Files belong to the mounting user; only no-op chowns pass.
            if uid.is_some_and(|u| u != self.uid) || gid.is_some_and(|g| g != self.gid) {
                return reply.error(Errno::EPERM);
            }
            let change = SetAttr {
                mode,
                size,
                mtime: mtime.map(time),
                atime: atime.map(time),
                handle: fh.map(|fh| fh.0),
            };
            let (uid, gid, ttl) = (self.uid, self.gid, self.attr_ttl);
            let respond = self.committed(move |result: Result<Attr>| match result {
                Ok(attr) => reply.attr(&ttl, &file_attr(&attr, uid, gid)),
                Err(error) => reply.error(errno(&error)),
            });
            match self.engine.setattr(ino.0, &change) {
                Err(CoreError::NeedsMaterialize(_)) => {
                    let engine = Arc::clone(&self.engine);
                    self.slow(
                        watch,
                        self.block_deadline(),
                        async move { engine.setattr_slow(ino.0, change).await },
                        respond,
                    );
                }
                result => respond(result, watch),
            }
        });
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
        // §8.7: special files cannot exist remotely.
        reply.error(Errno::EPERM);
    }

    fn mkdir(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        reply: ReplyEntry,
    ) {
        self.handle(Op::Mkdir, req, Some(parent.0), |watch| {
            let engine = Arc::clone(&self.engine);
            let name = name.to_owned();
            let respond = self.entry_reply();
            self.retrying(
                watch,
                move || engine.mkdir(parent.0, name.as_bytes(), mode & !umask),
                self.committed(move |result| respond(result, reply)),
            );
        });
    }

    fn unlink(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        self.handle(Op::Unlink, req, Some(parent.0), |watch| {
            let engine = Arc::clone(&self.engine);
            let name = name.to_owned();
            self.retrying(
                watch,
                move || engine.unlink(parent.0, name.as_bytes()),
                self.committed(reply_empty(reply)),
            );
        });
    }

    fn rmdir(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        self.handle(Op::Rmdir, req, Some(parent.0), |watch| {
            let engine = Arc::clone(&self.engine);
            let name = name.to_owned();
            self.retrying(
                watch,
                move || engine.rmdir(parent.0, name.as_bytes()),
                self.committed(reply_empty(reply)),
            );
        });
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
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        self.handle(Op::Rename, req, Some(parent.0), |watch| {
            if flags.intersects(RenameFlags::RENAME_EXCHANGE | RenameFlags::RENAME_WHITEOUT)
                || !(flags - RenameFlags::RENAME_NOREPLACE).is_empty()
            {
                return reply.error(Errno::EINVAL);
            }
            let mode = if flags.contains(RenameFlags::RENAME_NOREPLACE) {
                RenameMode::NoReplace
            } else {
                RenameMode::Replace
            };
            let engine = Arc::clone(&self.engine);
            let (name, newname) = (name.to_owned(), newname.to_owned());
            self.retrying(
                watch,
                move || {
                    engine.rename(
                        parent.0,
                        name.as_bytes(),
                        newparent.0,
                        newname.as_bytes(),
                        mode,
                    )
                },
                self.committed(reply_empty(reply)),
            );
        });
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

    fn open(&self, req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        self.handle(Op::Open, req, Some(ino.0), |watch| {
            let respond = self.committed(move |result: Result<(u64, bool)>| match result {
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
            respond(self.engine.open_file(ino.0, open_mode(flags.0)), watch);
        });
    }

    fn create(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        self.handle(Op::Create, req, Some(parent.0), |watch| {
            let engine = Arc::clone(&self.engine);
            let name = name.to_owned();
            let exclusive = flags & nix::libc::O_EXCL != 0;
            let open = open_mode(flags);
            let (uid, gid, attr_ttl, entry_ttl) =
                (self.uid, self.gid, self.attr_ttl, self.entry_ttl);
            self.retrying(
                watch,
                move || engine.create(parent.0, name.as_bytes(), mode & !umask, exclusive, open),
                self.committed(move |result| match result {
                    Ok((attr, handle)) => {
                        // `created` takes one TTL for both entry and attributes.
                        let ttl = attr_ttl.min(entry_ttl);
                        reply.created(
                            &ttl,
                            &file_attr(&attr, uid, gid),
                            Generation(0),
                            FileHandle(handle),
                            FopenFlags::empty(),
                        );
                    }
                    Err(error) => reply.error(errno(&error)),
                }),
            );
        });
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
        self.handle(Op::ReadRam, req, Some(ino.0), |mut watch| {
            match self.engine.read_local(fh.0, offset, size) {
                Ok(Some((data, from))) => {
                    watch.set_op(match from {
                        ReadFrom::Ram => Op::ReadRam,
                        ReadFrom::Disk => Op::ReadDisk,
                        ReadFrom::Staging => Op::ReadStaging,
                    });
                    reply.data(&data);
                }
                Ok(None) => {
                    let engine = Arc::clone(&self.engine);
                    watch.set_op(Op::ReadSlow);
                    self.slow(
                        watch,
                        self.block_deadline(),
                        async move { engine.read(fh.0, offset, size).await },
                        move |result, _watch| match result {
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
        req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: fuser::WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        reply: ReplyWrite,
    ) {
        self.handle(Op::Write, req, Some(ino.0), |watch| {
            let respond = self.committed(move |result: Result<u32>| match result {
                Ok(written) => reply.written(written),
                Err(error) => reply.error(errno(&error)),
            });
            match self.engine.write_local(fh.0, offset, data) {
                Err(CoreError::NeedsMaterialize(_)) => {
                    let engine = Arc::clone(&self.engine);
                    let data = data.to_vec();
                    self.slow(
                        watch,
                        self.block_deadline(),
                        async move { engine.write(fh.0, offset, data).await },
                        respond,
                    );
                }
                result => respond(result, watch),
            }
        });
    }

    fn flush(
        &self,
        req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        _lock_owner: fuser::LockOwner,
        reply: ReplyEmpty,
    ) {
        self.handle(Op::Flush, req, Some(ino.0), |watch| {
            match self.engine.flush(fh.0) {
                Ok(()) => self.engine.after_commit(move || {
                    reply.ok();
                    drop(watch);
                }),
                Err(error) => reply.error(errno(&error)),
            }
        });
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
        self.handle(Op::Release, req, Some(ino.0), |watch| {
            self.engine.release_file(fh.0);
            self.engine.after_commit(move || {
                reply.ok();
                drop(watch);
            });
        });
    }

    fn fsync(
        &self,
        req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        self.handle(Op::Fsync, req, Some(ino.0), |watch| {
            let engine = Arc::clone(&self.engine);
            let remote = engine.fsync_is_remote(ino.0);
            let deadline = if remote {
                self.block_deadline()
            } else {
                self.metadata_deadline
            };
            self.slow(
                watch,
                deadline,
                async move {
                    engine.fsync(ino.0).await?;
                    if remote {
                        engine.sync_remote(ino.0, deadline).await?;
                    }
                    Ok(())
                },
                move |result, _watch| reply_empty(reply)(result),
            );
        });
    }

    fn opendir(&self, req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        self.handle(Op::Opendir, req, Some(ino.0), |watch| {
            let engine = Arc::clone(&self.engine);
            self.retrying(
                watch,
                move || engine.opendir(ino.0),
                move |result, _watch| match result {
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
        self.handle(Op::Readdir, req, Some(ino.0), |_watch| {
            match self.engine.dir_snapshot(fh.0) {
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
        self.handle(Op::Readdirplus, req, Some(ino.0), |_watch| {
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
        req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        reply: ReplyEmpty,
    ) {
        self.handle(Op::Releasedir, req, Some(ino.0), |_watch| {
            self.engine.releasedir(fh.0);
            reply.ok();
        });
    }

    fn fsyncdir(
        &self,
        req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        self.handle(Op::Fsyncdir, req, Some(ino.0), |watch| {
            let engine = Arc::clone(&self.engine);
            self.slow(
                watch,
                self.metadata_deadline(),
                async move { engine.fsyncdir().await },
                move |result, _watch| reply_empty(reply)(result),
            );
        });
    }

    fn statfs(&self, req: &Request, ino: INodeNo, reply: ReplyStatfs) {
        self.handle(Op::Statfs, req, Some(ino.0), |_watch| {
            let s = self.engine.statfs();
            reply.statfs(
                s.blocks, s.bfree, s.bavail, s.files, s.ffree, s.bsize, s.namelen, s.bsize,
            );
        });
    }

    fn getxattr(&self, req: &Request, ino: INodeNo, name: &OsStr, size: u32, reply: ReplyXattr) {
        self.handle(Op::Getxattr, req, Some(ino.0), |_watch| {
            let value = name
                .to_str()
                .map_or(Ok(None), |name| self.engine.xattr(ino.0, name));
            match value {
                Ok(Some(value)) => xattr_reply(reply, value.as_bytes(), size),
                Ok(None) => reply.error(Errno::NO_XATTR),
                Err(error) => reply.error(errno(&error)),
            }
        });
    }

    fn listxattr(&self, req: &Request, ino: INodeNo, size: u32, reply: ReplyXattr) {
        self.handle(Op::Listxattr, req, Some(ino.0), |_watch| {
            match self.engine.xattr_names(ino.0) {
                Ok(names) => {
                    let list: Vec<u8> = names
                        .iter()
                        .flat_map(|name| name.bytes().chain(std::iter::once(0)))
                        .collect();
                    xattr_reply(reply, &list, size);
                }
                Err(error) => reply.error(errno(&error)),
            }
        });
    }

    fn access(&self, req: &Request, ino: INodeNo, mask: fuser::AccessFlags, reply: ReplyEmpty) {
        // Without `default_permissions` the daemon checks modes (D031).
        self.handle(Op::Access, req, Some(ino.0), |_watch| {
            reply_empty(reply)(self.engine.access(ino.0, mask.bits() as u32))
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
        reply.error(Errno::ENOTSUP);
    }

    fn removexattr(&self, _req: &Request, _ino: INodeNo, _name: &OsStr, reply: ReplyEmpty) {
        reply.error(Errno::NO_XATTR);
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
        // glibc's `posix_fallocate` falls back to writing (§8.7).
        reply.error(Errno::EOPNOTSUPP);
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
}

/// Mounts `engine` at `mountpoint` and starts the notify task. Blocks until
/// the kernel handshake is done; call from a plain thread or
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
        // No `default_permissions`: the kernel would answer EACCES from the
        // write bits cleared in `ro` subtrees before PdMount could answer
        // EROFS (D019, D031); the engine checks permissions instead.
        MountOption::NoAtime,
        MountOption::NoDev,
        MountOption::NoSuid,
    ];
    if engine.is_read_only() {
        config.mount_options.push(MountOption::RO);
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_fail_fast_unless_the_drive_is_online() {
        let normal = Duration::from_secs(20);
        let offline_read = Duration::from_secs(2);
        assert_eq!(
            deadline_for(Connectivity::Online, normal, offline_read),
            normal
        );
        for state in [Connectivity::Offline, Connectivity::NeedsLogin] {
            assert_eq!(deadline_for(state, normal, offline_read), offline_read);
        }
        // A configured deadline shorter than offline_read is kept.
        assert_eq!(
            deadline_for(Connectivity::Offline, Duration::from_secs(1), offline_read),
            Duration::from_secs(1)
        );
    }
}
