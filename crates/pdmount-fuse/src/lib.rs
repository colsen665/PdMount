//! FUSE adapter (DESIGN.md §4): maps fuser callbacks onto [`Engine`].
//!
//! A mount serves one tree or several (D080, D110). With several, a
//! [`Router`] gives the mount a virtual read-only top level with one folder
//! per tree and maps inode numbers to their tree; a rename between two trees
//! is EXDEV. With one tree nothing is mapped, so My Files keeps its paths and
//! inode numbers. A mount from [`mount_dynamic`] (Shared, D112) always has a
//! router, and its trees come and go while it is mounted ([`TreeSet`]); trees
//! whose root is a single file show in a second virtual folder, the files
//! folder.
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
//!
//! # Module map
//!
//! - `lib.rs` (here): `PdFs`, the `Filesystem` implementation (the callbacks,
//!   their hot and slow paths and deadlines).
//! - `router.rs`: [`Router`] and [`Route`], the inode numbers of a mount with
//!   several trees, with their tests.
//! - `trees.rs`: the trees of a mount by slot, and [`TreeSet`], the handle
//!   that adds, removes and renames them while mounted.
//! - `mount.rs`: [`MountTree`], [`Mounted`], [`mount`], [`mount_trees`] and
//!   [`mount_dynamic`] (session setup) and the notify threads (kernel
//!   invalidations).
//! - `convert.rs`: conversions between the engine's types and the kernel's
//!   (`file_attr`, `errno`, `open_mode`, ...) and small reply helpers.
//! - `readonly.rs`: [`RoFs`] and [`mount_read_only`], a mount over a
//!   read-only tree that is not an engine (Photos, D113).

mod convert;
mod mount;
mod readonly;
mod router;
mod trees;

pub use mount::{MountTree, Mounted, mount, mount_dynamic, mount_trees, set_profile};
pub use readonly::{RoFs, mount_read_only};
pub use router::{Route, Router};
pub use trees::{MAX_TREES, TreePlace, TreeSet};

use convert::{
    errno, file_attr, fill_dir, open_mode, reply_empty, time, top_read_only, xattr_reply,
};
use router::{FILES_INO, TOP_INO};
use trees::Trees;

use fuser::{
    Errno, FileAttr, FileHandle, Filesystem, FopenFlags, Generation, INodeNo, InitFlags,
    KernelConfig, OpenFlags, RenameFlags, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory,
    ReplyDirectoryPlus, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, ReplyXattr,
    Request, TimeOrNow,
};
use pdmount_backend::NodeKind;
use pdmount_core::cache::ReadFrom;
use pdmount_core::engine::{Engine, MAY_WRITE, SetAttr};
use pdmount_core::errors::{CoreError, Result};
use pdmount_core::health::Connectivity;
use pdmount_core::metrics::{self, Metrics, Op, Stopwatch};
use pdmount_core::vfs::{Attr, DirEntry, Ino, Lookup, ROOT_INO, RenameMode};
use std::ffi::OsStr;
use std::future::Future;
use std::os::unix::ffi::OsStrExt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::Semaphore;
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
    trees: Arc<parking_lot::RwLock<Trees>>,
    router: Router,
    /// The files folder's name; `None`: a fixed set of trees without one.
    files_folder: Option<Arc<str>>,
    /// Counts the requests of a mount that holds no tree.
    metrics: Arc<Metrics>,
    slow: Arc<Semaphore>,
    uid: u32,
    gid: u32,
    attr_ttl: Duration,
    entry_ttl: Duration,
    negative_ttl: Duration,
    metadata_deadline: Duration,
    block_deadline: Duration,
}

/// What a request's inode leads to.
enum Target {
    /// A virtual folder: the top level, or the files folder.
    Top,
    Tree {
        index: usize,
        ino: Ino,
        engine: Arc<Engine>,
    },
}

/// An entry of a virtual folder, with what readdirplus needs.
struct VirtualEntry {
    entry: DirEntry,
    /// The kernel's attributes; `None` when they are not known.
    attr: Option<Attr>,
    /// The tree's engine and its inode, which counts a kernel reference
    /// when the entry goes out in readdirplus; `None` for virtual entries.
    referenced: Option<(Arc<Engine>, Ino)>,
}

impl PdFs {
    fn new(
        trees: Arc<parking_lot::RwLock<Trees>>,
        router: Router,
        config: &pdmount_core::Config,
        files_folder: Option<Arc<str>>,
        uid: u32,
        gid: u32,
    ) -> Self {
        Self {
            attr_ttl: config.kernel.attr_ttl.0,
            entry_ttl: config.kernel.entry_ttl.0,
            negative_ttl: config.kernel.negative_ttl.0,
            metadata_deadline: config.timeouts.metadata.0,
            block_deadline: config.timeouts.block.0,
            router,
            trees,
            files_folder,
            metrics: Arc::new(Metrics::default()),
            slow: Arc::new(Semaphore::new(SLOW_PATH_LIMIT)),
            uid,
            gid,
        }
    }

    /// The tree behind `ino`; `None` for a number no tree has (any more).
    fn target(&self, ino: u64) -> Option<Target> {
        match self.router.route(ino) {
            Route::Top => Some(Target::Top),
            Route::Files => self.has_files().then_some(Target::Top),
            Route::Tree(index, ino) => {
                let trees = self.trees.read();
                let slot = trees.get(index)?;
                Some(Target::Tree {
                    index,
                    ino,
                    engine: Arc::clone(&slot.engine),
                })
            }
        }
    }

    /// The answer of a request on an inode that leads nowhere (a tree that
    /// left the mount, a revoked share, D112, D114): ENOENT for a request on
    /// the name (a lookup in it, `stat`, `open`, `access`), as for a deleted
    /// file; EIO for one on an open handle (`read`, `write`, `fstat`,
    /// `fsync`, `readdir`, `close`), whose content can no longer be kept.
    /// Releasing a handle always succeeds.
    fn gone(handle: bool) -> Errno {
        if handle { Errno::EIO } else { Errno::ENOENT }
    }

    /// Whether the mount shows the files folder now.
    fn has_files(&self) -> bool {
        self.files_folder.is_some() && self.trees.read().has_files()
    }

    /// Marks tree `index`'s volume as in use (D107).
    fn touch(&self, index: usize) {
        if let Some(volume_use) = self
            .trees
            .read()
            .get(index)
            .and_then(|slot| slot.volume_use.as_ref())
        {
            volume_use.touch();
        }
    }

    /// The engine a request on `ino` counts in: its tree's, or the first
    /// tree's for a virtual folder; `None` in a mount without trees.
    fn engine_for(&self, ino: u64) -> Option<Arc<Engine>> {
        match self.target(ino) {
            Some(Target::Tree { engine, .. }) => Some(engine),
            _ => self.trees.read().first().cloned(),
        }
    }

    /// Deadline of a metadata request: short while the drive cannot be
    /// reached, so uncached listings fail with EIO instead of hanging (§9.6).
    fn metadata_deadline(&self, engine: &Engine) -> Duration {
        self.deadline(engine, self.metadata_deadline)
    }

    /// Deadline of a request that may fetch blocks.
    fn block_deadline(&self, engine: &Engine) -> Duration {
        self.deadline(engine, self.block_deadline)
    }

    fn deadline(&self, engine: &Engine, normal: Duration) -> Duration {
        deadline_for(
            engine.health().state(),
            normal,
            engine.config().timeouts.offline_read.0,
        )
    }

    /// Runs a callback body as a hot-path handler, catching panics. The
    /// body gets the request's stopwatch and drops it after the reply.
    fn handle(&self, op: Op, req: &Request, ino: Option<u64>, body: impl FnOnce(Stopwatch)) {
        let metrics = self
            .engine_for(ino.unwrap_or(TOP_INO))
            .map_or_else(|| Arc::clone(&self.metrics), |e| Arc::clone(e.metrics()));
        let watch = Stopwatch::new(Arc::clone(&metrics), op, req.unique().into(), ino);
        if catch_unwind(AssertUnwindSafe(|| metrics::hot(|| body(watch)))).is_err() {
            Metrics::inc(&metrics.handler_panics);
            tracing::error!(op = op.as_str(), "FUSE handler panicked; answered EIO");
        }
    }

    /// Moves the rest of a request to a tokio task with a deadline.
    fn slow<T, F>(
        &self,
        engine: &Engine,
        mut watch: Stopwatch,
        deadline: Duration,
        work: F,
        reply: impl FnOnce(Result<T>, Stopwatch) + Send + 'static,
    ) where
        T: Send + 'static,
        F: Future<Output = Result<T>> + Send + 'static,
    {
        let permits = Arc::clone(&self.slow);
        let metrics = Arc::clone(engine.metrics());
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
        engine.runtime().spawn(task.instrument(span));
    }

    /// Runs `attempt` on the hot path; if it needs a listing or inode
    /// numbers, provides them on the slow path and tries again.
    fn retrying<T, A>(
        &self,
        engine: &Arc<Engine>,
        watch: Stopwatch,
        attempt: A,
        reply: impl FnOnce(Result<T>, Stopwatch) + Send + 'static,
    ) where
        T: Send + 'static,
        A: Fn() -> Result<T> + Send + 'static,
    {
        match attempt() {
            Err(error) if is_resolvable(&error) => {
                let resolver = Arc::clone(engine);
                self.slow(
                    engine,
                    watch,
                    self.metadata_deadline(engine),
                    async move {
                        let mut error = error;
                        for _ in 0..SLOW_ATTEMPTS {
                            resolve(&resolver, &error).await?;
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

    /// The attributes of a virtual folder (the top level, or the files
    /// folder): read-only, its times those of the last change of the trees.
    fn virtual_attr(&self, ino: u64) -> Attr {
        let trees = self.trees.read();
        let folders = trees.placed(false).count() as u32;
        let files = self.files_folder.is_some() && trees.has_files();
        Attr {
            ino,
            kind: NodeKind::Folder,
            size: 0,
            mtime: trees.changed,
            ctime: trees.changed,
            atime: trees.changed,
            perm: 0o555,
            nlink: if ino == FILES_INO {
                2
            } else {
                2 + folders + u32::from(files)
            },
        }
    }

    fn top_attr(&self) -> Attr {
        self.virtual_attr(TOP_INO)
    }

    /// The entries of a virtual folder, `.` and `..` first. The top level:
    /// one folder per tree placed as a folder, and the files folder while
    /// a tree holds a file. The files folder: each such tree's file, under
    /// the tree's name.
    fn virtual_entries(&self, dir: u64) -> Vec<VirtualEntry> {
        let dot = |name: &str, ino| VirtualEntry {
            entry: DirEntry {
                ino,
                kind: NodeKind::Folder,
                name: name.into(),
            },
            attr: Some(self.virtual_attr(ino)),
            referenced: None,
        };
        let mut entries = vec![dot(".", dir), dot("..", TOP_INO)];
        let files_attr = self.virtual_attr(FILES_INO);
        let trees = self.trees.read();
        if dir == FILES_INO {
            for (index, slot) in trees.placed(true) {
                let TreePlace::File(child) = &slot.place else {
                    continue;
                };
                // The tree's root is listed when it opens; one that is not
                // is left out until it is.
                let Ok(Some(attr)) = slot.engine.peek_child(ROOT_INO, child) else {
                    continue;
                };
                let local = attr.ino;
                let Ok(attr) = self.router.attr(index, attr) else {
                    continue;
                };
                entries.push(VirtualEntry {
                    entry: DirEntry {
                        ino: attr.ino,
                        kind: NodeKind::File,
                        name: slot.name.clone(),
                    },
                    attr: Some(attr),
                    referenced: Some((Arc::clone(&slot.engine), local)),
                });
            }
            return entries;
        }
        for (index, slot) in trees.placed(false) {
            let Ok(ino) = self.router.global(index, ROOT_INO) else {
                continue;
            };
            let attr = slot
                .engine
                .getattr(ROOT_INO)
                .and_then(|attr| self.router.attr(index, attr))
                .ok();
            entries.push(VirtualEntry {
                entry: DirEntry {
                    ino,
                    kind: NodeKind::Folder,
                    name: slot.name.clone(),
                },
                attr,
                referenced: Some((Arc::clone(&slot.engine), ROOT_INO)),
            });
        }
        if let Some(files) = &self.files_folder
            && trees.has_files()
        {
            entries.push(VirtualEntry {
                entry: DirEntry {
                    ino: FILES_INO,
                    kind: NodeKind::Folder,
                    name: files.to_string(),
                },
                attr: Some(files_attr),
                referenced: None,
            });
        }
        entries
    }

    /// Sends a successful reply only once the store has committed the rows
    /// the request queued (§5.6); errors go out at once. The stopwatch
    /// stops after the reply.
    fn committed<T: Send + 'static>(
        &self,
        engine: &Arc<Engine>,
        reply: impl FnOnce(Result<T>) + Send + 'static,
    ) -> impl FnOnce(Result<T>, Stopwatch) + Send + 'static {
        let engine = Arc::clone(engine);
        move |result, watch| match result {
            Ok(value) => engine.after_commit_kept(move |kept| {
                reply(kept.map(|()| value));
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

    /// Looks up a tree's folder, or the files folder, in the top level; a
    /// tree's folder counts as a kernel reference of the tree's root.
    fn top_lookup(&self, name: &OsStr) -> Result<Lookup> {
        if self
            .files_folder
            .as_deref()
            .is_some_and(|files| files.as_bytes() == name.as_bytes())
            && self.has_files()
        {
            return Ok(Lookup::Found(self.virtual_attr(FILES_INO)));
        }
        let found = {
            let trees = self.trees.read();
            trees
                .find(false, name.as_bytes())
                .and_then(|index| Some((index, Arc::clone(&trees.get(index)?.engine))))
        };
        let Some((index, engine)) = found else {
            return Ok(Lookup::Negative);
        };
        self.touch(index);
        let attr = engine
            .readdirplus_entry(ROOT_INO)
            .ok_or(CoreError::NotFound)?;
        self.router.lookup(index, Lookup::Found(attr))
    }

    /// Looks up a file in the files folder: the one file of the tree of
    /// that name.
    fn files_lookup(&self, name: &OsStr, watch: Stopwatch, reply: ReplyEntry) {
        let respond = self.lookup_reply();
        let found = {
            let trees = self.trees.read();
            trees.find(true, name.as_bytes()).and_then(|index| {
                let slot = trees.get(index)?;
                let TreePlace::File(child) = &slot.place else {
                    return None;
                };
                Some((index, Arc::clone(&slot.engine), child.clone()))
            })
        };
        let Some((index, engine, child)) = found else {
            return respond(Ok(Lookup::Negative), reply);
        };
        self.touch(index);
        let router = self.router;
        let looked_up = Arc::clone(&engine);
        self.retrying(
            &engine,
            watch,
            move || {
                looked_up
                    .lookup(ROOT_INO, child.as_bytes())
                    .and_then(|lookup| router.lookup(index, lookup))
            },
            move |result, _watch| respond(result, reply),
        );
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
            let respond = self.lookup_reply();
            match self.target(parent.0) {
                None => reply.error(Self::gone(false)),
                Some(Target::Top) if parent.0 == FILES_INO => self.files_lookup(name, watch, reply),
                Some(Target::Top) => respond(self.top_lookup(name), reply),
                Some(Target::Tree { index, ino, engine }) => {
                    self.touch(index);
                    let router = self.router;
                    let looked_up = Arc::clone(&engine);
                    let name = name.to_owned();
                    self.retrying(
                        &engine,
                        watch,
                        move || {
                            looked_up
                                .lookup(ino, name.as_bytes())
                                .and_then(|lookup| router.lookup(index, lookup))
                        },
                        move |result, _watch| respond(result, reply),
                    );
                }
            }
        });
    }

    fn forget(&self, req: &Request, ino: INodeNo, nlookup: u64) {
        self.handle(Op::Forget, req, Some(ino.0), |_watch| {
            if let Some(Target::Tree { ino, engine, .. }) = self.target(ino.0) {
                engine.forget(ino, nlookup);
            }
        });
    }

    fn getattr(&self, req: &Request, ino: INodeNo, fh: Option<FileHandle>, reply: ReplyAttr) {
        self.handle(Op::Getattr, req, Some(ino.0), |_watch| {
            let attr = match self.target(ino.0) {
                None => return reply.error(Self::gone(fh.is_some())),
                Some(Target::Top) => Ok(self.virtual_attr(ino.0)),
                Some(Target::Tree { index, ino, engine }) => engine
                    .getattr(ino)
                    .and_then(|attr| self.router.attr(index, attr)),
            };
            match attr {
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
            let (index, ino, engine) = match self.target(ino.0) {
                None => return reply.error(Self::gone(fh.is_some())),
                Some(Target::Top) => return reply.error(top_read_only()),
                Some(Target::Tree { index, ino, engine }) => (index, ino, engine),
            };
            let engine = &engine;
            let change = SetAttr {
                mode,
                size,
                mtime: mtime.map(time),
                atime: atime.map(time),
                handle: fh.map(|fh| fh.0),
            };
            let (uid, gid, ttl, router) = (self.uid, self.gid, self.attr_ttl, self.router);
            let respond = self.committed(engine, move |result: Result<Attr>| {
                match result.and_then(|attr| router.attr(index, attr)) {
                    Ok(attr) => reply.attr(&ttl, &file_attr(&attr, uid, gid)),
                    Err(error) => reply.error(errno(&error)),
                }
            });
            match engine.setattr(ino, &change) {
                Err(CoreError::NeedsMaterialize(_)) => {
                    let slow = Arc::clone(engine);
                    self.slow(
                        engine,
                        watch,
                        self.block_deadline(engine),
                        async move { slow.setattr_slow(ino, change).await },
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
            let (index, parent, engine) = match self.target(parent.0) {
                None => return reply.error(Errno::ENOENT),
                Some(Target::Top) => return reply.error(top_read_only()),
                Some(Target::Tree { index, ino, engine }) => (index, ino, engine),
            };
            let engine = &engine;
            let made = Arc::clone(engine);
            let name = name.to_owned();
            let respond = self.entry_reply();
            let router = self.router;
            self.retrying(
                engine,
                watch,
                move || {
                    made.mkdir(parent, name.as_bytes(), mode & !umask)
                        .and_then(|attr| router.attr(index, attr))
                },
                self.committed(engine, move |result| respond(result, reply)),
            );
        });
    }

    fn unlink(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        self.handle(Op::Unlink, req, Some(parent.0), |watch| {
            let (parent, engine) = match self.target(parent.0) {
                None => return reply.error(Errno::ENOENT),
                Some(Target::Top) => return reply.error(top_read_only()),
                Some(Target::Tree { ino, engine, .. }) => (ino, engine),
            };
            let engine = &engine;
            let removed = Arc::clone(engine);
            let name = name.to_owned();
            self.retrying(
                engine,
                watch,
                move || removed.unlink(parent, name.as_bytes()),
                self.committed(engine, reply_empty(reply)),
            );
        });
    }

    fn rmdir(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        self.handle(Op::Rmdir, req, Some(parent.0), |watch| {
            let (parent, engine) = match self.target(parent.0) {
                None => return reply.error(Errno::ENOENT),
                Some(Target::Top) => return reply.error(top_read_only()),
                Some(Target::Tree { ino, engine, .. }) => (ino, engine),
            };
            let engine = &engine;
            let removed = Arc::clone(engine);
            let name = name.to_owned();
            self.retrying(
                engine,
                watch,
                move || removed.rmdir(parent, name.as_bytes()),
                self.committed(engine, reply_empty(reply)),
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
            let (from, to) = match (self.target(parent.0), self.target(newparent.0)) {
                (None, _) | (_, None) => return reply.error(Errno::ENOENT),
                (Some(Target::Top), _) | (_, Some(Target::Top)) => {
                    return reply.error(top_read_only());
                }
                (Some(from), Some(to)) => (from, to),
            };
            let (
                Target::Tree {
                    index,
                    ino: parent,
                    engine,
                },
                Target::Tree {
                    index: new_index,
                    ino: newparent,
                    ..
                },
            ) = (from, to)
            else {
                return reply.error(Errno::EIO);
            };
            // Every tree is a file system of its own: `mv`, `rsync` and
            // `git` copy instead (D080).
            if index != new_index {
                return reply.error(Errno::EXDEV);
            }
            let engine = &engine;
            let renamed = Arc::clone(engine);
            let (name, newname) = (name.to_owned(), newname.to_owned());
            self.retrying(
                engine,
                watch,
                move || {
                    renamed.rename(parent, name.as_bytes(), newparent, newname.as_bytes(), mode)
                },
                self.committed(engine, reply_empty(reply)),
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
            let (index, ino, engine) = match self.target(ino.0) {
                None => return reply.error(Self::gone(false)),
                Some(Target::Top) => return reply.error(Errno::EISDIR),
                Some(Target::Tree { index, ino, engine }) => (index, ino, engine),
            };
            let engine = &engine;
            self.touch(index);
            let respond = self.committed(engine, move |result: Result<(u64, bool)>| match result {
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
            respond(engine.open_file(ino, open_mode(flags.0)), watch);
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
            let (index, parent, engine) = match self.target(parent.0) {
                None => return reply.error(Errno::ENOENT),
                Some(Target::Top) => return reply.error(top_read_only()),
                Some(Target::Tree { index, ino, engine }) => (index, ino, engine),
            };
            let engine = &engine;
            let created = Arc::clone(engine);
            let name = name.to_owned();
            let exclusive = flags & nix::libc::O_EXCL != 0;
            let open = open_mode(flags);
            let (uid, gid, attr_ttl, entry_ttl, router) = (
                self.uid,
                self.gid,
                self.attr_ttl,
                self.entry_ttl,
                self.router,
            );
            self.retrying(
                engine,
                watch,
                move || {
                    let (attr, handle) =
                        created.create(parent, name.as_bytes(), mode & !umask, exclusive, open)?;
                    Ok((router.attr(index, attr)?, handle))
                },
                self.committed(engine, move |result| match result {
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
            // A tree removed from its mount (a revoked share, D112): its
            // open files answer EIO.
            let Some(Target::Tree { engine, .. }) = self.target(ino.0) else {
                return reply.error(Self::gone(true));
            };
            let engine = &engine;
            match engine.read_local(fh.0, offset, size) {
                Ok(Some((data, from))) => {
                    watch.set_op(match from {
                        ReadFrom::Ram => Op::ReadRam,
                        ReadFrom::Disk => Op::ReadDisk,
                        ReadFrom::Staging => Op::ReadStaging,
                    });
                    reply.data(&data);
                }
                Ok(None) => {
                    let reader = Arc::clone(engine);
                    watch.set_op(Op::ReadSlow);
                    self.slow(
                        engine,
                        watch,
                        self.block_deadline(engine),
                        async move { reader.read(fh.0, offset, size).await },
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
            let Some(Target::Tree { engine, .. }) = self.target(ino.0) else {
                return reply.error(Self::gone(true));
            };
            let engine = &engine;
            let respond = self.committed(engine, move |result: Result<u32>| match result {
                Ok(written) => reply.written(written),
                Err(error) => reply.error(errno(&error)),
            });
            match engine.write_local(fh.0, offset, data) {
                Err(CoreError::NeedsMaterialize(_)) => {
                    let writer = Arc::clone(engine);
                    let data = data.to_vec();
                    self.slow(
                        engine,
                        watch,
                        self.block_deadline(engine),
                        async move { writer.write(fh.0, offset, data).await },
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
            let Some(Target::Tree { engine, .. }) = self.target(ino.0) else {
                return reply.error(Self::gone(true));
            };
            // The close of a writer answers for its writes: EIO once the
            // tree was retired before they committed (D114).
            let writes = engine.handle_writes(fh.0);
            match engine.flush(fh.0) {
                Ok(()) if writes => engine.after_commit_kept(move |kept| {
                    reply_empty(reply)(kept);
                    drop(watch);
                }),
                Ok(()) => engine.after_commit(move || {
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
            let Some(Target::Tree { engine, .. }) = self.target(ino.0) else {
                return reply.ok();
            };
            engine.release_file(fh.0);
            engine.after_commit(move || {
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
            let (ino, engine) = match self.target(ino.0) {
                None => return reply.error(Self::gone(true)),
                Some(Target::Top) => return reply.ok(),
                Some(Target::Tree { ino, engine, .. }) => (ino, engine),
            };
            let engine = &engine;
            let synced = Arc::clone(engine);
            let remote = engine.fsync_is_remote(ino);
            let deadline = if remote {
                self.block_deadline(engine)
            } else {
                self.metadata_deadline
            };
            self.slow(
                engine,
                watch,
                deadline,
                async move {
                    synced.fsync(ino).await?;
                    if remote {
                        synced.sync_remote(ino, deadline).await?;
                    }
                    Ok(())
                },
                move |result, _watch| reply_empty(reply)(result),
            );
        });
    }

    fn opendir(&self, req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        self.handle(Op::Opendir, req, Some(ino.0), |watch| {
            let (index, ino, engine) = match self.target(ino.0) {
                None => return reply.error(Self::gone(false)),
                // The top level has no handle: its entries are the trees.
                Some(Target::Top) => return reply.opened(FileHandle(0), FopenFlags::empty()),
                Some(Target::Tree { index, ino, engine }) => (index, ino, engine),
            };
            let engine = &engine;
            self.touch(index);
            let opened = Arc::clone(engine);
            self.retrying(
                engine,
                watch,
                move || opened.opendir(ino),
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
            let entries = match self.target(ino.0) {
                None => Err(CoreError::Errno(nix::libc::EIO)),
                Some(Target::Top) => Ok(self
                    .virtual_entries(ino.0)
                    .into_iter()
                    .map(|entry| entry.entry)
                    .collect::<Vec<_>>()),
                Some(Target::Tree { index, ino, engine }) => {
                    engine.dir_snapshot(fh.0).and_then(|entries| {
                        entries
                            .iter()
                            .map(|entry| self.router.entry(index, ino, entry))
                            .collect()
                    })
                }
            };
            match entries {
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
            match self.target(ino.0) {
                None => reply.error(Self::gone(true)),
                Some(Target::Top) => {
                    let entries = self.virtual_entries(ino.0);
                    for (index, virt) in entries.iter().enumerate().skip(offset as usize) {
                        let next = index as u64 + 1;
                        let entry = &virt.entry;
                        // No lookup reference for `.` and `..`.
                        let ttl = if entry.name == "." || entry.name == ".." {
                            Duration::ZERO
                        } else {
                            self.entry_ttl
                        };
                        let Some(attr) = &virt.attr else {
                            continue;
                        };
                        if reply.add(
                            INodeNo(entry.ino),
                            next,
                            &entry.name,
                            &ttl,
                            &self.file_attr(attr),
                            Generation(0),
                        ) {
                            break;
                        }
                        // Only entries that fit count as kernel references.
                        if let Some((engine, local)) = &virt.referenced {
                            engine.readdirplus_entry(*local);
                        }
                    }
                    reply.ok();
                }
                Some(Target::Tree {
                    index: tree,
                    ino: dir,
                    engine,
                }) => {
                    let entries = match engine.dir_snapshot(fh.0) {
                        Ok(entries) => entries,
                        Err(error) => return reply.error(errno(&error)),
                    };
                    for (index, entry) in entries.iter().enumerate().skip(offset as usize) {
                        let next = index as u64 + 1;
                        let Ok(out) = self.router.entry(tree, dir, entry) else {
                            continue;
                        };
                        if entry.name == "." || entry.name == ".." {
                            // No lookup reference for these; the kernel ignores the attributes.
                            let attr = if out.ino == TOP_INO {
                                Ok(self.top_attr())
                            } else {
                                engine
                                    .getattr(entry.ino)
                                    .and_then(|attr| self.router.attr(tree, attr))
                            };
                            let Ok(attr) = attr else {
                                continue;
                            };
                            let full = reply.add(
                                INodeNo(out.ino),
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
                        let Ok(attr) = engine
                            .getattr(entry.ino)
                            .and_then(|attr| self.router.attr(tree, attr))
                        else {
                            continue;
                        };
                        if reply.add(
                            INodeNo(out.ino),
                            next,
                            &entry.name,
                            &self.entry_ttl,
                            &self.file_attr(&attr),
                            Generation(0),
                        ) {
                            break;
                        }
                        // Only entries that fit count as kernel references.
                        engine.readdirplus_entry(entry.ino);
                    }
                    reply.ok();
                }
            }
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
            if let Some(Target::Tree { engine, .. }) = self.target(ino.0) {
                engine.releasedir(fh.0);
            }
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
            let engine = match self.target(ino.0) {
                None => return reply.error(Self::gone(true)),
                Some(Target::Top) => return reply.ok(),
                Some(Target::Tree { engine, .. }) => engine,
            };
            let engine = &engine;
            let synced = Arc::clone(engine);
            self.slow(
                engine,
                watch,
                self.metadata_deadline(engine),
                async move { synced.fsyncdir().await },
                move |result, _watch| reply_empty(reply)(result),
            );
        });
    }

    fn statfs(&self, req: &Request, ino: INodeNo, reply: ReplyStatfs) {
        self.handle(Op::Statfs, req, Some(ino.0), |_watch| {
            // The user's quota, from the tree asked or the first one; a
            // mount without trees has no figures.
            let Some(engine) = self.engine_for(ino.0) else {
                let name_max = pdmount_core::vfs::MAX_NAME_BYTES as u32;
                return reply.statfs(0, 0, 0, 0, 0, BLKSIZE, name_max, BLKSIZE);
            };
            let s = engine.statfs();
            reply.statfs(
                s.blocks, s.bfree, s.bavail, s.files, s.ffree, s.bsize, s.namelen, s.bsize,
            );
        });
    }

    fn getxattr(&self, req: &Request, ino: INodeNo, name: &OsStr, size: u32, reply: ReplyXattr) {
        self.handle(Op::Getxattr, req, Some(ino.0), |_watch| {
            let value = match self.target(ino.0) {
                None => Err(CoreError::NotFound),
                Some(Target::Top) => Ok(None),
                Some(Target::Tree { ino, engine, .. }) => name
                    .to_str()
                    .map_or(Ok(None), |name| engine.xattr(ino, name)),
            };
            match value {
                Ok(Some(value)) => xattr_reply(reply, value.as_bytes(), size),
                Ok(None) => reply.error(Errno::NO_XATTR),
                Err(error) => reply.error(errno(&error)),
            }
        });
    }

    fn listxattr(&self, req: &Request, ino: INodeNo, size: u32, reply: ReplyXattr) {
        self.handle(Op::Listxattr, req, Some(ino.0), |_watch| {
            let names = match self.target(ino.0) {
                None => Err(CoreError::NotFound),
                Some(Target::Top) => Ok(Vec::new()),
                Some(Target::Tree { ino, engine, .. }) => engine.xattr_names(ino),
            };
            match names {
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
            let mask = mask.bits() as u32;
            let result = match self.target(ino.0) {
                None => Err(CoreError::NotFound),
                Some(Target::Top) if mask & MAY_WRITE != 0 => Err(CoreError::ReadOnly),
                Some(Target::Top) => Ok(()),
                Some(Target::Tree { ino, engine, .. }) => engine.access(ino, mask),
            };
            reply_empty(reply)(result)
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
