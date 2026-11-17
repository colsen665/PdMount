//! The profile-wide disk cap shared by the caches of a profile's trees (D110).

use super::*;

/// One disk cap for the block caches of a profile's trees (D110 amended):
/// My Files' and every further tree's. Each cache keeps its own index and
/// evicts only its own blocks; here it reports its unpinned bytes, and a
/// cache that takes the total over 90 % of the cap makes room down to 80 %
/// by [`DiskBudget::enforce`]'s rule. A cache opened without one has a
/// budget of its own, which is the cache alone as before.
///
/// Locks: `members` takes nothing while held. `enforcing` is taken before
/// any cache's index lock and never while one is held, so a cache that
/// asks another one to evict holds no lock of its own.
pub struct DiskBudget {
    pub(super) cap: u64,
    pub(super) members: parking_lot::Mutex<Vec<Arc<Member>>>,
    /// One enforcement at a time, so two do not both evict for one excess.
    pub(super) enforcing: parking_lot::Mutex<()>,
    pub(super) next_id: AtomicU64,
}

/// A cache on a [`DiskBudget`].
pub(super) struct Member {
    pub(super) id: u64,
    /// My Files: the others never push it below half of the cap.
    pub(super) keeps_half: bool,
    /// The cache's unpinned bytes, written under its index lock.
    pub(super) bytes: Arc<AtomicU64>,
    /// Evicts the cache's least recently used blocks down to a target, as
    /// far as protected revisions allow and never below a floor; nothing
    /// once the cache is gone.
    pub(super) evict: Box<dyn Fn(u64, u64) -> Evicted + Send + Sync>,
}

impl Member {
    pub(super) fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }
}

/// A cache's place on a [`DiskBudget`]; it leaves when the cache drops.
pub(super) struct Seat {
    pub(super) budget: Arc<DiskBudget>,
    pub(super) id: u64,
}

impl Drop for Seat {
    fn drop(&mut self) {
        self.budget
            .members
            .lock()
            .retain(|member| member.id != self.id);
    }
}

/// A cache's part in a profile's [`DiskBudget`].
#[derive(Clone)]
pub struct SharedCap {
    pub budget: Arc<DiskBudget>,
    /// My Files' cache: kept at half the cap or more when others need room.
    pub keeps_half: bool,
}

impl DiskBudget {
    pub fn new(cap: u64) -> Arc<Self> {
        Arc::new(Self {
            cap,
            members: parking_lot::Mutex::new(Vec::new()),
            enforcing: parking_lot::Mutex::new(()),
            next_id: AtomicU64::new(0),
        })
    }

    /// `cache.max_bytes`.
    pub fn cap(&self) -> u64 {
        self.cap
    }

    /// The unpinned bytes of every cache on the budget.
    pub fn used(&self) -> u64 {
        self.members
            .lock()
            .iter()
            .map(|member| member.bytes())
            .sum()
    }

    /// Caches on the budget.
    pub fn caches(&self) -> usize {
        self.members.lock().len()
    }

    pub(super) fn join(
        self: &Arc<Self>,
        keeps_half: bool,
        bytes: Arc<AtomicU64>,
        evict: Box<dyn Fn(u64, u64) -> Evicted + Send + Sync>,
    ) -> Seat {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.members.lock().push(Arc::new(Member {
            id,
            keeps_half,
            bytes,
            evict,
        }));
        Seat {
            budget: Arc::clone(self),
            id,
        }
    }

    /// Each member's share of the cap: alone, all of it; otherwise the one
    /// that keeps half has half, and the others split the rest evenly.
    pub(super) fn shares(&self, members: &[Arc<Member>]) -> Vec<u64> {
        if members.len() <= 1 {
            return vec![self.cap; members.len()];
        }
        let half = if members.iter().any(|member| member.keeps_half) {
            self.cap / 2
        } else {
            0
        };
        let others = members.iter().filter(|member| !member.keeps_half).count() as u64;
        let each = (self.cap - half) / others.max(1);
        members
            .iter()
            .map(|member| if member.keeps_half { half } else { each })
            .collect()
    }

    /// Called by the cache `writer` after its bytes grew (a block stored,
    /// a pin let go, the cache opened): above 90 % of the cap in total,
    /// makes room down to 80 %. A writer over its share evicts its own
    /// blocks first. Then each other cache over its share, the one furthest
    /// over first, evicts down to its share at most, so My Files (whose
    /// share is half the cap) never goes below half for the others. Last
    /// the writer evicts its own, also below its share: free space is
    /// anyone's, and a cache alone behaves as without a budget.
    pub(super) fn enforce(&self, writer: u64) {
        let high = self.cap / 10 * 9;
        if self.used() <= high {
            return;
        }
        let _enforcing = self.enforcing.lock();
        let members = self.members.lock().clone();
        let total = || members.iter().map(|member| member.bytes()).sum::<u64>();
        // Another enforcement may have made room meanwhile.
        if total() <= high {
            return;
        }
        let low = self.cap / 10 * 8;
        let shares = self.shares(&members);
        let me = members.iter().position(|member| member.id == writer);
        // By what the total is over the lower mark, never below `floor`.
        let evict = |at: usize, floor: u64| {
            let excess = total().saturating_sub(low);
            if excess > 0 {
                let bytes = members[at].bytes();
                (members[at].evict)(bytes.saturating_sub(excess), floor);
            }
        };
        let mut tried = vec![false; members.len()];
        if let Some(me) = me.filter(|&me| members[me].bytes() > shares[me]) {
            tried[me] = true;
            evict(me, 0);
        }
        while total() > low {
            let victim = (0..members.len())
                .filter(|&at| !tried[at] && Some(at) != me && members[at].bytes() > shares[at])
                .max_by_key(|&at| members[at].bytes() - shares[at]);
            let Some(victim) = victim else { break };
            tried[victim] = true;
            evict(victim, shares[victim]);
        }
        if let Some(me) = me.filter(|&me| !tried[me]) {
            evict(me, 0);
        }
    }
}
