//! In-memory model of Proton Drive "My Files" for tests (DESIGN.md §15.1).
//!
//! It enforces the server rules PdMount relies on: unique names per folder,
//! the base-revision precondition of new revisions, the name-hash
//! precondition of renames, trash semantics, quota, and a paged event
//! stream with `more` and `refresh`. Clones share one drive, so two clones
//! act as two devices. One-shot failures can be injected per operation.

use crate::{
    AccountInfo, Backend, BackendError, BlockInfo, EventId, EventKind, EventPage, FileMeta, LinkId,
    NameHash, NodeKind, RemoteEvent, RemoteNode, Result, RevisionId, StorageInfo, UploadSource,
};
use bytes::Bytes;
use parking_lot::Mutex;
use sha1::Digest;
use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::time::SystemTime;

/// Operations of the [`Backend`] trait, for fault injection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FakeOp {
    Account,
    Root,
    ListChildren,
    GetNodes,
    Blocks,
    FetchBlock,
    UploadNew,
    UploadRevision,
    CreateFolder,
    Rename,
    Trash,
    LatestEventId,
    PollEvents,
    StorageInfo,
}

/// A remote change applied by the fake, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FakeChange {
    CreatedFile { link: LinkId, name: String },
    CreatedFolder { link: LinkId, name: String },
    NewRevision { link: LinkId, revision: RevisionId },
    Renamed { link: LinkId, name: String },
    Trashed { link: LinkId },
}

#[derive(Debug, Clone)]
struct Revision {
    id: RevisionId,
    content: Bytes,
    mtime: SystemTime,
    mode: Option<u32>,
}

#[derive(Debug, Clone)]
struct Node {
    link: LinkId,
    parent: Option<LinkId>,
    name: String,
    kind: NodeKind,
    trashed: bool,
    folder_mtime: Option<SystemTime>,
    revisions: Vec<Revision>,
}

impl Node {
    fn active(&self) -> Option<&Revision> {
        self.revisions.last()
    }

    fn to_remote(&self) -> RemoteNode {
        let active = self.active();
        RemoteNode {
            link_id: self.link.clone(),
            parent_link_id: self.parent.clone(),
            name: self.name.clone(),
            kind: self.kind,
            active_revision: active.map(|r| r.id.clone()),
            size: active.map(|r| r.content.len() as u64),
            mtime: active.map(|r| r.mtime).or(self.folder_mtime),
            mode: active.and_then(|r| r.mode),
            sha1: active.map(|r| sha1::Sha1::digest(&r.content).into()),
            name_hash: Some(name_hash(&self.name)),
            trashed: self.trashed,
        }
    }
}

fn name_hash(name: &str) -> NameHash {
    NameHash::new(format!("fake:{name}"))
}

struct State {
    username: String,
    root: LinkId,
    nodes: BTreeMap<LinkId, Node>,
    events: Vec<RemoteEvent>,
    changes: Vec<FakeChange>,
    next_id: u64,
    quota_max: u64,
    block_size: usize,
    event_page_size: usize,
    refresh_pending: bool,
    faults: VecDeque<(FakeOp, BackendError)>,
}

impl State {
    fn new_id(&mut self, prefix: &str) -> String {
        self.next_id += 1;
        format!("{prefix}{}", self.next_id)
    }

    fn take_fault(&mut self, op: FakeOp) -> Result<()> {
        let position = self.faults.iter().position(|(o, _)| *o == op);
        match position.and_then(|i| self.faults.remove(i)) {
            Some((_, error)) => Err(error),
            None => Ok(()),
        }
    }

    fn node(&self, link: &LinkId) -> Result<&Node> {
        self.nodes
            .get(link)
            .filter(|n| !n.trashed)
            .ok_or(BackendError::NotFound)
    }

    fn folder(&self, link: &LinkId) -> Result<&Node> {
        let node = self.node(link)?;
        match node.kind {
            NodeKind::Folder => Ok(node),
            NodeKind::File => Err(BackendError::Other(format!("{link} is not a folder"))),
        }
    }

    fn child_named(&self, parent: &LinkId, name: &str) -> Option<&Node> {
        self.nodes
            .values()
            .find(|n| !n.trashed && n.parent.as_ref() == Some(parent) && n.name == name)
    }

    fn ensure_name_free(&self, parent: &LinkId, name: &str, except: Option<&LinkId>) -> Result<()> {
        match self.child_named(parent, name) {
            Some(existing) if Some(&existing.link) != except => Err(BackendError::AlreadyExists {
                existing: Some(existing.link.clone()),
            }),
            _ => Ok(()),
        }
    }

    fn used(&self) -> u64 {
        self.nodes
            .values()
            .flat_map(|n| &n.revisions)
            .map(|r| r.content.len() as u64)
            .sum()
    }

    fn ensure_quota(&self, extra: u64) -> Result<()> {
        if self.used() + extra > self.quota_max {
            Err(BackendError::QuotaExceeded)
        } else {
            Ok(())
        }
    }

    fn push_event(&mut self, kind: EventKind, link: &LinkId) {
        let node = &self.nodes[link];
        self.events.push(RemoteEvent {
            kind,
            link_id: link.clone(),
            parent_link_id: node.parent.clone(),
            trashed: node.trashed,
        });
    }

    fn is_descendant(&self, node: &LinkId, ancestor: &LinkId) -> bool {
        let mut current = Some(node.clone());
        while let Some(link) = current {
            if &link == ancestor {
                return true;
            }
            current = self.nodes.get(&link).and_then(|n| n.parent.clone());
        }
        false
    }
}

/// In-memory Proton Drive. `Clone` shares the drive.
#[derive(Clone)]
pub struct FakeBackend {
    state: Arc<Mutex<State>>,
}

impl Default for FakeBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeBackend {
    pub const DEFAULT_BLOCK_SIZE: usize = 4 << 20;

    pub fn new() -> Self {
        let root = LinkId::new("root");
        let mut nodes = BTreeMap::new();
        nodes.insert(
            root.clone(),
            Node {
                link: root.clone(),
                parent: None,
                name: String::new(),
                kind: NodeKind::Folder,
                trashed: false,
                folder_mtime: None,
                revisions: Vec::new(),
            },
        );
        Self {
            state: Arc::new(Mutex::new(State {
                username: "fake@example.test".into(),
                root,
                nodes,
                events: Vec::new(),
                changes: Vec::new(),
                next_id: 0,
                quota_max: 5 << 30,
                block_size: Self::DEFAULT_BLOCK_SIZE,
                event_page_size: 50,
                refresh_pending: false,
                faults: VecDeque::new(),
            })),
        }
    }

    /// Makes the next call of `op` fail with `error` (one-shot, FIFO).
    pub fn inject_failure(&self, op: FakeOp, error: BackendError) {
        self.state.lock().faults.push_back((op, error));
    }

    /// Makes the next event poll answer `refresh = true`.
    pub fn request_refresh(&self) {
        self.state.lock().refresh_pending = true;
    }

    pub fn set_quota(&self, max: u64) {
        self.state.lock().quota_max = max;
    }

    pub fn set_block_size(&self, block_size: usize) {
        assert!(block_size > 0, "block size must be positive");
        self.state.lock().block_size = block_size;
    }

    pub fn set_event_page_size(&self, page_size: usize) {
        assert!(page_size > 0, "event page size must be positive");
        self.state.lock().event_page_size = page_size;
    }

    /// Content of the active revision of a file.
    pub fn content(&self, file: &LinkId) -> Option<Bytes> {
        let state = self.state.lock();
        state.nodes.get(file)?.active().map(|r| r.content.clone())
    }

    pub fn revision_count(&self, file: &LinkId) -> usize {
        self.state
            .lock()
            .nodes
            .get(file)
            .map_or(0, |n| n.revisions.len())
    }

    /// Every change applied so far, in order.
    pub fn changes(&self) -> Vec<FakeChange> {
        self.state.lock().changes.clone()
    }

    fn with_state<T>(&self, op: FakeOp, f: impl FnOnce(&mut State) -> Result<T>) -> Result<T> {
        let mut state = self.state.lock();
        state.take_fault(op)?;
        f(&mut state)
    }
}

async fn read_source(source: UploadSource) -> Result<Bytes> {
    match source {
        UploadSource::Bytes(bytes) => Ok(bytes),
        UploadSource::File(path) => tokio::fs::read(&path)
            .await
            .map(Bytes::from)
            .map_err(|e| BackendError::Other(format!("cannot read {}: {e}", path.display()))),
    }
}

#[async_trait::async_trait]
impl Backend for FakeBackend {
    async fn account(&self) -> Result<AccountInfo> {
        self.with_state(FakeOp::Account, |s| {
            Ok(AccountInfo {
                username: s.username.clone(),
                root: s.root.clone(),
            })
        })
    }

    async fn root(&self) -> Result<RemoteNode> {
        self.with_state(FakeOp::Root, |s| Ok(s.nodes[&s.root].to_remote()))
    }

    async fn list_children(&self, folder: &LinkId) -> Result<Vec<RemoteNode>> {
        self.with_state(FakeOp::ListChildren, |s| {
            s.folder(folder)?;
            let mut children: Vec<_> = s
                .nodes
                .values()
                .filter(|n| !n.trashed && n.parent.as_ref() == Some(folder))
                .map(Node::to_remote)
                .collect();
            children.sort_by(|a, b| a.name.cmp(&b.name));
            Ok(children)
        })
    }

    async fn get_nodes(&self, ids: &[LinkId]) -> Result<Vec<Option<RemoteNode>>> {
        self.with_state(FakeOp::GetNodes, |s| {
            Ok(ids
                .iter()
                .map(|id| s.nodes.get(id).map(Node::to_remote))
                .collect())
        })
    }

    async fn blocks(&self, file: &LinkId, revision: &RevisionId) -> Result<Vec<BlockInfo>> {
        self.with_state(FakeOp::Blocks, |s| {
            let node = s.nodes.get(file).ok_or(BackendError::NotFound)?;
            let rev = node
                .revisions
                .iter()
                .find(|r| &r.id == revision)
                .ok_or(BackendError::NotFound)?;
            Ok(rev
                .content
                .chunks(s.block_size)
                .zip(1..)
                .map(|(chunk, index)| BlockInfo {
                    index,
                    plain_size: Some(chunk.len() as u32),
                })
                .collect())
        })
    }

    async fn fetch_block(&self, file: &LinkId, revision: &RevisionId, index: u32) -> Result<Bytes> {
        self.with_state(FakeOp::FetchBlock, |s| {
            let node = s.nodes.get(file).ok_or(BackendError::NotFound)?;
            let rev = node
                .revisions
                .iter()
                .find(|r| &r.id == revision)
                .ok_or(BackendError::NotFound)?;
            let start = (index as usize)
                .checked_sub(1)
                .map(|i| i * s.block_size)
                .filter(|&start| start < rev.content.len())
                .ok_or(BackendError::NotFound)?;
            let end = (start + s.block_size).min(rev.content.len());
            Ok(rev.content.slice(start..end))
        })
    }

    async fn upload_new(
        &self,
        parent: &LinkId,
        name: &str,
        source: UploadSource,
        meta: FileMeta,
    ) -> Result<(LinkId, RevisionId)> {
        let content = read_source(source).await?;
        self.with_state(FakeOp::UploadNew, |s| {
            s.folder(parent)?;
            s.ensure_name_free(parent, name, None)?;
            s.ensure_quota(content.len() as u64)?;
            let link = LinkId::new(s.new_id("link-"));
            let revision = RevisionId::new(s.new_id("rev-"));
            s.nodes.insert(
                link.clone(),
                Node {
                    link: link.clone(),
                    parent: Some(parent.clone()),
                    name: name.to_owned(),
                    kind: NodeKind::File,
                    trashed: false,
                    folder_mtime: None,
                    revisions: vec![Revision {
                        id: revision.clone(),
                        content,
                        mtime: meta.mtime,
                        mode: meta.mode,
                    }],
                },
            );
            s.push_event(EventKind::Create, &link);
            s.changes.push(FakeChange::CreatedFile {
                link: link.clone(),
                name: name.to_owned(),
            });
            Ok((link, revision))
        })
    }

    async fn upload_revision(
        &self,
        file: &LinkId,
        base: &RevisionId,
        source: UploadSource,
        meta: FileMeta,
    ) -> Result<RevisionId> {
        let content = read_source(source).await?;
        self.with_state(FakeOp::UploadRevision, |s| {
            let node = s.node(file)?;
            if node.kind != NodeKind::File {
                return Err(BackendError::Other(format!("{file} is not a file")));
            }
            let current = node.active().map(|r| r.id.clone());
            if current.as_ref() != Some(base) {
                return Err(BackendError::RevisionConflict { current });
            }
            s.ensure_quota(content.len() as u64)?;
            let revision = RevisionId::new(s.new_id("rev-"));
            if let Some(node) = s.nodes.get_mut(file) {
                node.revisions.push(Revision {
                    id: revision.clone(),
                    content,
                    mtime: meta.mtime,
                    mode: meta.mode,
                });
            }
            s.push_event(EventKind::UpdateContent, file);
            s.changes.push(FakeChange::NewRevision {
                link: file.clone(),
                revision: revision.clone(),
            });
            Ok(revision)
        })
    }

    async fn create_folder(
        &self,
        parent: &LinkId,
        name: &str,
        mtime: SystemTime,
    ) -> Result<LinkId> {
        self.with_state(FakeOp::CreateFolder, |s| {
            s.folder(parent)?;
            s.ensure_name_free(parent, name, None)?;
            let link = LinkId::new(s.new_id("link-"));
            s.nodes.insert(
                link.clone(),
                Node {
                    link: link.clone(),
                    parent: Some(parent.clone()),
                    name: name.to_owned(),
                    kind: NodeKind::Folder,
                    trashed: false,
                    folder_mtime: Some(mtime),
                    revisions: Vec::new(),
                },
            );
            s.push_event(EventKind::Create, &link);
            s.changes.push(FakeChange::CreatedFolder {
                link: link.clone(),
                name: name.to_owned(),
            });
            Ok(link)
        })
    }

    async fn rename(
        &self,
        node: &LinkId,
        new_parent: Option<&LinkId>,
        new_name: &str,
        expected: Option<&NameHash>,
    ) -> Result<()> {
        self.with_state(FakeOp::Rename, |s| {
            if node == &s.root {
                return Err(BackendError::PermissionDenied);
            }
            let current = s.node(node)?;
            if expected.is_some_and(|hash| *hash != name_hash(&current.name)) {
                return Err(BackendError::NameConflict);
            }
            let parent = match new_parent {
                Some(parent) => parent.clone(),
                None => current
                    .parent
                    .clone()
                    .ok_or(BackendError::PermissionDenied)?,
            };
            s.folder(&parent)?;
            if s.is_descendant(&parent, node) {
                return Err(BackendError::Other(
                    "cannot move a folder into itself".into(),
                ));
            }
            s.ensure_name_free(&parent, new_name, Some(node))?;
            if let Some(entry) = s.nodes.get_mut(node) {
                entry.parent = Some(parent);
                entry.name = new_name.to_owned();
            }
            s.push_event(EventKind::UpdateMetadata, node);
            s.changes.push(FakeChange::Renamed {
                link: node.clone(),
                name: new_name.to_owned(),
            });
            Ok(())
        })
    }

    async fn trash(&self, nodes: &[LinkId]) -> Result<Vec<Result<()>>> {
        self.with_state(FakeOp::Trash, |s| {
            Ok(nodes
                .iter()
                .map(|link| {
                    if link == &s.root {
                        return Err(BackendError::PermissionDenied);
                    }
                    s.node(link)?;
                    if let Some(entry) = s.nodes.get_mut(link) {
                        entry.trashed = true;
                    }
                    s.push_event(EventKind::Delete, link);
                    s.changes.push(FakeChange::Trashed { link: link.clone() });
                    Ok(())
                })
                .collect())
        })
    }

    async fn latest_event_id(&self) -> Result<EventId> {
        self.with_state(FakeOp::LatestEventId, |s| {
            Ok(EventId::new(s.events.len().to_string()))
        })
    }

    async fn poll_events(&self, since: &EventId) -> Result<EventPage> {
        self.with_state(FakeOp::PollEvents, |s| {
            let start: usize = since
                .as_str()
                .parse()
                .ok()
                .filter(|&i| i <= s.events.len())
                .ok_or_else(|| BackendError::Other(format!("unknown event id {since}")))?;
            if std::mem::take(&mut s.refresh_pending) {
                return Ok(EventPage {
                    next: EventId::new(s.events.len().to_string()),
                    more: false,
                    refresh: true,
                    events: Vec::new(),
                });
            }
            let end = (start + s.event_page_size).min(s.events.len());
            Ok(EventPage {
                next: EventId::new(end.to_string()),
                more: end < s.events.len(),
                refresh: false,
                events: s.events[start..end].to_vec(),
            })
        })
    }

    async fn storage_info(&self) -> Result<StorageInfo> {
        self.with_state(FakeOp::StorageInfo, |s| {
            Ok(StorageInfo {
                used: s.used(),
                max: s.quota_max,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn meta() -> FileMeta {
        FileMeta {
            mtime: SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            mode: Some(0o644),
            media_type: "application/octet-stream".into(),
        }
    }

    fn bytes(data: &'static [u8]) -> UploadSource {
        UploadSource::Bytes(Bytes::from_static(data))
    }

    async fn root(fake: &FakeBackend) -> LinkId {
        fake.root().await.expect("root").link_id
    }

    #[tokio::test]
    async fn files_and_folders_are_listed_with_metadata() {
        let fake = FakeBackend::new();
        let root = root(&fake).await;
        let docs = fake
            .create_folder(&root, "docs", SystemTime::UNIX_EPOCH)
            .await
            .expect("mkdir");
        let (file, rev) = fake
            .upload_new(&docs, "a.txt", bytes(b"hello"), meta())
            .await
            .expect("upload");

        let children = fake.list_children(&docs).await.expect("list");
        assert_eq!(children.len(), 1);
        let node = &children[0];
        assert_eq!(node.name, "a.txt");
        assert_eq!(node.kind, NodeKind::File);
        assert_eq!(node.active_revision.as_ref(), Some(&rev));
        assert_eq!(node.size, Some(5));
        assert_eq!(node.mtime, Some(meta().mtime));
        assert_eq!(node.mode, Some(0o644));
        assert_eq!(node.sha1, Some(sha1::Sha1::digest(b"hello").into()));
        assert_eq!(node.parent_link_id.as_ref(), Some(&docs));

        let root_children = fake.list_children(&root).await.expect("list root");
        assert_eq!(root_children[0].kind, NodeKind::Folder);
        let fetched = fake
            .get_nodes(&[file, LinkId::new("missing")])
            .await
            .expect("get");
        assert!(fetched[0].is_some() && fetched[1].is_none());
    }

    #[tokio::test]
    async fn names_are_unique_per_folder() {
        let fake = FakeBackend::new();
        let root = root(&fake).await;
        let (first, _) = fake
            .upload_new(&root, "x", bytes(b"1"), meta())
            .await
            .expect("upload");
        let error = fake
            .upload_new(&root, "x", bytes(b"2"), meta())
            .await
            .unwrap_err();
        assert_eq!(
            error,
            BackendError::AlreadyExists {
                existing: Some(first)
            }
        );
        assert!(matches!(
            fake.create_folder(&root, "x", SystemTime::UNIX_EPOCH).await,
            Err(BackendError::AlreadyExists { .. })
        ));
    }

    #[tokio::test]
    async fn new_revisions_require_the_active_base() {
        let fake = FakeBackend::new();
        let root = root(&fake).await;
        let (file, first) = fake
            .upload_new(&root, "f", bytes(b"v1"), meta())
            .await
            .expect("upload");
        let second = fake
            .upload_revision(&file, &first, bytes(b"v2"), meta())
            .await
            .expect("revision");
        let stale = fake
            .upload_revision(&file, &first, bytes(b"v3"), meta())
            .await
            .unwrap_err();
        assert_eq!(
            stale,
            BackendError::RevisionConflict {
                current: Some(second)
            }
        );
        assert_eq!(fake.content(&file).as_deref(), Some(&b"v2"[..]));
        assert_eq!(fake.revision_count(&file), 2);
    }

    #[tokio::test]
    async fn rename_checks_name_hash_target_and_cycles() {
        let fake = FakeBackend::new();
        let root = root(&fake).await;
        let a = fake
            .create_folder(&root, "a", SystemTime::UNIX_EPOCH)
            .await
            .expect("a");
        let b = fake
            .create_folder(&a, "b", SystemTime::UNIX_EPOCH)
            .await
            .expect("b");
        let (file, _) = fake
            .upload_new(&root, "f", bytes(b"x"), meta())
            .await
            .expect("f");

        let hash = fake
            .get_nodes(std::slice::from_ref(&file))
            .await
            .expect("get")[0]
            .as_ref()
            .and_then(|n| n.name_hash.clone())
            .expect("hash");
        fake.rename(&file, Some(&b), "g", Some(&hash))
            .await
            .expect("move and rename");
        assert_eq!(fake.list_children(&b).await.expect("list")[0].name, "g");
        assert_eq!(
            fake.rename(&file, None, "h", Some(&hash)).await,
            Err(BackendError::NameConflict)
        );
        assert!(fake.rename(&a, Some(&b), "a", None).await.is_err());
        assert_eq!(
            fake.rename(&root, None, "x", None).await,
            Err(BackendError::PermissionDenied)
        );
    }

    #[tokio::test]
    async fn trash_reports_per_node_results_and_hides_nodes() {
        let fake = FakeBackend::new();
        let root = root(&fake).await;
        let (file, _) = fake
            .upload_new(&root, "f", bytes(b"x"), meta())
            .await
            .expect("f");
        let results = fake
            .trash(&[file.clone(), LinkId::new("missing")])
            .await
            .expect("trash");
        assert_eq!(results, vec![Ok(()), Err(BackendError::NotFound)]);
        assert!(fake.list_children(&root).await.expect("list").is_empty());
        let trashed = fake.get_nodes(&[file]).await.expect("get");
        assert!(trashed[0].as_ref().is_some_and(|n| n.trashed));
        // The name is free again once the old node is trashed.
        fake.upload_new(&root, "f", bytes(b"y"), meta())
            .await
            .expect("reuse name");
    }

    #[tokio::test]
    async fn events_are_paged_and_refresh_can_be_requested() {
        let fake = FakeBackend::new();
        fake.set_event_page_size(2);
        let root = root(&fake).await;
        let cursor = fake.latest_event_id().await.expect("cursor");
        let (file, rev) = fake
            .upload_new(&root, "f", bytes(b"1"), meta())
            .await
            .expect("f");
        fake.upload_revision(&file, &rev, bytes(b"2"), meta())
            .await
            .expect("rev");
        fake.rename(&file, None, "g", None).await.expect("rename");

        let first = fake.poll_events(&cursor).await.expect("poll");
        assert!(first.more && !first.refresh);
        let kinds: Vec<_> = first.events.iter().map(|e| e.kind).collect();
        assert_eq!(kinds, [EventKind::Create, EventKind::UpdateContent]);
        let second = fake.poll_events(&first.next).await.expect("poll");
        assert!(!second.more);
        assert_eq!(second.events[0].kind, EventKind::UpdateMetadata);
        assert_eq!(second.events[0].parent_link_id.as_ref(), Some(&root));
        assert!(
            fake.poll_events(&second.next)
                .await
                .expect("poll")
                .events
                .is_empty()
        );

        fake.request_refresh();
        assert!(fake.poll_events(&second.next).await.expect("poll").refresh);
        assert!(!fake.poll_events(&second.next).await.expect("poll").refresh);
        assert!(fake.poll_events(&EventId::new("999")).await.is_err());
    }

    #[tokio::test]
    async fn blocks_split_content_at_the_block_size() {
        let fake = FakeBackend::new();
        fake.set_block_size(4);
        let root = root(&fake).await;
        let (file, rev) = fake
            .upload_new(&root, "f", bytes(b"0123456789"), meta())
            .await
            .expect("f");
        let blocks = fake.blocks(&file, &rev).await.expect("blocks");
        let sizes: Vec<_> = blocks.iter().map(|b| (b.index, b.plain_size)).collect();
        assert_eq!(sizes, [(1, Some(4)), (2, Some(4)), (3, Some(2))]);
        assert_eq!(
            fake.fetch_block(&file, &rev, 3).await.expect("block"),
            &b"89"[..]
        );
        assert_eq!(
            fake.fetch_block(&file, &rev, 4).await,
            Err(BackendError::NotFound)
        );
        assert_eq!(
            fake.fetch_block(&file, &rev, 0).await,
            Err(BackendError::NotFound)
        );
    }

    #[tokio::test]
    async fn quota_counts_every_revision() {
        let fake = FakeBackend::new();
        fake.set_quota(5);
        let root = root(&fake).await;
        let (file, rev) = fake
            .upload_new(&root, "f", bytes(b"123"), meta())
            .await
            .expect("f");
        assert_eq!(
            fake.upload_revision(&file, &rev, bytes(b"456"), meta())
                .await,
            Err(BackendError::QuotaExceeded)
        );
        assert_eq!(
            fake.storage_info().await.expect("info"),
            StorageInfo { used: 3, max: 5 }
        );
    }

    #[tokio::test]
    async fn injected_failures_fire_once_for_their_operation() {
        let fake = FakeBackend::new();
        fake.inject_failure(FakeOp::ListChildren, BackendError::Transient("503".into()));
        let root = root(&fake).await;
        assert_eq!(
            fake.list_children(&root).await,
            Err(BackendError::Transient("503".into()))
        );
        assert!(fake.list_children(&root).await.is_ok());
    }

    #[tokio::test]
    async fn clones_share_one_drive_like_two_devices() {
        let device_a = FakeBackend::new();
        let device_b = device_a.clone();
        let root = root(&device_a).await;
        let cursor = device_b.latest_event_id().await.expect("cursor");
        let (file, _) = device_a
            .upload_new(&root, "shared", bytes(b"x"), meta())
            .await
            .expect("upload");
        let page = device_b.poll_events(&cursor).await.expect("poll");
        assert_eq!(page.events[0].link_id, file);
        assert_eq!(
            device_a.changes(),
            vec![FakeChange::CreatedFile {
                link: file,
                name: "shared".into()
            }]
        );
    }
}
