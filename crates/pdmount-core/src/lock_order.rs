//! The engine's lock order ([`crate::engine`] module doc), checked in debug
//! builds.
//!
//! A ranked lock records its rank in a per-thread list while it is held.
//! Taking one whose rank is not above every rank held panics, so a reversed
//! order, a recursive read of the tree and two staging locks at once show
//! up in tests rather than as a rare deadlock. Guards are not `Send`, so
//! they never cross an `.await`, and a thread's list holds one task's
//! locks. Release builds keep only the rank field.

use std::ops::{Deref, DerefMut};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Rank {
    /// The lock of one inode's staging.
    Staging,
    /// The map from inode to staging.
    StagedMap,
    /// Access rules (§11).
    Access,
    Tree,
    /// Locks that take nothing while held: journal entries, the conflict
    /// registry, `.pdignore` rules.
    Leaf,
}

#[cfg(debug_assertions)]
thread_local! {
    static HELD: std::cell::RefCell<Vec<Rank>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn acquire(rank: Rank) {
    #[cfg(debug_assertions)]
    {
        let violated = HELD.with_borrow_mut(|held| {
            let top = held.iter().max().copied().filter(|&top| top >= rank);
            if top.is_none() {
                held.push(rank);
            }
            top
        });
        // Outside the borrow: unwinding drops guards, which borrow again.
        if let Some(top) = violated {
            panic!("lock order: {rank:?} taken while holding {top:?}");
        }
    }
    #[cfg(not(debug_assertions))]
    let _ = rank;
}

fn release(rank: Rank) {
    #[cfg(debug_assertions)]
    HELD.with_borrow_mut(|held| {
        if let Some(at) = held.iter().rposition(|&r| r == rank) {
            held.remove(at);
        }
    });
    #[cfg(not(debug_assertions))]
    let _ = rank;
}

/// A held ranked lock.
pub(crate) struct Guard<G> {
    guard: G,
    rank: Rank,
}

impl<G: Deref> Deref for Guard<G> {
    type Target = G::Target;

    fn deref(&self) -> &G::Target {
        &self.guard
    }
}

impl<G: DerefMut> DerefMut for Guard<G> {
    fn deref_mut(&mut self) -> &mut G::Target {
        &mut self.guard
    }
}

impl<G> Drop for Guard<G> {
    fn drop(&mut self) {
        release(self.rank);
    }
}

pub(crate) struct Mutex<T> {
    rank: Rank,
    inner: parking_lot::Mutex<T>,
}

impl<T> Mutex<T> {
    pub(crate) const fn new(rank: Rank, value: T) -> Self {
        Self {
            rank,
            inner: parking_lot::Mutex::new(value),
        }
    }

    pub(crate) fn lock(&self) -> Guard<parking_lot::MutexGuard<'_, T>> {
        acquire(self.rank);
        Guard {
            guard: self.inner.lock(),
            rank: self.rank,
        }
    }
}

pub(crate) struct RwLock<T> {
    rank: Rank,
    inner: parking_lot::RwLock<T>,
}

impl<T> RwLock<T> {
    pub(crate) const fn new(rank: Rank, value: T) -> Self {
        Self {
            rank,
            inner: parking_lot::RwLock::new(value),
        }
    }

    pub(crate) fn read(&self) -> Guard<parking_lot::RwLockReadGuard<'_, T>> {
        acquire(self.rank);
        Guard {
            guard: self.inner.read(),
            rank: self.rank,
        }
    }

    pub(crate) fn write(&self) -> Guard<parking_lot::RwLockWriteGuard<'_, T>> {
        acquire(self.rank);
        Guard {
            guard: self.inner.write(),
            rank: self.rank,
        }
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;

    #[test]
    fn locks_in_rank_order_pass() {
        let staging = Mutex::new(Rank::Staging, ());
        let tree = RwLock::new(Rank::Tree, ());
        let leaf = Mutex::new(Rank::Leaf, ());
        let _staging = staging.lock();
        let _tree = tree.read();
        drop(leaf.lock());
        drop(_staging);
        // Released out of order: the tree is still held.
        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(staging.lock())));
        assert!(result.is_err());
    }

    #[test]
    #[should_panic(expected = "Tree taken while holding Tree")]
    fn a_recursive_read_panics() {
        let tree = RwLock::new(Rank::Tree, ());
        let _first = tree.read();
        let _second = tree.read();
    }

    #[test]
    #[should_panic(expected = "Access taken while holding Tree")]
    fn a_reversed_order_panics() {
        let access = RwLock::new(Rank::Access, ());
        let tree = RwLock::new(Rank::Tree, ());
        let _tree = tree.read();
        let _access = access.read();
    }
}
