//! A read-only tree that is not an engine (M5.13, D113): what the FUSE
//! layer calls for a mount over it ([`pdmount_fuse::mount_read_only`]).
//!
//! The Photos tree ([`crate::photos::PhotosTree`]) is its one user: a
//! virtual tree over the Photos timeline with no folder operations of its
//! own (D080). It answers lookups, attributes, listings and reads; every
//! change is EROFS, answered by the FUSE layer without asking the tree.
//!
//! The methods without `async` are hot-path operations, as the engine's:
//! they use RAM and the local disk only, and return
//! [`CoreError::NeedsListing`] when a folder's entries must arrive first;
//! the FUSE layer then awaits [`ReadOnlyTree::resolve`] on its slow path and
//! asks again.

use crate::Config;
use crate::cache::{ReadData, ReadFrom};
use crate::engine::StatFs;
use crate::errors::{CoreError, Result};
use crate::health::Health;
use crate::metrics::Metrics;
use crate::vfs::{Attr, DirEntry, Ino, Lookup};
use std::sync::Arc;

#[async_trait::async_trait]
pub trait ReadOnlyTree: Send + Sync + 'static {
    /// `name` in folder `parent`.
    fn lookup(&self, parent: Ino, name: &[u8]) -> Result<Lookup>;

    fn getattr(&self, ino: Ino) -> Result<Attr>;

    /// Opens a folder: a handle on a snapshot of its entries, and whether
    /// the kernel may keep the pages it has of it (D072: only while the
    /// entries hash the same as at its last open).
    fn opendir(&self, ino: Ino) -> Result<(u64, bool)>;

    /// The entries of an open folder, `.` and `..` first.
    fn dir_snapshot(&self, handle: u64) -> Result<Arc<Vec<DirEntry>>>;

    fn releasedir(&self, handle: u64);

    /// Opens a file for reading: a handle, and whether the kernel may keep
    /// its cached pages (the content is the one it has).
    fn open(&self, ino: Ino) -> Result<(u64, bool)>;

    /// A read from RAM or the disk cache; `None` when a block must be
    /// downloaded ([`ReadOnlyTree::read`]).
    fn read_local(
        &self,
        handle: u64,
        offset: u64,
        size: u32,
    ) -> Result<Option<(ReadData, ReadFrom)>>;

    /// A read that downloads what it misses.
    async fn read(&self, handle: u64, offset: u64, size: u32) -> Result<ReadData>;

    fn release(&self, handle: u64);

    /// The slow path of a hot-path error ([`CoreError::NeedsListing`]): waits
    /// until the folder's entries the request needs are there, or as many
    /// as arrive within the request's deadline. Other errors come back.
    async fn resolve(&self, error: &CoreError) -> Result<()>;

    fn statfs(&self) -> StatFs;

    fn metrics(&self) -> &Arc<Metrics>;

    fn health(&self) -> &Arc<Health>;

    fn config(&self) -> &Config;

    fn runtime(&self) -> &tokio::runtime::Handle;
}
