//! Block cache (DESIGN.md §6).
//!
//! Content is cached per block of an immutable revision, so entries are only
//! ever evicted, never invalidated. Lookups go RAM LRU → encrypted disk cache
//! → download. Downloads of the same block are shared ([`SingleFlight`]) and
//! limited per priority class. Block buffers come from the [`Pool`] reserved
//! at startup (D049).
//!
//! # Module map
//!
//! - `layout.rs`: [`BlockKey`], [`Priority`], [`BlockSpan`] and [`Layout`], the
//!   block layout of a revision.
//! - `read.rs`: [`ReadFrom`], [`ReadData`] and [`ReadBuf`], what a read returns
//!   and how it is assembled.
//! - `ram.rs`: the RAM LRU of blocks.
//! - `disk.rs`: the encrypted disk cache: index, read, put, pins and
//!   protections, eviction, wipe, the orphan sweep, [`DiskStats`].
//! - `budget.rs`: [`DiskBudget`] and [`SharedCap`], the profile-wide disk cap
//!   shared by the caches of a profile's trees (D110).
//! - `block_cache.rs`: [`CacheOptions`] and the [`BlockCache`] facade
//!   (RAM, disk and download, buffers from the pool).
//! - `tests.rs`: the tests of all of the above.

use crate::errors::{CoreError, Result};
use crate::metrics::{Metrics, check_not_hot};
use crate::pool::{
    Buffer, ClassUsage, NONCE_LEN, PLAIN_START, Pool, SCRATCH, SCRATCH_CAPACITY, SEAL_OVERHEAD,
    SlotMut, TAG_LEN,
};
use crate::singleflight::SingleFlight;
use crate::staging::Protections;
use crate::store::Store;
use bytes::Bytes;
use chacha20poly1305::aead::{AeadCore, AeadInPlace, KeyInit, OsRng};
use chacha20poly1305::{Tag, XChaCha20Poly1305, XNonce};
use pdmount_backend::{Backend, BlockInfo, LinkId, RevisionId};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::Write;
use std::num::NonZeroUsize;
use std::ops::Range;
use std::os::unix::fs::{DirBuilderExt, FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Proton's plaintext block size (`sdk/node/revision.rs:34`).
pub const BLOCK_SIZE: u64 = 4 * 1024 * 1024;

mod block_cache;
mod budget;
mod disk;
mod layout;
mod ram;
mod read;
#[cfg(test)]
mod tests;

pub use block_cache::{BlockCache, CacheOptions, DownloadSlot, DownloadSlots};
pub use budget::{DiskBudget, SharedCap};
pub use disk::{DiskStats, Evicted, Swept};
pub use layout::{BlockKey, BlockSpan, Layout, Priority};
pub use read::{READ_BLOCKS_HELD, ReadBuf, ReadData, ReadFrom};

use budget::Seat;
use disk::DiskCache;
use layout::store_layout;
use ram::{Cached, RamCache};
