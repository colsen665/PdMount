//! Inode numbers of a mount with several trees (D110), or with a set of
//! trees that changes while it is mounted (D112).

use pdmount_core::errors::{CoreError, Result};
use pdmount_core::vfs::{Attr, DirEntry, Ino, Invalidation, Lookup, ROOT_INO};

/// Bits of a routed inode number that hold the tree's own number.
pub(super) const TREE_SHIFT: u32 = 48;

pub(super) const LOCAL_MASK: u64 = (1 << TREE_SHIFT) - 1;

/// The virtual top level of a mount with several trees.
pub(super) const TOP_INO: u64 = 1;

/// The files folder: a virtual folder in the top level that holds the
/// trees whose root is a single file (shared files, D112). No tree number
/// is this small.
pub(super) const FILES_INO: u64 = 2;

/// Inode numbers of a mount (D110). With one tree there is no router: the
/// kernel sees the engine's own numbers, so My Files keeps its paths and
/// inode numbers. With several, the top level is inode 1 and inode `n` of
/// tree `i` is `(i + 1) << 48 | n`; a tree's root folder (its inode 1) is
/// then the tree's folder in the top level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Router {
    pub(super) routed: bool,
}

/// Where a kernel inode number leads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// The virtual top level.
    Top,
    /// The files folder in the top level.
    Files,
    /// Tree `.0`, its inode `.1`.
    Tree(usize, Ino),
}

impl Router {
    pub fn new(trees: usize) -> Self {
        Self { routed: trees > 1 }
    }

    /// The router of a mount whose trees change while it is mounted: it
    /// routes whatever number of trees it holds, none included.
    pub fn routed() -> Self {
        Self { routed: true }
    }

    pub fn is_routed(self) -> bool {
        self.routed
    }

    /// The tree and its inode behind a kernel inode number.
    pub fn route(self, ino: u64) -> Route {
        if !self.routed {
            return Route::Tree(0, ino);
        }
        if ino == TOP_INO {
            return Route::Top;
        }
        if ino == FILES_INO {
            return Route::Files;
        }
        Route::Tree(
            ((ino >> TREE_SHIFT) as usize).wrapping_sub(1),
            ino & LOCAL_MASK,
        )
    }

    /// The kernel's number of tree `tree`'s inode `local`; EIO for an inode
    /// number too large to route (an engine reserves numbers 16 384 at a
    /// time from 2, so 2^48 is never reached in practice).
    pub fn global(self, tree: usize, local: Ino) -> Result<u64> {
        if !self.routed {
            return Ok(local);
        }
        if local > LOCAL_MASK || local == 0 || tree >= (1 << (64 - TREE_SHIFT)) - 1 {
            return Err(CoreError::Internal(format!(
                "inode {local} of tree {tree} cannot be routed"
            )));
        }
        Ok(((tree as u64 + 1) << TREE_SHIFT) | local)
    }

    pub(super) fn attr(self, tree: usize, mut attr: Attr) -> Result<Attr> {
        attr.ino = self.global(tree, attr.ino)?;
        Ok(attr)
    }

    pub(super) fn lookup(self, tree: usize, lookup: Lookup) -> Result<Lookup> {
        Ok(match lookup {
            Lookup::Found(attr) => Lookup::Found(self.attr(tree, attr)?),
            other => other,
        })
    }

    /// A directory entry of tree `tree`'s directory `dir`: the `..` of a
    /// tree's root is the top level.
    pub(super) fn entry(self, tree: usize, dir: Ino, entry: &DirEntry) -> Result<DirEntry> {
        let ino = if self.routed && dir == ROOT_INO && entry.name == ".." {
            TOP_INO
        } else {
            self.global(tree, entry.ino)?
        };
        Ok(DirEntry {
            ino,
            kind: entry.kind,
            name: entry.name.clone(),
        })
    }

    pub(super) fn invalidation(
        self,
        tree: usize,
        invalidation: Invalidation,
    ) -> Result<Invalidation> {
        Ok(match invalidation {
            Invalidation::Entry { parent, name } => Invalidation::Entry {
                parent: self.global(tree, parent)?,
                name,
            },
            Invalidation::Inode { ino } => Invalidation::Inode {
                ino: self.global(tree, ino)?,
            },
            Invalidation::Delete {
                parent,
                child,
                name,
            } => Invalidation::Delete {
                parent: self.global(tree, parent)?,
                child: self.global(tree, child)?,
                name,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pdmount_backend::NodeKind;

    #[test]
    fn one_tree_keeps_its_inode_numbers() {
        let router = Router::new(1);
        assert!(!router.is_routed());
        for ino in [1, 2, 77, LOCAL_MASK + 5] {
            assert_eq!(router.route(ino), Route::Tree(0, ino));
            assert_eq!(router.global(0, ino).expect("global"), ino);
        }
    }

    #[test]
    fn several_trees_route_by_the_high_bits() {
        let router = Router::new(3);
        assert!(router.is_routed());
        assert_eq!(router.route(1), Route::Top);
        // A tree's root is its folder in the top level.
        let root = router.global(0, ROOT_INO).expect("global");
        assert_eq!(root, (1 << 48) | 1);
        assert_ne!(root, TOP_INO);
        assert_eq!(router.route(root), Route::Tree(0, ROOT_INO));
        for (tree, local) in [(0, 2), (1, 1), (2, 16_385), (2, LOCAL_MASK)] {
            let global = router.global(tree, local).expect("global");
            assert_eq!(router.route(global), Route::Tree(tree, local));
        }
        // Numbers no tree could have.
        assert!(router.global(0, LOCAL_MASK + 1).is_err());
        assert!(router.global(0, 0).is_err());
        assert_eq!(router.route(5), Route::Tree(usize::MAX, 5));
        assert_eq!(router.route(FILES_INO), Route::Files);
    }

    #[test]
    fn a_changing_mount_routes_any_number_of_trees() {
        let router = Router::routed();
        assert!(router.is_routed());
        assert_eq!(router.route(TOP_INO), Route::Top);
        assert_eq!(router.route(FILES_INO), Route::Files);
        // The slot decides the numbers, whatever else the mount holds.
        let global = router.global(7, 42).expect("global");
        assert_eq!(global, (8 << 48) | 42);
        assert_eq!(router.route(global), Route::Tree(7, 42));
    }

    #[test]
    fn a_tree_roots_parent_is_the_top_level() {
        let router = Router::new(2);
        let entry = |name: &str, ino| DirEntry {
            ino,
            kind: NodeKind::Folder,
            name: name.into(),
        };
        let up = router
            .entry(1, ROOT_INO, &entry("..", ROOT_INO))
            .expect("entry");
        assert_eq!(up.ino, TOP_INO);
        let here = router
            .entry(1, ROOT_INO, &entry(".", ROOT_INO))
            .expect("entry");
        assert_eq!(here.ino, (2 << 48) | 1);
        let below = router.entry(1, 9, &entry("..", ROOT_INO)).expect("entry");
        assert_eq!(below.ino, (2 << 48) | 1);
        // Without a router nothing changes.
        let single = Router::new(1);
        assert_eq!(
            single
                .entry(0, ROOT_INO, &entry("..", ROOT_INO))
                .expect("entry")
                .ino,
            ROOT_INO
        );
    }

    #[test]
    fn invalidations_carry_the_routed_numbers() {
        let router = Router::new(2);
        assert_eq!(
            router
                .invalidation(
                    1,
                    Invalidation::Delete {
                        parent: ROOT_INO,
                        child: 7,
                        name: "x".into()
                    }
                )
                .expect("routed"),
            Invalidation::Delete {
                parent: (2 << 48) | 1,
                child: (2 << 48) | 7,
                name: "x".into()
            }
        );
        assert_eq!(
            router
                .invalidation(0, Invalidation::Inode { ino: 3 })
                .expect("routed"),
            Invalidation::Inode { ino: (1 << 48) | 3 }
        );
    }
}
