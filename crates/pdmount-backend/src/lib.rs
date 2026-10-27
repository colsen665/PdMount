//! Storage backend interface (DESIGN.md §3.2).
//!
//! The filesystem core talks only to [`Backend`]. Two implementations exist:
//! [`proton::ProtonBackend`] (the community SDK) and [`fake::FakeBackend`]
//! (an in-memory model of Proton Drive for tests). SDK types never cross
//! this boundary.

pub mod fake;
pub mod proton;

use bytes::Bytes;
use std::fmt;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

macro_rules! id_type {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

id_type!(
    /// Remote node id, stable across renames and moves.
    LinkId
);
id_type!(
    /// Revision id; revisions are immutable.
    RevisionId
);
id_type!(
    /// Position in the volume event stream.
    EventId
);
id_type!(
    /// Server-side hash of a node's name, used as rename precondition.
    NameHash
);

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum NodeKind {
    File,
    Folder,
}

/// Metadata of a remote node, decrypted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteNode {
    pub link_id: LinkId,
    /// `None` only for the root.
    pub parent_link_id: Option<LinkId>,
    pub name: String,
    pub kind: NodeKind,
    /// Active revision; `None` for folders.
    pub active_revision: Option<RevisionId>,
    /// Plaintext size of the active revision; `None` when unknown.
    pub size: Option<u64>,
    pub mtime: Option<SystemTime>,
    /// POSIX permission bits stored by PdMount (`pdmount.mode`).
    pub mode: Option<u32>,
    pub sha1: Option<[u8; 20]>,
    pub name_hash: Option<NameHash>,
    pub trashed: bool,
}

/// Layout of one block of a revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockInfo {
    /// 1-based block index.
    pub index: u32,
    /// Plaintext size; `None` when the revision does not declare it.
    pub plain_size: Option<u32>,
}

/// Content to upload. The size must be known up front.
#[derive(Debug, Clone)]
pub enum UploadSource {
    Bytes(Bytes),
    File(PathBuf),
}

/// Metadata sent with a new revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileMeta {
    pub mtime: SystemTime,
    pub mode: Option<u32>,
    pub media_type: String,
    /// SHA-1 of the content, checked by the server-side commit when given.
    pub sha1: Option<[u8; 20]>,
}

/// One revision of a file, for tests and diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevisionMeta {
    pub id: RevisionId,
    pub created: SystemTime,
    pub size: Option<u64>,
    /// `None` when the uploading client did not claim a SHA-1.
    pub sha1: Option<[u8; 20]>,
    /// Client UID that uploaded the revision, when recorded.
    pub client_uid: Option<String>,
    pub active: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum EventKind {
    Delete,
    Create,
    UpdateMetadata,
    UpdateContent,
}

/// One volume event. Events carry ids only; the node is fetched separately.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RemoteEvent {
    pub kind: EventKind,
    pub link_id: LinkId,
    pub parent_link_id: Option<LinkId>,
    pub trashed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventPage {
    /// Cursor to poll from next.
    pub next: EventId,
    /// More events are available immediately.
    pub more: bool,
    /// The server asks the client to re-list everything.
    pub refresh: bool,
    pub events: Vec<RemoteEvent>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StorageInfo {
    pub used: u64,
    pub max: u64,
}

/// Account and drive identity, for `pdmount status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountInfo {
    pub username: String,
    pub root: LinkId,
}

/// Classified backend failure (DESIGN.md §13 and Appendix A).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BackendError {
    #[error("not found")]
    NotFound,
    #[error("a node with this name already exists")]
    AlreadyExists { existing: Option<LinkId> },
    #[error("permission denied")]
    PermissionDenied,
    #[error("storage quota exceeded")]
    QuotaExceeded,
    #[error("file too large for this plan")]
    FileTooLarge,
    #[error("folder has too many children")]
    TooManyChildren,
    #[error("folder nesting too deep")]
    NestingTooDeep,
    #[error("the base revision is no longer the active revision")]
    RevisionConflict { current: Option<RevisionId> },
    #[error("the node changed since it was read")]
    NameConflict,
    #[error("the move would put a folder inside itself")]
    InvalidMove,
    #[error("rate limited")]
    RateLimited { retry_after: Option<Duration> },
    #[error("temporary failure: {0}")]
    Transient(String),
    #[error("not signed in or session expired")]
    Unauthorized,
    #[error("integrity check failed: {0}")]
    Integrity(String),
    #[error("offline: {0}")]
    Offline(String),
    #[error("not supported yet: {0}")]
    Unsupported(&'static str),
    #[error("{0}")]
    Other(String),
}

impl BackendError {
    /// Whether repeating the operation later may succeed.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::RateLimited { .. } | Self::Transient(_) | Self::Offline(_)
        )
    }

    /// No connection or no session: the operation is not at fault and
    /// succeeds once the drive is reachable again.
    pub fn is_unreachable(&self) -> bool {
        matches!(self, Self::Offline(_) | Self::Unauthorized)
    }

    /// Neither waiting nor a new session helps: the same request fails again.
    pub fn is_permanent(&self) -> bool {
        !self.is_retryable() && !self.is_unreachable()
    }

    /// Permanent failures that no retry can fix and that need the user (quota,
    /// plan limits, permissions): no point in trying again before parking.
    pub fn needs_the_user(&self) -> bool {
        matches!(
            self,
            Self::QuotaExceeded
                | Self::FileTooLarge
                | Self::Unsupported(_)
                | Self::PermissionDenied
                | Self::TooManyChildren
                | Self::NestingTooDeep
        )
    }
}

pub type Result<T> = std::result::Result<T, BackendError>;

/// The largest plaintext block Proton Drive stores (and PdMount uploads).
pub const MAX_BLOCK_BYTES: usize = 4 << 20;

/// Copies a fetched block into the caller's buffer (`fetch_block_into` of
/// a backend that gets its blocks as owned bytes).
pub fn copy_block(data: &[u8], buf: &mut [u8], index: u32, revision: &RevisionId) -> Result<usize> {
    let capacity = buf.len();
    let target = buf.get_mut(..data.len()).ok_or_else(|| {
        BackendError::Integrity(format!(
            "block {index} of {revision} has {} bytes, more than the {capacity} expected",
            data.len()
        ))
    })?;
    target.copy_from_slice(data);
    Ok(data.len())
}

/// Operations PdMount needs from Proton Drive "My Files".
#[async_trait::async_trait]
pub trait Backend: Send + Sync + 'static {
    /// Account name and My Files root.
    async fn account(&self) -> Result<AccountInfo>;

    /// Plaintext block size of uploads: revisions uploaded by PdMount are
    /// split into blocks of this size, last one shorter. The block cache is
    /// seeded with that layout after an upload.
    fn upload_block_size(&self) -> u64 {
        4 << 20
    }

    /// The My Files root folder.
    async fn root(&self) -> Result<RemoteNode>;

    /// All children of a folder (paginated internally). Trashed children
    /// are not listed.
    async fn list_children(&self, folder: &LinkId) -> Result<Vec<RemoteNode>>;

    /// Nodes by id, in request order; `None` for ids that do not exist.
    async fn get_nodes(&self, ids: &[LinkId]) -> Result<Vec<Option<RemoteNode>>>;

    /// Block layout of a revision.
    async fn blocks(&self, file: &LinkId, revision: &RevisionId) -> Result<Vec<BlockInfo>>;

    /// Verified plaintext of one block (1-based index) into `buf`, returning
    /// the block's length. A block longer than `buf` is an
    /// [`BackendError::Integrity`] error. The block cache reads through
    /// this, so every backend and wrapper implements it.
    async fn fetch_block_into(
        &self,
        file: &LinkId,
        revision: &RevisionId,
        index: u32,
        buf: &mut [u8],
    ) -> Result<usize>;

    /// [`Backend::fetch_block_into`] into a new buffer (probes and tests).
    async fn fetch_block(&self, file: &LinkId, revision: &RevisionId, index: u32) -> Result<Bytes> {
        let mut buf = vec![0; MAX_BLOCK_BYTES];
        let len = self
            .fetch_block_into(file, revision, index, &mut buf)
            .await?;
        buf.truncate(len);
        Ok(Bytes::from(buf))
    }

    /// Creates a file with its first revision.
    async fn upload_new(
        &self,
        parent: &LinkId,
        name: &str,
        source: UploadSource,
        meta: FileMeta,
    ) -> Result<(LinkId, RevisionId)>;

    /// Adds a revision on top of `base`. Fails with
    /// [`BackendError::RevisionConflict`] when `base` is no longer active.
    async fn upload_revision(
        &self,
        file: &LinkId,
        base: &RevisionId,
        source: UploadSource,
        meta: FileMeta,
    ) -> Result<RevisionId>;

    async fn create_folder(&self, parent: &LinkId, name: &str, mtime: SystemTime)
    -> Result<LinkId>;

    /// Renames and/or moves a node. With `expected`, fails with
    /// [`BackendError::NameConflict`] when the current name hash differs.
    async fn rename(
        &self,
        node: &LinkId,
        new_parent: Option<&LinkId>,
        new_name: &str,
        expected: Option<&NameHash>,
    ) -> Result<()>;

    /// Moves nodes to the trash; one result per node, in request order.
    async fn trash(&self, nodes: &[LinkId]) -> Result<Vec<Result<()>>>;

    /// Revisions of a file, oldest first.
    async fn revisions(&self, _file: &LinkId) -> Result<Vec<RevisionMeta>> {
        Err(BackendError::Unsupported("revision list"))
    }

    async fn latest_event_id(&self) -> Result<EventId>;

    async fn poll_events(&self, since: &EventId) -> Result<EventPage>;

    async fn storage_info(&self) -> Result<StorageInfo>;
}
