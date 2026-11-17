//! The trees of a mount (D110), and the set that changes while it is
//! mounted (D112).
//!
//! A tree's number is its slot: it fixes the tree's inode numbers
//! ([`Router::global`]) for the life of the mount. A slot is never reused,
//! not even after its tree was removed, because the kernel may still hold
//! inodes of the removed tree (open files, cached entries): those then lead
//! nowhere (a read of an open file answers EIO) instead of into another
//! tree. A mount has room for [`MAX_TREES`] trees over its life.
//!
//! In a mount with a router, a tree shows as a folder of the top level
//! ([`TreePlace::Folder`]), or, when its root holds a single file
//! ([`TreePlace::File`]), that file shows in the files folder, a second
//! virtual folder in the top level (shared single files, M5.12).

use super::mount::{MountTree, notify_loop};
use super::router::{FILES_INO, Router, TOP_INO, TREE_SHIFT};
use fuser::Notifier;
use pdmount_core::engine::Engine;
use pdmount_core::poller::VolumeUse;
use pdmount_core::vfs::Invalidation;
use std::sync::Arc;
use std::time::SystemTime;
use tokio::sync::mpsc;

/// Trees one mount can hold over its life (slot numbers are never reused).
pub const MAX_TREES: usize = (1 << (64 - TREE_SHIFT)) - 2;

/// Where a tree shows in a mount with a router.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreePlace {
    /// Its root folder is a folder of the top level, named after the tree.
    Folder,
    /// Its root folder holds one file, named `.0` in the tree; that file
    /// shows in the files folder under the tree's name.
    File(String),
}

/// A tree as the adapter serves it.
pub(super) struct Slot {
    pub name: String,
    pub engine: Arc<Engine>,
    pub volume_use: Option<VolumeUse>,
    pub place: TreePlace,
}

impl Slot {
    pub fn is_file(&self) -> bool {
        matches!(self.place, TreePlace::File(_))
    }
}

/// The trees of a mount, by slot.
pub(super) struct Trees {
    slots: Vec<Option<Slot>>,
    /// When the virtual folders' entries last changed: their times.
    pub changed: SystemTime,
}

impl Trees {
    pub fn new(slots: Vec<Slot>) -> Self {
        Self {
            slots: slots.into_iter().map(Some).collect(),
            changed: SystemTime::now(),
        }
    }

    pub fn get(&self, index: usize) -> Option<&Slot> {
        self.slots.get(index)?.as_ref()
    }

    /// The trees present, with their slots.
    pub fn iter(&self) -> impl Iterator<Item = (usize, &Slot)> {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| Some((index, slot.as_ref()?)))
    }

    /// The trees in the top level (`files`: false) or the files folder.
    pub fn placed(&self, files: bool) -> impl Iterator<Item = (usize, &Slot)> {
        self.iter().filter(move |(_, slot)| slot.is_file() == files)
    }

    /// Whether slot `index` held a tree that was removed.
    #[cfg(test)]
    pub fn removed(&self, index: usize) -> bool {
        matches!(self.slots.get(index), Some(None))
    }

    pub fn has_files(&self) -> bool {
        self.placed(true).next().is_some()
    }

    /// The first tree named `name` in the top level or the files folder.
    pub fn find(&self, files: bool, name: &[u8]) -> Option<usize> {
        self.placed(files)
            .find(|(_, slot)| slot.name.as_bytes() == name)
            .map(|(index, _)| index)
    }

    /// The first tree's engine: the defaults of requests outside a tree.
    pub fn first(&self) -> Option<&Arc<Engine>> {
        self.iter().next().map(|(_, slot)| &slot.engine)
    }

    /// Adds a tree in a new slot; `None` when the mount has no slot left.
    pub fn add(&mut self, slot: Slot) -> Option<usize> {
        if self.slots.len() >= MAX_TREES {
            return None;
        }
        self.slots.push(Some(slot));
        self.changed = SystemTime::now();
        Some(self.slots.len() - 1)
    }

    /// Removes a tree; its slot stays taken.
    pub fn remove(&mut self, index: usize) -> Option<Slot> {
        let slot = self.slots.get_mut(index)?.take()?;
        self.changed = SystemTime::now();
        Some(slot)
    }

    /// Renames a tree; returns its old name.
    pub fn rename(&mut self, index: usize, name: String) -> Option<String> {
        let slot = self.slots.get_mut(index)?.as_mut()?;
        if slot.name == name {
            return None;
        }
        self.changed = SystemTime::now();
        Some(std::mem::replace(&mut slot.name, name))
    }
}

/// The handle of a mount whose trees change while it is mounted (a Shared
/// mount, D112): add, remove and rename trees. Each change invalidates the
/// kernel's entries of the top level or the files folder, so a new tree
/// shows at once (also where a lookup had cached its absence), and a
/// removed one is gone.
#[derive(Clone)]
pub struct TreeSet {
    pub(super) trees: Arc<parking_lot::RwLock<Trees>>,
    pub(super) router: Router,
    pub(super) notifier: Notifier,
    /// Invalidations of the virtual folders, sent by their own thread.
    pub(super) top: mpsc::UnboundedSender<Invalidation>,
    pub(super) files_folder: Arc<str>,
}

impl TreeSet {
    /// Adds `tree`; returns its slot. Starts the tree's notify thread
    /// first, so a tree whose thread cannot start never shows.
    pub fn add(&self, tree: MountTree) -> std::io::Result<usize> {
        let MountTree {
            name,
            engine,
            invalidations,
            volume_use,
            place,
        } = tree;
        let file = matches!(place, TreePlace::File(_));
        // The thread learns its slot once the tree has one; the tree's
        // invalidations wait in their channel until then.
        let (slot_tx, slot_rx) = std::sync::mpsc::sync_channel::<usize>(1);
        let notifier = self.notifier.clone();
        let router = self.router;
        std::thread::Builder::new()
            .name("pdmount-notify".into())
            .spawn(move || {
                // No slot: the tree was not added.
                let Ok(index) = slot_rx.recv() else {
                    return;
                };
                notify_loop(notifier, invalidations, move |invalidation| {
                    router.invalidation(index, invalidation)
                })
            })?;
        let (index, files_appear) = {
            let mut trees = self.trees.write();
            let files_appear = file && !trees.has_files();
            let index = trees
                .add(Slot {
                    name: name.clone(),
                    engine,
                    volume_use,
                    place,
                })
                .ok_or_else(|| std::io::Error::other("the mount has no tree slot left"))?;
            (index, files_appear)
        };
        let _ = slot_tx.send(index);
        if files_appear {
            self.entry_changed(false, &self.files_folder);
        }
        self.entry_changed(file, &name);
        Ok(index)
    }

    /// Removes the tree in slot `index`: its inodes lead nowhere from now
    /// on, so a path through it answers ENOENT and an open file of it EIO,
    /// also where the kernel had its pages cached (they are dropped).
    /// Returns its engine.
    pub fn remove(&self, index: usize) -> Option<Arc<Engine>> {
        let (slot, files_gone) = {
            let mut trees = self.trees.write();
            let slot = trees.remove(index)?;
            let files_gone = slot.is_file() && !trees.has_files();
            (slot, files_gone)
        };
        // The name first: paths through the tree end at once, however many
        // inodes the kernel holds. The inodes it still holds (open files,
        // a working directory) are invalidated after it, which drops their
        // pages; those it lets go with the entry need nothing.
        self.entry_changed(slot.is_file(), &slot.name);
        if files_gone {
            self.entry_changed(false, &self.files_folder);
        }
        for ino in slot.engine.kernel_inos() {
            if let Ok(ino) = self.router.global(index, ino) {
                let _ = self.top.send(Invalidation::Inode { ino });
            }
        }
        Some(slot.engine)
    }

    /// Shows the tree in slot `index` under `name`.
    pub fn rename(&self, index: usize, name: &str) -> bool {
        let (old, file) = {
            let mut trees = self.trees.write();
            let Some(old) = trees.rename(index, name.to_owned()) else {
                return false;
            };
            let file = trees.get(index).is_some_and(Slot::is_file);
            (old, file)
        };
        self.entry_changed(file, &old);
        self.entry_changed(file, name);
        true
    }

    /// The trees' slots and names, for tests and `status`.
    pub fn names(&self) -> Vec<(usize, String)> {
        self.trees
            .read()
            .iter()
            .map(|(index, slot)| (index, slot.name.clone()))
            .collect()
    }

    /// Drops the kernel's entry `name` of the top level or the files
    /// folder, and the folder's cached pages and attributes.
    fn entry_changed(&self, files: bool, name: &str) {
        let parent = if files { FILES_INO } else { TOP_INO };
        let _ = self.top.send(Invalidation::Entry {
            parent,
            name: name.to_owned(),
        });
        let _ = self.top.send(Invalidation::Inode { ino: parent });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Slots without engines: the set's bookkeeping alone.
    struct Names(Vec<Option<(String, bool)>>);

    impl Names {
        fn of(trees: &Trees) -> Self {
            Self(
                trees
                    .slots
                    .iter()
                    .map(|slot| slot.as_ref().map(|s| (s.name.clone(), s.is_file())))
                    .collect(),
            )
        }
    }

    async fn engine(dir: &std::path::Path) -> Arc<Engine> {
        let options = pdmount_core::engine::EngineOptions::in_dir(
            dir,
            pdmount_core::Config::default(),
            pdmount_core::device::DeviceIdentity {
                id: pdmount_core::device::Uuid::from_u128(3),
                name: "trees-test".into(),
            },
            pdmount_core::conflicts::system_clock(),
        );
        let backend = Arc::new(pdmount_backend::fake::FakeBackend::new());
        Engine::open(backend, options).await.expect("engine").0
    }

    fn slot(engine: &Arc<Engine>, name: &str, place: TreePlace) -> Slot {
        Slot {
            name: name.into(),
            engine: Arc::clone(engine),
            volume_use: None,
            place,
        }
    }

    #[tokio::test]
    async fn slots_are_never_reused_and_keep_their_numbers() {
        let dir = tempfile::tempdir().expect("tempdir");
        let engine = engine(dir.path()).await;
        let mut trees = Trees::new(vec![slot(&engine, "a", TreePlace::Folder)]);
        let b = trees
            .add(slot(&engine, "b", TreePlace::Folder))
            .expect("room");
        let f = trees
            .add(slot(&engine, "f.txt", TreePlace::File("file".into())))
            .expect("room");
        assert_eq!((b, f), (1, 2));
        assert_eq!(trees.find(false, b"b"), Some(1));
        assert_eq!(trees.find(true, b"f.txt"), Some(2));
        // A file is not a folder of the top level, and the other way round.
        assert_eq!(trees.find(false, b"f.txt"), None);
        assert_eq!(trees.find(true, b"b"), None);
        assert!(trees.has_files());

        // Removed: its slot stays taken, the others keep theirs.
        let before = trees.changed;
        assert!(trees.remove(1).is_some());
        assert!(trees.remove(1).is_none());
        assert!(trees.removed(1) && !trees.removed(0) && !trees.removed(9));
        assert!(trees.changed >= before);
        assert_eq!(trees.find(false, b"b"), None);
        let c = trees
            .add(slot(&engine, "b", TreePlace::Folder))
            .expect("room");
        assert_eq!(c, 3, "a new tree of the same name gets a new slot");
        assert_eq!(trees.find(false, b"b"), Some(3));
        assert_eq!(trees.find(false, b"a"), Some(0));

        // Renamed in place.
        assert_eq!(trees.rename(3, "b (bob)".into()), Some("b".into()));
        assert_eq!(trees.rename(3, "b (bob)".into()), None, "unchanged");
        assert_eq!(trees.find(false, b"b (bob)"), Some(3));
        assert!(trees.remove(2).is_some());
        assert!(!trees.has_files());
        let Names(names) = Names::of(&trees);
        assert_eq!(
            names,
            [
                Some(("a".into(), false)),
                None,
                None,
                Some(("b (bob)".into(), false)),
            ]
        );
        assert_eq!(
            trees.iter().map(|(index, _)| index).collect::<Vec<_>>(),
            [0, 3]
        );
    }

    #[tokio::test]
    async fn a_mount_runs_out_of_slots_rather_than_reusing_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let engine = engine(dir.path()).await;
        let mut trees = Trees::new(Vec::new());
        // Fill up to the last slot cheaply: the bookkeeping only.
        trees.slots.resize_with(MAX_TREES - 1, || None);
        let last = trees
            .add(slot(&engine, "last", TreePlace::Folder))
            .expect("the last slot");
        assert_eq!(last, MAX_TREES - 1);
        assert!(Router::routed().global(last, 1).is_ok());
        assert!(
            trees
                .add(slot(&engine, "more", TreePlace::Folder))
                .is_none()
        );
    }
}
