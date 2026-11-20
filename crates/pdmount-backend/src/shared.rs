//! Shared-with-me (M5.12, D107, D112): the listing of the shares other users
//! gave the user, the core event stream that signals changes to it, and
//! [`ShareTree`], the backend of one share's tree.
//!
//! Share membership, roles and revocations arrive only on the core event
//! stream (`DriveShareRefresh`, M5.9); the content under a share arrives on
//! its owner's volume stream, which the tree's poller follows.
//!
//! A share's root is a folder or a single file. A tree has a folder at its
//! root, so [`ShareTree`] gives a shared file a virtual root folder holding
//! only that file, under a fixed name ([`SHARED_FILE_NAME`]); the mount shows
//! it under the share's name in a virtual folder of its own. Either way the
//! wrapper reports the first "not found" of the share's root, which is how a
//! revocation shows before the core event does (M5.9).

use crate::{
    AccountInfo, Backend, BackendError, BlockInfo, EventId, EventPage, FileMeta, LinkId, Listing,
    NameHash, NodeKind, NodeLookup, RemoteNode, Result, RevisionId, RevisionMeta, StorageInfo,
    UploadSource,
};
use bytes::Bytes;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

/// The name of a shared file in its tree's virtual root folder. The mount
/// shows the file under the share's name instead, so a rename by its owner
/// never reaches the tree.
pub const SHARED_FILE_NAME: &str = "file";

/// The id of a shared file's virtual root folder. Server link ids are
/// base64, so `~` never starts one.
pub const VIRTUAL_ROOT: &str = "~share-root";

/// The user's role in a share, as the server reports it on the share's root.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ShareRole {
    Viewer,
    Editor,
    Admin,
}

impl ShareRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Viewer => "viewer",
            Self::Editor => "editor",
            Self::Admin => "admin",
        }
    }
}

/// One entry of the shared-with-me listing: a share and its root.
///
/// Everything here but the ids comes from the share's creator or the
/// owner's node metadata, so it is untrusted input (D070): the names are
/// decrypted but unchecked, and `owner` is the creator's address as the
/// server has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedEntry {
    pub share: String,
    pub volume: String,
    pub root: LinkId,
    /// A folder, or a single file.
    pub kind: NodeKind,
    /// The root's name.
    pub name: String,
    /// The share's creator.
    pub owner: String,
    pub role: ShareRole,
    /// When the share was created; orders same-name shares (D107).
    pub created: Option<SystemTime>,
}

/// A share of the listing whose root the server returns but whose
/// metadata cannot be decrypted (D119): it is not gone, so a consumer keeps
/// what it has of it (its tree, its name) and logs it; only a share the
/// listing no longer has, or whose root is absent, goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DegradedShare {
    pub share: String,
    pub volume: String,
    pub root: LinkId,
}

/// The shared-with-me listing as [`SharedWithMe::shared_listing`] reads it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SharedListing {
    /// The shares whose roots were read.
    pub entries: Vec<SharedEntry>,
    /// The shares whose roots cannot be read: not in `entries`, not gone.
    pub degraded: Vec<DegradedShare>,
}

/// A page of the core event stream, reduced to what PdMount uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorePage {
    pub next: EventId,
    pub more: bool,
    /// The server asks the client to read everything again.
    pub refresh: bool,
    /// `DriveShareRefresh`: shares, roles or memberships changed.
    pub shares_changed: bool,
}

/// What a Shared mount needs beyond a tree's [`Backend`].
#[async_trait::async_trait]
pub trait SharedWithMe: Send + Sync + 'static {
    /// The shares other users gave the user, folders and files, with their
    /// roots read anew. Shares whose root cannot be read are left out:
    /// [`SharedWithMe::shared_listing`] names them.
    async fn shared_with_me(&self) -> Result<Vec<SharedEntry>>;

    /// [`SharedWithMe::shared_with_me`] with the shares whose root the
    /// server returns but cannot be decrypted, which are not gone (D119).
    /// The default knows none.
    async fn shared_listing(&self) -> Result<SharedListing> {
        Ok(SharedListing {
            entries: self.shared_with_me().await?,
            degraded: Vec::new(),
        })
    }

    async fn latest_core_event_id(&self) -> Result<EventId>;

    async fn poll_core_events(&self, since: &EventId) -> Result<CorePage>;
}

/// Called once, at the first "not found" of a share's root.
pub type OnGone = Arc<dyn Fn() + Send + Sync>;

/// The backend of one share's tree: the share's tree backend, with the
/// first "not found" of the root reported ([`OnGone`]), and for a shared
/// file a virtual root folder that holds only the file. Changes are refused
/// for a shared file (its tree's write gate stays closed anyway, D080).
pub struct ShareTree {
    inner: Arc<dyn Backend>,
    /// The share's root on its volume.
    root: LinkId,
    /// The root is a file: the tree's root is [`VIRTUAL_ROOT`].
    file: bool,
    on_gone: OnGone,
    gone: AtomicBool,
}

impl ShareTree {
    /// `inner` is the tree backend of the share's volume and root.
    pub fn new(inner: Arc<dyn Backend>, root: LinkId, kind: NodeKind, on_gone: OnGone) -> Self {
        Self {
            inner,
            root,
            file: kind == NodeKind::File,
            on_gone,
            gone: AtomicBool::new(false),
        }
    }

    fn virtual_root() -> LinkId {
        LinkId::new(VIRTUAL_ROOT)
    }

    /// Reports the root's "not found" once; passes the result on.
    fn watch<T>(&self, result: Result<T>) -> Result<T> {
        if matches!(result, Err(BackendError::NotFound)) {
            self.gone_now();
        }
        result
    }

    fn gone_now(&self) {
        if !self.gone.swap(true, Ordering::SeqCst) {
            tracing::info!(root = %self.root, "a share's root is not found; the share is gone");
            (self.on_gone)();
        }
    }

    /// The shared file as its tree shows it: in the virtual root, under
    /// [`SHARED_FILE_NAME`].
    fn as_child(node: RemoteNode) -> RemoteNode {
        RemoteNode {
            parent_link_id: Some(Self::virtual_root()),
            name: SHARED_FILE_NAME.to_owned(),
            name_hash: None,
            ..node
        }
    }

    fn folder(mtime: Option<SystemTime>) -> RemoteNode {
        RemoteNode {
            link_id: Self::virtual_root(),
            parent_link_id: None,
            name: String::new(),
            kind: NodeKind::Folder,
            active_revision: None,
            size: None,
            mtime,
            mode: None,
            sha1: None,
            name_hash: None,
            trashed: false,
            unverified: None,
        }
    }

    /// The shared file, fresh; `NotFound` (reported) when it is gone. One
    /// whose metadata cannot be read is not gone (D119): `Transient`, so
    /// the tree keeps what it has and asks again later.
    async fn file_node(&self) -> Result<RemoteNode> {
        let node = match self
            .inner
            .get_nodes_checked(std::slice::from_ref(&self.root))
            .await?
            .pop()
        {
            Some(NodeLookup::Found(node)) => Some(node),
            Some(NodeLookup::Degraded) => return Err(self.degraded()),
            Some(NodeLookup::Absent) | None => None,
        }
        .filter(|node| !node.trashed && node.kind == NodeKind::File)
        .ok_or(BackendError::NotFound);
        self.watch(node)
    }

    /// The root's metadata cannot be read: logged, never "gone" (D119).
    fn degraded(&self) -> BackendError {
        tracing::warn!(
            root = %self.root,
            "a share's root cannot be decrypted; the share is kept as it was"
        );
        BackendError::Transient("the share's root cannot be decrypted".into())
    }

    /// [`Backend::get_nodes_checked`] of a shared file's tree: the virtual
    /// root, the file, and nothing else.
    async fn file_nodes(&self, ids: &[LinkId]) -> Result<Vec<NodeLookup>> {
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            out.push(if id.as_str() == VIRTUAL_ROOT {
                // The virtual root stays when the file is gone.
                match self.file_node().await {
                    Ok(file) => NodeLookup::Found(Self::folder(file.mtime)),
                    Err(BackendError::NotFound) => NodeLookup::Found(Self::folder(None)),
                    Err(error) => return Err(error),
                }
            } else if *id == self.root {
                match self.file_node().await {
                    Ok(file) => NodeLookup::Found(Self::as_child(file)),
                    Err(BackendError::NotFound) => NodeLookup::Absent,
                    Err(error) => return Err(error),
                }
            } else {
                NodeLookup::Absent
            });
        }
        Ok(out)
    }

    fn refuse(&self) -> BackendError {
        BackendError::PermissionDenied
    }
}

#[async_trait::async_trait]
impl Backend for ShareTree {
    async fn account(&self) -> Result<AccountInfo> {
        let account = self.inner.account().await?;
        Ok(if self.file {
            AccountInfo {
                root: Self::virtual_root(),
                ..account
            }
        } else {
            account
        })
    }

    fn upload_block_size(&self) -> u64 {
        self.inner.upload_block_size()
    }

    async fn root(&self) -> Result<RemoteNode> {
        if self.file {
            let file = self.file_node().await?;
            return Ok(Self::folder(file.mtime));
        }
        let root = self.inner.root().await;
        self.watch(root)
    }

    async fn list_children(&self, folder: &LinkId) -> Result<Vec<RemoteNode>> {
        if self.file {
            if folder.as_str() != VIRTUAL_ROOT {
                return Err(BackendError::NotFound);
            }
            return Ok(vec![Self::as_child(self.file_node().await?)]);
        }
        let children = self.inner.list_children(folder).await;
        if *folder == self.root {
            self.watch(children)
        } else {
            children
        }
    }

    async fn list_children_checked(&self, folder: &LinkId) -> Result<Listing> {
        if self.file {
            return Ok(Listing {
                children: self.list_children(folder).await?,
                degraded: Vec::new(),
            });
        }
        let listing = self.inner.list_children_checked(folder).await;
        if *folder == self.root {
            self.watch(listing)
        } else {
            listing
        }
    }

    async fn get_nodes(&self, ids: &[LinkId]) -> Result<Vec<Option<RemoteNode>>> {
        Ok(self
            .get_nodes_checked(ids)
            .await?
            .into_iter()
            .map(NodeLookup::found)
            .collect())
    }

    /// A root the server does not return is gone; one it returns but
    /// whose metadata cannot be read is not (D119).
    async fn get_nodes_checked(&self, ids: &[LinkId]) -> Result<Vec<NodeLookup>> {
        if self.file {
            return self.file_nodes(ids).await;
        }
        let nodes = self.inner.get_nodes_checked(ids).await?;
        for (id, node) in ids.iter().zip(&nodes) {
            if *id != self.root {
                continue;
            }
            match node {
                NodeLookup::Absent => self.gone_now(),
                NodeLookup::Degraded => {
                    self.degraded();
                }
                NodeLookup::Found(_) => {}
            }
        }
        Ok(nodes)
    }

    async fn blocks(&self, file: &LinkId, revision: &RevisionId) -> Result<Vec<BlockInfo>> {
        self.inner.blocks(file, revision).await
    }

    async fn fetch_block_into(
        &self,
        file: &LinkId,
        revision: &RevisionId,
        index: u32,
        buf: &mut [u8],
    ) -> Result<usize> {
        self.inner
            .fetch_block_into(file, revision, index, buf)
            .await
    }

    async fn fetch_block(&self, file: &LinkId, revision: &RevisionId, index: u32) -> Result<Bytes> {
        self.inner.fetch_block(file, revision, index).await
    }

    async fn upload_new(
        &self,
        parent: &LinkId,
        name: &str,
        source: UploadSource,
        meta: FileMeta,
    ) -> Result<(LinkId, RevisionId)> {
        if self.file {
            return Err(self.refuse());
        }
        self.inner.upload_new(parent, name, source, meta).await
    }

    async fn upload_revision(
        &self,
        file: &LinkId,
        base: &RevisionId,
        source: UploadSource,
        meta: FileMeta,
    ) -> Result<RevisionId> {
        if self.file {
            return Err(self.refuse());
        }
        self.inner.upload_revision(file, base, source, meta).await
    }

    async fn create_folder(
        &self,
        parent: &LinkId,
        name: &str,
        mtime: SystemTime,
    ) -> Result<LinkId> {
        if self.file {
            return Err(self.refuse());
        }
        self.inner.create_folder(parent, name, mtime).await
    }

    async fn rename(
        &self,
        node: &LinkId,
        new_parent: Option<&LinkId>,
        new_name: &str,
        expected: Option<&NameHash>,
    ) -> Result<()> {
        if self.file {
            return Err(self.refuse());
        }
        self.inner
            .rename(node, new_parent, new_name, expected)
            .await
    }

    async fn trash(&self, nodes: &[LinkId]) -> Result<Vec<Result<()>>> {
        if self.file {
            return Err(self.refuse());
        }
        self.inner.trash(nodes).await
    }

    async fn revisions(&self, file: &LinkId) -> Result<Vec<RevisionMeta>> {
        self.inner.revisions(file).await
    }

    async fn latest_event_id(&self) -> Result<EventId> {
        self.inner.latest_event_id().await
    }

    /// The owner's volume stream. For a shared file only the file's own
    /// events pass, placed in the virtual root. When the stream is refused
    /// (not found, permission denied), the root is checked: a revoked
    /// share is reported at once instead of failing every poll.
    async fn poll_events(&self, since: &EventId) -> Result<EventPage> {
        let polled = self.inner.poll_events(since).await;
        if let Err(BackendError::NotFound | BackendError::PermissionDenied) = &polled
            && let Ok(nodes) = self.inner.get_nodes(std::slice::from_ref(&self.root)).await
            && nodes.first().is_some_and(Option::is_none)
        {
            self.gone_now();
        }
        let mut page = polled?;
        if self.file {
            page.events.retain(|event| event.link_id == self.root);
            for event in &mut page.events {
                event.parent_link_id = Some(Self::virtual_root());
            }
        }
        Ok(page)
    }

    async fn storage_info(&self) -> Result<StorageInfo> {
        self.inner.storage_info().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::{FakeBackend, FakeRole};
    use std::sync::atomic::AtomicUsize;

    fn counter() -> (Arc<AtomicUsize>, OnGone) {
        let count = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&count);
        (
            count,
            Arc::new(move || {
                seen.fetch_add(1, Ordering::SeqCst);
            }),
        )
    }

    #[tokio::test]
    async fn a_shared_file_sits_alone_in_a_virtual_root() {
        let fake = FakeBackend::new();
        let added = fake
            .add_shared_file(
                "report.pdf",
                b"content",
                "owner@example.test",
                FakeRole::Viewer,
            )
            .await
            .expect("share");
        let (gone, on_gone) = counter();
        let tree = ShareTree::new(
            Arc::new(fake.tree(&added.volume, added.root.clone())),
            added.root.clone(),
            NodeKind::File,
            on_gone,
        );
        let root = tree.root().await.expect("root");
        assert_eq!(root.link_id.as_str(), VIRTUAL_ROOT);
        assert_eq!(root.kind, NodeKind::Folder);
        assert_eq!(
            tree.account().await.expect("account").root.as_str(),
            VIRTUAL_ROOT
        );
        let children = tree
            .list_children(&ShareTree::virtual_root())
            .await
            .expect("children");
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].name, SHARED_FILE_NAME);
        assert_eq!(children[0].link_id, added.root);
        assert_eq!(
            children[0].parent_link_id.as_ref().map(LinkId::as_str),
            Some(VIRTUAL_ROOT)
        );
        let revision = children[0].active_revision.clone().expect("revision");
        let block = tree
            .fetch_block(&added.root, &revision, 1)
            .await
            .expect("block");
        assert_eq!(&block[..], b"content");
        // Changes are refused.
        assert_eq!(
            tree.create_folder(&ShareTree::virtual_root(), "x", SystemTime::now())
                .await,
            Err(BackendError::PermissionDenied)
        );
        assert_eq!(gone.load(Ordering::SeqCst), 0);

        // Revoked: the root reads as not found, reported once.
        fake.set_member_role(
            added.share.as_deref().expect("share"),
            "fake@example.test",
            None,
        )
        .expect("revoke");
        assert_eq!(
            tree.list_children(&ShareTree::virtual_root()).await,
            Err(BackendError::NotFound)
        );
        assert!(tree.root().await.is_err());
        assert_eq!(gone.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_folder_share_reports_its_roots_first_not_found() {
        let fake = FakeBackend::new();
        let added = fake
            .add_root("Team", Some("owner@example.test"), Some(FakeRole::Viewer))
            .expect("share");
        let (gone, on_gone) = counter();
        let tree = ShareTree::new(
            Arc::new(fake.tree(&added.volume, added.root.clone())),
            added.root.clone(),
            NodeKind::Folder,
            on_gone,
        );
        tree.list_children(&added.root).await.expect("listed");
        tree.get_nodes(std::slice::from_ref(&added.root))
            .await
            .expect("nodes");
        // A missing node below the root says nothing about the share.
        tree.get_nodes(&[LinkId::new("link-none")])
            .await
            .expect("nodes");
        assert_eq!(gone.load(Ordering::SeqCst), 0);
        fake.set_member_role(
            added.share.as_deref().expect("share"),
            "fake@example.test",
            None,
        )
        .expect("revoke");
        // A refused event stream checks the root.
        fake.inject_failure(crate::fake::FakeOp::PollEvents, BackendError::NotFound);
        let since = tree.latest_event_id().await.expect("cursor");
        assert!(tree.poll_events(&since).await.is_err());
        assert_eq!(gone.load(Ordering::SeqCst), 1);
        let nodes = tree
            .get_nodes(std::slice::from_ref(&added.root))
            .await
            .expect("nodes");
        assert_eq!(nodes, [None]);
        assert!(tree.list_children(&added.root).await.is_err());
        assert_eq!(gone.load(Ordering::SeqCst), 1);
    }

    /// A root the server returns but cannot decrypt (a name its owner
    /// made unreadable, say) is not gone: neither the tree nor the listing
    /// lets it look so (D119).
    #[tokio::test]
    async fn a_root_that_cannot_be_decrypted_is_not_gone() {
        let fake = FakeBackend::new();
        let added = fake
            .add_root("Team", Some("owner@example.test"), Some(FakeRole::Viewer))
            .expect("share");
        let (gone, on_gone) = counter();
        let tree = ShareTree::new(
            Arc::new(fake.tree(&added.volume, added.root.clone())),
            added.root.clone(),
            NodeKind::Folder,
            on_gone,
        );
        fake.set_degraded(&added.root, true);
        let nodes = tree
            .get_nodes_checked(std::slice::from_ref(&added.root))
            .await
            .expect("nodes");
        assert_eq!(nodes, [NodeLookup::Degraded]);
        assert_eq!(
            tree.get_nodes(std::slice::from_ref(&added.root))
                .await
                .expect("nodes"),
            [None]
        );
        assert_eq!(
            gone.load(Ordering::SeqCst),
            0,
            "a degraded root is not gone"
        );
        let listing = fake.shared_listing().await.expect("listing");
        assert!(listing.entries.is_empty());
        assert_eq!(
            listing.degraded,
            [DegradedShare {
                share: added.share.clone().expect("share"),
                volume: added.volume.clone(),
                root: added.root.clone(),
            }]
        );
        // Readable again: listed again.
        fake.set_degraded(&added.root, false);
        let listing = fake.shared_listing().await.expect("listing");
        assert_eq!((listing.entries.len(), listing.degraded.len()), (1, 0));
        assert_eq!(gone.load(Ordering::SeqCst), 0);
        // Absent is gone.
        fake.set_member_role(
            added.share.as_deref().expect("share"),
            "fake@example.test",
            None,
        )
        .expect("revoke");
        tree.get_nodes(std::slice::from_ref(&added.root))
            .await
            .expect("nodes");
        assert_eq!(gone.load(Ordering::SeqCst), 1);
    }

    /// A shared file whose metadata cannot be decrypted is a transient
    /// failure of its tree, not a gone share (D119).
    #[tokio::test]
    async fn a_shared_file_that_cannot_be_decrypted_is_not_gone() {
        let fake = FakeBackend::new();
        let added = fake
            .add_shared_file(
                "report.pdf",
                b"content",
                "owner@example.test",
                FakeRole::Viewer,
            )
            .await
            .expect("share");
        let (gone, on_gone) = counter();
        let tree = ShareTree::new(
            Arc::new(fake.tree(&added.volume, added.root.clone())),
            added.root.clone(),
            NodeKind::File,
            on_gone,
        );
        fake.set_degraded(&added.root, true);
        assert!(matches!(tree.root().await, Err(BackendError::Transient(_))));
        assert!(matches!(
            tree.get_nodes(std::slice::from_ref(&added.root)).await,
            Err(BackendError::Transient(_))
        ));
        assert_eq!(gone.load(Ordering::SeqCst), 0);
    }
}
