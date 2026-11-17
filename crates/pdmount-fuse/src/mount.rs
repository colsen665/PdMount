//! Mount setup: [`mount`], [`mount_trees`] and [`mount_dynamic`] start the
//! fuser session, and one thread per tree sends its kernel invalidations.

use super::trees::{Slot, TreePlace, TreeSet, Trees};
use super::{FUSER_THREADS, PdFs, Router};
use fuser::{INodeNo, MountOption, Notifier};
use pdmount_core::Config;
use pdmount_core::engine::Engine;
use pdmount_core::errors::Result;
use pdmount_core::poller::VolumeUse;
use pdmount_core::vfs::Invalidation;
use std::ffi::OsStr;
use std::path::Path;
use std::sync::{Arc, OnceLock};
use tokio::sync::mpsc;

/// The source (`fsname`) of this process's mounts once [`set_profile`] named
/// the profile: `pdmount:<profile>`, so the mount table and `df` show whose a
/// mount is and the stale rule can tell profiles apart (D127).
static FS_NAME: OnceLock<String> = OnceLock::new();

/// Names the daemon's profile in the source of every mount it makes from
/// now on (D127). Only the first call counts.
pub fn set_profile(profile: &str) {
    let _ = FS_NAME.set(format!("pdmount:{profile}"));
}

/// The `fsname` mount option: `pdmount:<profile>`, or `pdmount` when no
/// profile was named (tests). The type stays `fuse.pdmount` (`subtype`).
pub(crate) fn fs_name() -> MountOption {
    MountOption::FSName(FS_NAME.get().map_or("pdmount", String::as_str).to_owned())
}

/// One tree of a mount (D080, D110): its engine, the receiver of its
/// kernel invalidations, and, in a mount with several trees, its folder's
/// name in the virtual top level.
pub struct MountTree {
    pub name: String,
    pub engine: Arc<Engine>,
    pub invalidations: mpsc::UnboundedReceiver<Invalidation>,
    /// Tells the pollers that the tree's volume is in use (D107); `None`
    /// for My Files, whose volume always polls at the short interval.
    pub volume_use: Option<VolumeUse>,
    /// Where the tree shows in a mount with a router.
    pub place: TreePlace,
}

/// A running mount.
pub struct Mounted {
    pub(super) session: fuser::BackgroundSession,
    /// The trees of a mount from [`mount_dynamic`].
    trees: Option<TreeSet>,
}

impl Mounted {
    /// A mount of a fixed set of trees, or of a read-only tree.
    pub(super) fn of(session: fuser::BackgroundSession) -> Self {
        Self {
            session,
            trees: None,
        }
    }

    /// The changing set of trees of a mount from [`mount_dynamic`].
    pub fn tree_set(&self) -> Option<TreeSet> {
        self.trees.clone()
    }

    /// Whether the session ended, e.g. after an external `fusermount3 -u`.
    pub fn is_finished(&self) -> bool {
        self.session.guard.is_finished()
    }

    pub fn unmount(self) -> std::io::Result<()> {
        self.session.umount_and_join()
    }
}

/// Mounts `engine` at `mountpoint` and starts the notify task: one tree,
/// with no router (My Files). Blocks until the kernel handshake is done;
/// call from a plain thread or `spawn_blocking`, not from an async task.
pub fn mount(
    engine: Arc<Engine>,
    invalidations: mpsc::UnboundedReceiver<Invalidation>,
    mountpoint: &Path,
) -> std::io::Result<Mounted> {
    mount_trees(
        vec![MountTree {
            name: String::new(),
            engine,
            invalidations,
            volume_use: None,
            place: TreePlace::Folder,
        }],
        mountpoint,
    )
}

/// Mounts `trees` at `mountpoint` (D110): one tree directly, as [`mount`]
/// does; several under a virtual read-only top level with one folder per
/// tree. The mount is read-only for the kernel only when every tree's
/// engine is (`mount -o ro`); a closed write gate answers EROFS itself, so
/// it can open at runtime. Blocks like [`mount`].
pub fn mount_trees(trees: Vec<MountTree>, mountpoint: &Path) -> std::io::Result<Mounted> {
    if trees.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "a mount needs at least one tree",
        ));
    }
    let router = Router::new(trees.len());
    let config = trees[0].engine.config().clone();
    let read_only = trees.iter().all(|tree| tree.engine.is_read_only());
    start(trees, mountpoint, router, &config, read_only, None)
}

/// Mounts `trees` at `mountpoint` under a virtual read-only top level whose
/// trees change while it is mounted ([`Mounted::tree_set`]; a Shared mount,
/// D112): it may start with none. Trees placed as files show in a virtual
/// folder named `files_folder`. `config` gives the kernel's TTLs and the
/// deadlines; `read_only` mounts it read-only for the kernel (`-o ro`).
/// Blocks like [`mount`].
pub fn mount_dynamic(
    trees: Vec<MountTree>,
    mountpoint: &Path,
    config: &Config,
    read_only: bool,
    files_folder: &str,
) -> std::io::Result<Mounted> {
    start(
        trees,
        mountpoint,
        Router::routed(),
        config,
        read_only,
        Some(Arc::from(files_folder)),
    )
}

/// The session of [`mount_trees`] and [`mount_dynamic`]; with
/// `files_folder`, a changing set of trees.
fn start(
    trees: Vec<MountTree>,
    mountpoint: &Path,
    router: Router,
    settings: &Config,
    read_only: bool,
    files_folder: Option<Arc<str>>,
) -> std::io::Result<Mounted> {
    let uid = nix::unistd::getuid().as_raw();
    let gid = nix::unistd::getgid().as_raw();
    let mut config = fuser::Config::default();
    config.mount_options = vec![
        fs_name(),
        MountOption::Subtype("pdmount".into()),
        // No `default_permissions`: the kernel would answer EACCES from the
        // write bits cleared in `ro` subtrees before PdMount could answer
        // EROFS (D019, D031); the engine checks permissions instead.
        MountOption::NoAtime,
        MountOption::NoDev,
        MountOption::NoSuid,
    ];
    if read_only {
        config.mount_options.push(MountOption::RO);
    }
    config.n_threads = Some(FUSER_THREADS);
    config.clone_fd = true;
    let mut receivers = Vec::with_capacity(trees.len());
    let slots = trees
        .into_iter()
        .map(|tree| {
            receivers.push(tree.invalidations);
            Slot {
                name: tree.name,
                engine: tree.engine,
                volume_use: tree.volume_use,
                place: tree.place,
            }
        })
        .collect();
    let trees = Arc::new(parking_lot::RwLock::new(Trees::new(slots)));
    let fs = PdFs::new(
        Arc::clone(&trees),
        router,
        settings,
        files_folder.clone(),
        uid,
        gid,
    );
    let session = fuser::Session::new(fs, mountpoint, &config)?;
    for (index, invalidations) in receivers.into_iter().enumerate() {
        let notifier = session.notifier();
        std::thread::Builder::new()
            .name("pdmount-notify".into())
            .spawn(move || {
                notify_loop(notifier, invalidations, move |invalidation| {
                    router.invalidation(index, invalidation)
                })
            })?;
    }
    let tree_set = match files_folder {
        Some(files_folder) => {
            let (top, receiver) = mpsc::unbounded_channel();
            let notifier = session.notifier();
            std::thread::Builder::new()
                .name("pdmount-notify".into())
                .spawn(move || {
                    notify_loop(notifier, receiver, Ok);
                })?;
            Some(TreeSet {
                trees,
                router,
                notifier: session.notifier(),
                top,
                files_folder,
            })
        }
        None => None,
    };
    Ok(Mounted {
        session: session.spawn()?,
        trees: tree_set,
    })
}

/// Sends a tree's kernel invalidations (§9.5), with `map` giving the
/// kernel's numbers (the router's for a tree), or those of a mount's
/// virtual folders. Runs on its own thread: never on a FUSE thread, never
/// holding engine locks; it ends when the sender (the engine) is gone.
/// ENOENT means the kernel had nothing cached.
pub(super) fn notify_loop(
    notifier: Notifier,
    mut invalidations: mpsc::UnboundedReceiver<Invalidation>,
    map: impl Fn(Invalidation) -> Result<Invalidation>,
) {
    while let Some(invalidation) = invalidations.blocking_recv() {
        let invalidation = match map(invalidation) {
            Ok(invalidation) => invalidation,
            Err(error) => {
                tracing::warn!(%error, "invalidation not sent");
                continue;
            }
        };
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
