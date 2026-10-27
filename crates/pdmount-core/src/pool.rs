//! Block memory reserved at startup (DESIGN.md §6.2, D049).
//!
//! The RAM cache, `seed` and block reads take their buffers from here, so
//! after startup no block-sized allocation is made for them. Each size class
//! is one anonymous mapping, populated when the pool is made and resident
//! from then on, cut into fixed slots. A slot is on its class's free list, or
//! owned by one [`SlotMut`], or by the [`Bytes`] it was frozen into; the last
//! clone of those `Bytes` puts it back on the free list.
//!
//! A slot holds a sealed block (nonce ‖ ciphertext ‖ tag, as in the disk
//! cache), so it is [`SEAL_OVERHEAD`] bytes larger than its class's
//! capacity. The plaintext sits at [`PLAIN_START`].
//!
//! Besides the block classes, a few scratch slots of [`SCRATCH_CAPACITY`]
//! hold replies that span two blocks while they are assembled.

// The mapping is raw memory: every `unsafe` block below relies on a slot
// being either free or owned by exactly one `SlotMut` (see `SlotMut`).
#![allow(unsafe_code)]

use crate::errors::{CoreError, Result};
use bytes::Bytes;
use nix::sys::mman::{MapFlags, ProtFlags, mmap_anonymous, munmap};
use std::num::NonZeroUsize;
use std::ptr::NonNull;
use tokio::sync::Notify;

/// XChaCha20-Poly1305 nonce, before the ciphertext.
pub const NONCE_LEN: usize = 24;
/// Poly1305 tag, after the ciphertext.
pub const TAG_LEN: usize = 16;
pub const SEAL_OVERHEAD: usize = NONCE_LEN + TAG_LEN;
/// Where the plaintext starts in a slot.
pub const PLAIN_START: usize = NONCE_LEN;

/// Block capacity per class, smallest first. A block uses the smallest
/// class it fits in.
const CAPACITIES: [usize; 3] = [64 << 10, 1 << 20, 4 << 20];
/// The largest class needs this many slots: a read across a block boundary
/// holds two.
const MIN_LARGE_SLOTS: usize = 2;
/// The largest FUSE read (`max_pages` 256).
pub const SCRATCH_CAPACITY: usize = 1 << 20;
/// One per fuser thread (`FUSER_THREADS` in pdmount-fuse); not part of
/// `cache.ram_bytes`.
pub const SCRATCH_SLOTS: usize = 4;
/// The index of the scratch class, after the block classes.
pub const SCRATCH: usize = CAPACITIES.len();

/// One anonymous mapping, unmapped on drop.
struct Region {
    ptr: NonNull<u8>,
    len: usize,
}

// SAFETY: the region is plain memory; access to its slots is coordinated by
// the free lists (see `SlotMut`).
unsafe impl Send for Region {}
unsafe impl Sync for Region {}

impl Region {
    fn map(len: usize) -> Result<Self> {
        let Some(length) = NonZeroUsize::new(len) else {
            return Ok(Self {
                ptr: NonNull::dangling(),
                len: 0,
            });
        };
        // SAFETY: a fresh private anonymous mapping aliases nothing.
        let ptr = unsafe {
            mmap_anonymous(
                None,
                length,
                ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                MapFlags::MAP_PRIVATE | MapFlags::MAP_POPULATE,
            )
        }
        .map_err(|errno| {
            CoreError::Io(format!(
                "cannot map {len} bytes for the block pool: {errno}"
            ))
        })?;
        Ok(Self {
            ptr: ptr.cast(),
            len,
        })
    }
}

impl Drop for Region {
    fn drop(&mut self) {
        if self.len > 0 {
            // SAFETY: mapped in `map` with this length; every slot holds an
            // `Arc<Pool>`, so none is alive any more.
            let _ = unsafe { munmap(self.ptr.cast(), self.len) };
        }
    }
}

struct Class {
    capacity: usize,
    slot_size: usize,
    slots: usize,
    region: Region,
    /// Free slot indexes. Its capacity is `slots`, so pushing never
    /// allocates.
    free: parking_lot::Mutex<Vec<u32>>,
    /// Signalled when a slot is freed or becomes evictable.
    notify: Notify,
}

/// Slots of one class, for `pd stats`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClassUsage {
    /// The scratch class rather than a block class.
    pub scratch: bool,
    pub capacity: usize,
    pub slots: usize,
    pub in_use: usize,
}

pub struct Pool {
    classes: Vec<Class>,
}

/// The smallest `cache.ram_bytes`: two 4 MiB slots must fit in about 5/8
/// of it (§6.2). The config check uses it too.
pub const MIN_RAM_BYTES: u64 = 16 << 20;

impl Pool {
    /// Splits `ram_bytes` across the classes: about 5/8 to 4 MiB blocks
    /// (rounded to whole slots, so the 32 MiB default gives five, one per
    /// download in flight), 1/4 to 1 MiB and the rest to 64 KiB. Fails
    /// below [`MIN_RAM_BYTES`].
    pub fn new(ram_bytes: u64) -> Result<Self> {
        let total = usize::try_from(ram_bytes).unwrap_or(usize::MAX);
        let slot = |capacity: usize| capacity + SEAL_OVERHEAD;
        let large_slot = slot(CAPACITIES[2]);
        let large = (total / 8 * 5 + large_slot / 2) / large_slot;
        let large = large.min(total / large_slot);
        if ram_bytes < MIN_RAM_BYTES || large < MIN_LARGE_SLOTS {
            return Err(CoreError::Refused(format!(
                "cache.ram_bytes is {ram_bytes} bytes; the block pool needs room for two 4 MiB \
                 blocks in about 5/8 of it, so use at least 16 MiB"
            )));
        }
        let mut left = total - large * large_slot;
        let middle = (total / 4 / slot(CAPACITIES[1])).min(left / slot(CAPACITIES[1]));
        left -= middle * slot(CAPACITIES[1]);
        let small = left / slot(CAPACITIES[0]);
        Self::with_slots([small, middle, large])
    }

    /// A pool with the given number of slots per block class, smallest
    /// first, and [`SCRATCH_SLOTS`] scratch slots.
    pub(crate) fn with_slots(slots: [usize; 3]) -> Result<Self> {
        let classes = CAPACITIES
            .iter()
            .chain([&SCRATCH_CAPACITY])
            .zip(slots.into_iter().chain([SCRATCH_SLOTS]))
            .map(|(&capacity, slots)| {
                let slot_size = capacity + SEAL_OVERHEAD;
                Ok(Class {
                    capacity,
                    slot_size,
                    slots,
                    region: Region::map(slots * slot_size)?,
                    free: parking_lot::Mutex::new((0..slots as u32).rev().collect()),
                    notify: Notify::new(),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self { classes })
    }

    /// The smallest block class whose slots hold a block of `len` bytes.
    pub fn class_for(&self, len: usize) -> Option<usize> {
        self.classes[..SCRATCH]
            .iter()
            .position(|class| len <= class.capacity)
    }

    pub fn slot_size(&self, class: usize) -> usize {
        self.classes[class].slot_size
    }

    /// A free slot of `class`, if any.
    pub fn try_take(self: &std::sync::Arc<Self>, class: usize) -> Option<SlotMut> {
        let index = self.classes[class].free.lock().pop()?;
        Some(SlotMut {
            pool: std::sync::Arc::clone(self),
            class,
            index,
        })
    }

    /// Resolves after a slot of `class` was freed or became evictable since
    /// the last wait (a wake-up with nobody waiting is kept for the next).
    pub async fn changed(&self, class: usize) {
        self.classes[class].notify.notified().await;
    }

    /// Wakes one waiter for `class`: a slot was freed or can be evicted now.
    pub fn wake(&self, class: usize) {
        self.classes[class].notify.notify_one();
    }

    pub fn usage(&self) -> Vec<ClassUsage> {
        self.classes
            .iter()
            .enumerate()
            .map(|(index, class)| ClassUsage {
                scratch: index == SCRATCH,
                capacity: class.capacity,
                slots: class.slots,
                in_use: class.slots - class.free.lock().len(),
            })
            .collect()
    }

    fn release(&self, class: usize, index: u32) {
        self.classes[class].free.lock().push(index);
        self.wake(class);
    }
}

/// A slot taken from the free list, writable by its one owner. Frozen into
/// [`Bytes`] it is read-only; it goes back to the free list when dropped.
pub struct SlotMut {
    pool: std::sync::Arc<Pool>,
    class: usize,
    index: u32,
}

impl SlotMut {
    fn ptr(&self) -> *mut u8 {
        let class = &self.pool.classes[self.class];
        // SAFETY: `index < slots`, so the slot lies inside the region.
        unsafe {
            class
                .region
                .ptr
                .as_ptr()
                .add(self.index as usize * class.slot_size)
        }
    }

    pub fn class(&self) -> usize {
        self.class
    }
}

impl AsRef<[u8]> for SlotMut {
    fn as_ref(&self) -> &[u8] {
        // SAFETY: the slot is inside the mapping, which outlives `self` (it
        // holds an `Arc<Pool>`), and only this owner reaches it.
        unsafe { std::slice::from_raw_parts(self.ptr(), self.pool.slot_size(self.class)) }
    }
}

impl AsMut<[u8]> for SlotMut {
    fn as_mut(&mut self) -> &mut [u8] {
        // SAFETY: as in `as_ref`; `&mut self` makes the access exclusive.
        unsafe { std::slice::from_raw_parts_mut(self.ptr(), self.pool.slot_size(self.class)) }
    }
}

impl Drop for SlotMut {
    fn drop(&mut self) {
        self.pool.release(self.class, self.index);
    }
}

/// A block buffer: a pool slot, or a heap allocation when the pool had none
/// (counted in `cache_pool_fallbacks`). Laid out like a slot, with the
/// plaintext at [`PLAIN_START`].
pub enum Buffer {
    Slot(SlotMut),
    Heap(Vec<u8>),
}

impl Buffer {
    /// A heap buffer for a block of `len` bytes.
    pub fn heap(len: usize) -> Self {
        Self::Heap(vec![0; len + SEAL_OVERHEAD])
    }

    /// The pool class, if the buffer is a slot.
    pub fn class(&self) -> Option<usize> {
        match self {
            Self::Slot(slot) => Some(slot.class()),
            Self::Heap(_) => None,
        }
    }

    /// The plaintext's `len` bytes as `Bytes`, without copying.
    pub fn freeze(self, len: usize) -> Bytes {
        let bytes = match self {
            Self::Slot(slot) => Bytes::from_owner(slot),
            Self::Heap(vec) => Bytes::from(vec),
        };
        bytes.slice(PLAIN_START..PLAIN_START + len)
    }
}

impl AsMut<[u8]> for Buffer {
    fn as_mut(&mut self) -> &mut [u8] {
        match self {
            Self::Slot(slot) => slot.as_mut(),
            Self::Heap(vec) => vec,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    const MIB: u64 = 1 << 20;

    #[test]
    fn the_default_split_gives_five_large_slots() {
        let pool = Pool::new(32 * MIB).expect("pool");
        let usage = pool.usage();
        assert_eq!(
            usage.iter().map(|class| class.slots).collect::<Vec<_>>(),
            vec![79, 7, 5, SCRATCH_SLOTS]
        );
        assert!(usage[SCRATCH].scratch);
        let reserved: usize = usage[..SCRATCH]
            .iter()
            .map(|class| class.slots * (class.capacity + SEAL_OVERHEAD))
            .sum();
        assert!(reserved as u64 <= 32 * MIB);
        assert!(usage.iter().all(|class| class.in_use == 0));
    }

    #[test]
    fn too_small_a_pool_is_refused() {
        let error = Pool::new(8 * MIB).err().expect("refused");
        assert!(error.to_string().contains("cache.ram_bytes"), "{error}");
        Pool::new(16 * MIB).expect("16 MiB is enough");
    }

    #[test]
    fn blocks_use_the_smallest_class_they_fit() {
        let pool = Pool::new(16 * MIB).expect("pool");
        assert_eq!(pool.class_for(100), Some(0));
        assert_eq!(pool.class_for(64 << 10), Some(0));
        assert_eq!(pool.class_for((64 << 10) + 1), Some(1));
        assert_eq!(pool.class_for(4 << 20), Some(2));
        assert_eq!(pool.class_for((4 << 20) + 1), None);
    }

    #[test]
    fn slots_return_to_the_free_list_when_the_last_clone_drops() {
        let pool = Arc::new(Pool::new(16 * MIB).expect("pool"));
        let slots = pool.usage()[2].slots;
        let mut held: Vec<SlotMut> = (0..slots)
            .map(|_| pool.try_take(2).expect("slot"))
            .collect();
        assert!(pool.try_take(2).is_none());
        let mut slot = held.pop().expect("slot");
        slot.as_mut()[PLAIN_START..PLAIN_START + 3].copy_from_slice(b"abc");
        let data = Buffer::Slot(slot).freeze(3);
        let clone = data.slice(1..);
        drop(data);
        assert_eq!(pool.usage()[2].in_use, slots);
        assert_eq!(&clone[..], b"bc");
        drop(clone);
        assert_eq!(pool.usage()[2].in_use, slots - 1);
        let again = pool.try_take(2).expect("reused");
        assert!(pool.try_take(2).is_none());
        drop((again, held));
        assert_eq!(pool.usage()[2].in_use, 0);
    }

    #[test]
    fn slots_outlive_the_pool_handle() {
        let pool = Arc::new(Pool::new(16 * MIB).expect("pool"));
        let mut slot = pool.try_take(0).expect("slot");
        slot.as_mut()[PLAIN_START] = 7;
        let data = Buffer::Slot(slot).freeze(1);
        drop(pool);
        assert_eq!(&data[..], &[7]);
    }
}
