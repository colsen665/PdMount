//! [`Backend`] on top of the community Proton Drive SDK (patched fork,
//! docs/sdk-patches.md).
//!
//! M0 implements the read side (account, listing, node lookup, blocks,
//! events, quota). Uploads and namespace changes return
//! [`BackendError::Unsupported`] until M2.

use crate::{
    AccountInfo, Backend, BackendError, BlockInfo, EventId, EventKind, EventPage, FileMeta, LinkId,
    NameHash, NodeKind, RemoteEvent, RemoteNode, Result, RevisionId, StorageInfo, UploadSource,
};
use bytes::Bytes;
use futures::StreamExt;
use parking_lot::Mutex;
use proton_drive_sdk::client::ProtonDriveClient;
use proton_drive_sdk::error::ApiError;
use proton_drive_sdk::node::download::{
    CompletedDownloadManifestVerificationException, DataIntegrityException,
};
use proton_drive_sdk::node::draft::{NodeWithSameNameExistsError, RevisionDraftConflictError};
use proton_drive_sdk::node::file::block_map::BlockMap;
use proton_drive_sdk::node::revision::RevisionUid;
use proton_drive_sdk::node::{Node, NodeUid};
use proton_drive_sdk::proton_sdk_rs2::auth::TokenRefreshError;
use proton_drive_sdk::utils::PotentialObject;
use proton_drive_sdk::volume::VolumeId;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

/// Key of the additional revision metadata holding POSIX permission bits,
/// written as an octal string such as `"644"`.
pub const MODE_METADATA_KEY: &str = "pdmount.mode";

/// Block URLs expire; maps older than this are fetched again.
const BLOCK_MAP_TTL: Duration = Duration::from_secs(600);

type BlockMapKey = (LinkId, RevisionId);

pub struct ProtonBackend {
    client: ProtonDriveClient,
    username: String,
    volume: VolumeId,
    root: LinkId,
    block_maps: Mutex<HashMap<BlockMapKey, (Instant, Arc<BlockMap>)>>,
}

impl ProtonBackend {
    /// Resolves the My Files root; fails if the drive is not reachable.
    pub async fn new(client: ProtonDriveClient, username: String) -> Result<Self> {
        let root = client.get_my_files_folder().await.map_err(map_error)?;
        Ok(Self {
            volume: root.base.uid.volume_id.clone(),
            root: LinkId::new(root.base.uid.link_id.raw()),
            client,
            username,
            block_maps: Mutex::new(HashMap::new()),
        })
    }

    pub fn client(&self) -> &ProtonDriveClient {
        &self.client
    }

    fn node_uid(&self, link: &LinkId) -> NodeUid {
        NodeUid::from_parts(self.volume.raw(), link.as_str())
    }

    fn revision_uid(&self, file: &LinkId, revision: &RevisionId) -> RevisionUid {
        RevisionUid::new(
            self.node_uid(file),
            proton_drive_sdk::revision::RevisionId::new(revision.as_str().to_owned()),
        )
    }

    async fn block_map(&self, file: &LinkId, revision: &RevisionId) -> Result<Arc<BlockMap>> {
        let key = (file.clone(), revision.clone());
        if let Some((fetched, map)) = self.block_maps.lock().get(&key)
            && fetched.elapsed() < BLOCK_MAP_TTL
        {
            return Ok(map.clone());
        }
        let map = Arc::new(
            self.client
                .get_block_map(self.revision_uid(file, revision))
                .await
                .map_err(map_error)?,
        );
        self.block_maps
            .lock()
            .insert(key, (Instant::now(), map.clone()));
        Ok(map)
    }
}

fn link_of(uid: &NodeUid) -> LinkId {
    LinkId::new(uid.link_id.raw())
}

fn system_time(time: chrono::DateTime<chrono::Utc>) -> SystemTime {
    SystemTime::from(time)
}

/// Parses the octal permission string stored under [`MODE_METADATA_KEY`].
fn parse_mode(value: &str) -> Option<u32> {
    u32::from_str_radix(value.trim(), 8)
        .ok()
        .filter(|mode| *mode <= 0o7777)
}

fn convert_node(node: Node) -> RemoteNode {
    match node {
        Node::Folder(folder) | Node::Album(folder) => RemoteNode {
            link_id: link_of(&folder.base.uid),
            parent_link_id: folder.base.parent_uid.as_ref().map(link_of),
            name: folder.base.name,
            kind: NodeKind::Folder,
            active_revision: None,
            size: None,
            mtime: folder
                .claimed_modification_time
                .or(Some(folder.base.creation_time))
                .map(system_time),
            mode: None,
            sha1: None,
            name_hash: None,
            trashed: folder.base.trash_time.is_some(),
        },
        Node::File(file) | Node::Photo(file) => {
            let base = file.base.base;
            let revision = file.active_revision;
            let mode = revision
                .additional_claimed_metadata
                .iter()
                .flatten()
                .find(|property| property.key == MODE_METADATA_KEY)
                .and_then(|property| parse_mode(&property.value));
            RemoteNode {
                link_id: link_of(&base.uid),
                parent_link_id: base.parent_uid.as_ref().map(link_of),
                name: base.name,
                kind: NodeKind::File,
                active_revision: Some(RevisionId::new(revision.uid.revision_id.raw())),
                size: revision
                    .claimed_size
                    .and_then(|size| u64::try_from(size).ok()),
                mtime: revision.claimed_modification_time.map(system_time),
                mode,
                sha1: revision
                    .claimed_digests
                    .sha1
                    .and_then(|digest| <[u8; 20]>::try_from(digest).ok()),
                name_hash: None,
                trashed: base.trash_time.is_some(),
            }
        }
    }
}

/// Nodes whose metadata could not be decrypted are skipped with a warning.
fn decrypted<D: std::fmt::Debug>(object: PotentialObject<Node, D>) -> Option<RemoteNode> {
    match object {
        PotentialObject::Node(node) => Some(convert_node(node)),
        PotentialObject::Degraded(degraded) => {
            tracing::warn!(
                ?degraded,
                "skipping node whose metadata could not be decrypted"
            );
            None
        }
    }
}

/// Classifies an SDK error (DESIGN.md Appendix A).
pub fn map_error(error: anyhow::Error) -> BackendError {
    if let Some(conflict) = error.downcast_ref::<NodeWithSameNameExistsError>() {
        return BackendError::AlreadyExists {
            existing: conflict
                .conflict
                .as_ref()
                .and_then(|c| c.link_id.as_ref())
                .map(|link| LinkId::new(link.raw())),
        };
    }
    if let Some(conflict) = error.downcast_ref::<RevisionDraftConflictError>() {
        return BackendError::RevisionConflict {
            current: conflict
                .conflict
                .as_ref()
                .and_then(|c| c.revision_id.as_ref())
                .map(|revision| RevisionId::new(revision.raw())),
        };
    }
    if error.downcast_ref::<DataIntegrityException>().is_some()
        || error
            .downcast_ref::<CompletedDownloadManifestVerificationException>()
            .is_some()
    {
        return BackendError::Integrity(format!("{error:#}"));
    }
    if let Some(refresh) = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<TokenRefreshError>())
    {
        return if refresh.is_refresh_token_expired() {
            BackendError::Unauthorized
        } else {
            BackendError::Transient(format!("{error:#}"))
        };
    }
    if let Some(api) = ApiError::find(&error) {
        return map_api_error(api, &error);
    }
    if let Some(http) = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<reqwest::Error>())
    {
        if http.is_connect() {
            return BackendError::Offline(format!("{error:#}"));
        }
        if http.is_timeout() {
            return BackendError::Transient(format!("{error:#}"));
        }
    }
    BackendError::Other(format!("{error:#}"))
}

fn map_api_error(api: &ApiError, error: &anyhow::Error) -> BackendError {
    if api.is_unauthorized() {
        BackendError::Unauthorized
    } else if api.is_not_found() {
        BackendError::NotFound
    } else if api.is_already_exists() {
        BackendError::AlreadyExists { existing: None }
    } else if api.is_permission_denied() {
        BackendError::PermissionDenied
    } else if api.is_quota_exceeded() {
        BackendError::QuotaExceeded
    } else if api.is_file_too_large() {
        BackendError::FileTooLarge
    } else if api.is_too_many_children() {
        BackendError::TooManyChildren
    } else if api.is_nesting_too_deep() {
        BackendError::NestingTooDeep
    } else if api.is_rate_limited() {
        BackendError::RateLimited {
            retry_after: api.retry_after,
        }
    } else if api.is_retryable() {
        BackendError::Transient(format!("{error:#}"))
    } else {
        BackendError::Other(format!("{error:#}"))
    }
}

fn convert_event_kind(raw: i32) -> Option<EventKind> {
    use proton_drive_sdk::api::events::VolumeEventType;
    Some(match VolumeEventType::from_raw(raw)? {
        VolumeEventType::Delete => EventKind::Delete,
        VolumeEventType::Create => EventKind::Create,
        VolumeEventType::UpdateMetadata => EventKind::UpdateMetadata,
        VolumeEventType::UpdateContent => EventKind::UpdateContent,
    })
}

const WRITES_UNSUPPORTED: &str = "uploads and namespace changes are implemented in M2";

#[async_trait::async_trait]
impl Backend for ProtonBackend {
    async fn account(&self) -> Result<AccountInfo> {
        Ok(AccountInfo {
            username: self.username.clone(),
            root: self.root.clone(),
        })
    }

    async fn root(&self) -> Result<RemoteNode> {
        let root = self.client.get_my_files_folder().await.map_err(map_error)?;
        Ok(convert_node(Node::Folder(root)))
    }

    async fn list_children(&self, folder: &LinkId) -> Result<Vec<RemoteNode>> {
        let children = self
            .client
            .list_children(
                self.volume.clone(),
                Some(proton_drive_sdk::links::LinkId::new(
                    folder.as_str().to_owned(),
                )),
            )
            .await
            .map_err(map_error)?;
        Ok(children
            .into_iter()
            .filter_map(decrypted)
            .filter(|node| !node.trashed)
            .collect())
    }

    async fn get_nodes(&self, ids: &[LinkId]) -> Result<Vec<Option<RemoteNode>>> {
        let uids = ids.iter().map(|id| self.node_uid(id)).collect();
        let mut found: HashMap<LinkId, RemoteNode> = HashMap::new();
        let mut stream =
            std::pin::pin!(self.client.enumerate_nodes(uids).await.map_err(map_error)?);
        while let Some(item) = stream.next().await {
            match item.map_err(map_error) {
                Ok(object) => {
                    if let Some(node) = decrypted(object) {
                        found.insert(node.link_id.clone(), node);
                    }
                }
                Err(BackendError::NotFound) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(ids.iter().map(|id| found.remove(id)).collect())
    }

    async fn blocks(&self, file: &LinkId, revision: &RevisionId) -> Result<Vec<BlockInfo>> {
        let map = self.block_map(file, revision).await?;
        Ok(map
            .blocks
            .iter()
            .map(|entry| BlockInfo {
                index: entry.index.max(0) as u32,
                plain_size: entry.plain_size,
            })
            .collect())
    }

    async fn fetch_block(&self, file: &LinkId, revision: &RevisionId, index: u32) -> Result<Bytes> {
        let map = self.block_map(file, revision).await?;
        let index = i32::try_from(index).map_err(|_| BackendError::NotFound)?;
        if map.entry(index).is_none() {
            return Err(BackendError::NotFound);
        }
        self.client
            .download_block(&map, index)
            .await
            .map(Bytes::from)
            .map_err(map_error)
    }

    async fn upload_new(
        &self,
        _parent: &LinkId,
        _name: &str,
        _source: UploadSource,
        _meta: FileMeta,
    ) -> Result<(LinkId, RevisionId)> {
        Err(BackendError::Unsupported(WRITES_UNSUPPORTED))
    }

    async fn upload_revision(
        &self,
        _file: &LinkId,
        _base: &RevisionId,
        _source: UploadSource,
        _meta: FileMeta,
    ) -> Result<RevisionId> {
        Err(BackendError::Unsupported(WRITES_UNSUPPORTED))
    }

    async fn create_folder(
        &self,
        _parent: &LinkId,
        _name: &str,
        _mtime: SystemTime,
    ) -> Result<LinkId> {
        Err(BackendError::Unsupported(WRITES_UNSUPPORTED))
    }

    async fn rename(
        &self,
        _node: &LinkId,
        _new_parent: Option<&LinkId>,
        _new_name: &str,
        _expected: Option<&NameHash>,
    ) -> Result<()> {
        Err(BackendError::Unsupported(WRITES_UNSUPPORTED))
    }

    async fn trash(&self, _nodes: &[LinkId]) -> Result<Vec<Result<()>>> {
        Err(BackendError::Unsupported(WRITES_UNSUPPORTED))
    }

    async fn latest_event_id(&self) -> Result<EventId> {
        self.client
            .get_volume_latest_event_id(self.volume.clone())
            .await
            .map(EventId::new)
            .map_err(map_error)
    }

    async fn poll_events(&self, since: &EventId) -> Result<EventPage> {
        let response = self
            .client
            .poll_volume_events(self.volume.clone(), since.as_str())
            .await
            .map_err(map_error)?;
        let events = response
            .events
            .into_iter()
            .filter_map(|event| {
                let Some(kind) = convert_event_kind(event.event_type) else {
                    tracing::warn!(event_type = event.event_type, "ignoring unknown event type");
                    return None;
                };
                Some(RemoteEvent {
                    kind,
                    link_id: LinkId::new(event.link.link_id.raw()),
                    parent_link_id: event.link.parent_link_id.map(|p| LinkId::new(p.raw())),
                    trashed: event.link.is_trashed,
                })
            })
            .collect();
        Ok(EventPage {
            next: EventId::new(response.event_id),
            more: response.more,
            refresh: response.refresh,
            events,
        })
    }

    async fn storage_info(&self) -> Result<StorageInfo> {
        let (used, max) = self
            .client
            .get_user_storage_info()
            .await
            .map_err(map_error)?;
        Ok(StorageInfo {
            used: u64::try_from(used).unwrap_or(0),
            max: u64::try_from(max).unwrap_or(0),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_metadata_is_octal_permission_bits() {
        assert_eq!(parse_mode("644"), Some(0o644));
        assert_eq!(parse_mode("0755"), Some(0o755));
        assert_eq!(parse_mode("4755"), Some(0o4755));
        assert_eq!(parse_mode("99"), None);
        assert_eq!(parse_mode("100000"), None);
        assert_eq!(parse_mode(""), None);
    }

    fn api_error(status: u16, body: &str) -> anyhow::Error {
        ApiError::from_http(
            reqwest::StatusCode::from_u16(status).expect("status"),
            &reqwest::header::HeaderMap::new(),
            body,
        )
        .into()
    }

    #[test]
    fn api_errors_map_to_backend_errors() {
        let map = |status, body| map_error(api_error(status, body).context("while listing"));
        assert_eq!(
            map(422, r#"{"Code":2501,"Error":"x"}"#),
            BackendError::NotFound
        );
        assert_eq!(
            map(422, r#"{"Code":2500,"Error":"x"}"#),
            BackendError::AlreadyExists { existing: None }
        );
        assert_eq!(
            map(422, r#"{"Code":2011,"Error":"x"}"#),
            BackendError::PermissionDenied
        );
        assert_eq!(
            map(422, r#"{"Code":200002,"Error":"x"}"#),
            BackendError::QuotaExceeded
        );
        assert_eq!(
            map(422, r#"{"Code":200003,"Error":"x"}"#),
            BackendError::FileTooLarge
        );
        assert_eq!(
            map(422, r#"{"Code":200300,"Error":"x"}"#),
            BackendError::TooManyChildren
        );
        assert_eq!(
            map(422, r#"{"Code":200301,"Error":"x"}"#),
            BackendError::NestingTooDeep
        );
        assert_eq!(map(401, "{}"), BackendError::Unauthorized);
        assert_eq!(
            map(429, "{}"),
            BackendError::RateLimited { retry_after: None }
        );
        assert!(matches!(map(503, "busy"), BackendError::Transient(_)));
        assert!(matches!(
            map(400, r#"{"Code":2001,"Error":"bad"}"#),
            BackendError::Other(_)
        ));
    }

    #[test]
    fn integrity_and_unknown_errors_are_classified() {
        let integrity: anyhow::Error = DataIntegrityException::new("hash mismatch").into();
        assert!(matches!(map_error(integrity), BackendError::Integrity(_)));
        assert!(matches!(
            map_error(anyhow::anyhow!("something else")),
            BackendError::Other(_)
        ));
    }

    #[test]
    fn event_types_map_one_to_one() {
        assert_eq!(convert_event_kind(0), Some(EventKind::Delete));
        assert_eq!(convert_event_kind(1), Some(EventKind::Create));
        assert_eq!(convert_event_kind(2), Some(EventKind::UpdateMetadata));
        assert_eq!(convert_event_kind(3), Some(EventKind::UpdateContent));
        assert_eq!(convert_event_kind(7), None);
    }
}
