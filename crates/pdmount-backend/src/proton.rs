//! [`Backend`] on top of the community Proton Drive SDK (patched fork,
//! docs/sdk-patches.md).
//!
//! Uploads commit with the content's SHA-1 checked by the server. Renames
//! go through `rename_node`, moves with an optional new name through
//! `move_node` (P10), one call each. With an `expected` name hash (the hex
//! digest that `get_nodes` reports, P14) they use the `_with_original_hash`
//! variants, so the server refuses a rename that raced another device's;
//! without one they send no precondition.

use crate::block_lists::{BLOCK_PAGE_SIZE, BlockLists, Listed, Page, PageSource};
use crate::photos::{
    MAX_THUMBNAIL_BYTES, Photos, PhotosVolume, RelatedPhoto, ThumbnailRef, TimelineItem,
};
use crate::shared::{CorePage, DegradedShare, ShareRole, SharedEntry, SharedListing, SharedWithMe};
use crate::{
    AccountInfo, Backend, BackendError, BlockInfo, EventId, EventKind, EventPage, FileMeta, LinkId,
    Listing, NameHash, NodeKind, NodeLookup, RemoteEvent, RemoteNode, Result, RevisionId,
    SignaturePart, StorageInfo, Unverified, UploadSource,
};
use bytes::Bytes;
use parking_lot::Mutex;
use proton_drive_sdk::account::{AccountClient, AccountClientAdapter};
use proton_drive_sdk::api::revision::RevisionDto;
use proton_drive_sdk::client::{ProtonDriveClient, ProtonDriveDefaults};
use proton_drive_sdk::error::{ApiError, HttpTransferError, TooManyRequestsException};
use proton_drive_sdk::http::retry::DriveApiRetryMiddleware;
use proton_drive_sdk::node::download::{
    CompletedDownloadManifestVerificationException, DataIntegrityException,
};
use proton_drive_sdk::node::draft::{NodeWithSameNameExistsError, RevisionDraftConflictError};
use proton_drive_sdk::node::file::FileOperations;
use proton_drive_sdk::node::file::block_map::verify_block_listing;
use proton_drive_sdk::node::revision::RevisionUid;
use proton_drive_sdk::node::thumbnail::ThumbnailType;
use proton_drive_sdk::node::{DegradedNode, Node, NodeMemberRole, NodeUid};
use proton_drive_sdk::photo::ProtonPhotosClient;
use proton_drive_sdk::proton_sdk_rs2::auth::{TokenCredential, TokenRefreshError};
use proton_drive_sdk::proton_sdk_rs2::client::ProtonApiDefaults;
use proton_drive_sdk::proton_sdk_rs2::crypto::proton_rpgp::AsPublicKeyRef;
use proton_drive_sdk::proton_sdk_rs2::session::ProtonAPISession;
use proton_drive_sdk::share::ShareId;
use proton_drive_sdk::utils::PotentialObject;
use proton_drive_sdk::volume::VolumeId;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// Key of the additional revision metadata holding POSIX permission bits,
/// written as an octal string such as `"644"`.
const MODE_METADATA_KEY: &str = "pdmount.mode";

/// A listed block's storage target, with the key that decrypts it.
#[derive(Clone)]
struct BlockTarget {
    bare_url: String,
    token: String,
    content_key: proton_drive_sdk::pgp::PgpSessionKey,
}

/// The revisions' block lists, read by range (D135): a page of the block a
/// read needs, the rest for the manifest.
type ProtonBlockLists = BlockLists<BlockTarget, RevisionDto>;

/// The authenticated connection, replaced as a whole by a new login (M3.16).
#[derive(Clone)]
struct Live {
    client: ProtonDriveClient,
    credential: TokenCredential,
    /// The Photos clients of the session ([`ProtonBackend::set_photos`]).
    photos: Option<PhotosClients>,
    /// The shared-with-me reader ([`ProtonBackend::set_shared_api`]); it
    /// holds no tokens, so a new login keeps it.
    shared: Option<SharedApi>,
}

/// The clients a Photos mount needs (M5.13, D113): the SDK's Photos client,
/// whose drive client reads photos through the Photos API's link endpoints
/// once it has found the Photos share, and PdMount's own client of the
/// timeline endpoint ([`TimelineClient`]): the SDK's `get_timeline_page`
/// drops `RelatedPhotos`, and its timeline types refuse a whole page for
/// one tag or capture time they do not know (D119).
#[derive(Clone)]
pub struct PhotosClients {
    client: Arc<ProtonPhotosClient>,
    drive: ProtonDriveClient,
    timeline: Arc<TimelineClient>,
    /// The account client of the session, as the drive client's: the keys
    /// that verify a thumbnail's manifest (M5.13b), which the drive client
    /// keeps to itself.
    account: Arc<dyn AccountClient>,
    /// The Photos volume and root, found once per session: finding them
    /// also gives the drive client the Photos share's key.
    ready: Arc<tokio::sync::OnceCell<PhotosVolume>>,
    /// The session's tokens, which every call through these clients uses
    /// and refreshes: a call never mixes two sessions (D119).
    credential: TokenCredential,
}

impl PhotosClients {
    /// The Photos clients of `session`, with this device's client uid;
    /// `limits` sets the transfer limits on the drive client as on My
    /// Files' (D033).
    pub fn new(
        session: &ProtonAPISession,
        uid: Option<String>,
        limits: impl FnOnce(ProtonDriveClient) -> ProtonDriveClient,
    ) -> anyhow::Result<Self> {
        let client = ProtonPhotosClient::new(session, uid)?;
        let drive = limits(client.drive().clone());
        let (http, base) = drive_http(session)?;
        let timeline = TimelineClient {
            http,
            base,
            credential: session.token_credential.clone(),
        };
        Ok(Self {
            client: Arc::new(client),
            drive,
            timeline: Arc::new(timeline),
            account: Arc::new(AccountClientAdapter::new(session)),
            ready: Arc::default(),
            credential: session.token_credential.clone(),
        })
    }

    /// The drive client the Photos client reads photos with (its link
    /// endpoints), once the Photos share is found.
    pub fn drive(&self) -> &ProtonDriveClient {
        &self.drive
    }
}

/// The drive API's HTTP client and base URL for PdMount's own readers
/// ([`TimelineClient`], [`SharedApi`]), as `ProtonPhotosClient::new` builds
/// its own: the session's headers and timeout, the SDK's retry middleware.
/// Neither depends on the session's tokens, which each request adds.
fn drive_http(
    session: &ProtonAPISession,
) -> anyhow::Result<(reqwest_middleware::ClientWithMiddleware, reqwest::Url)> {
    let http = session.get_http_client(
        Some(ProtonDriveDefaults::DRIVE_BASE_ROUTE.to_owned()),
        Some(Duration::from_secs(u64::from(
            ProtonApiDefaults::DEFAULT_TIMEOUT_SECONDS,
        ))),
        None,
    )?;
    let base = reqwest::Url::parse(&session.client_config.base_url.to_string())?
        .join(ProtonDriveDefaults::DRIVE_BASE_ROUTE)?;
    let http = reqwest_middleware::ClientBuilder::new(http)
        .with(DriveApiRetryMiddleware::default())
        .build();
    Ok((http, base))
}

/// PdMount's client of the shared-with-me listing (`v2/sharedwithme`, F1),
/// read into its own types ([`SharedPageDto`]) as the timeline is (D119):
/// the SDK takes `ShareTargetType` as a closed enum, so one link of a type
/// it does not know failed the whole page, and a Shared mount stayed empty.
/// The official clients skip such a link
/// (`cs/client/js/src/internal/sharing/apiService.ts:191-194`).
#[derive(Clone)]
pub struct SharedApi {
    http: reqwest_middleware::ClientWithMiddleware,
    base: reqwest::Url,
}

impl SharedApi {
    /// The client for `session`'s API; any later session's tokens work
    /// with it ([`ProtonBackend::set_shared_api`]).
    pub fn new(session: &ProtonAPISession) -> anyhow::Result<Self> {
        let (http, base) = drive_http(session)?;
        Ok(Self { http, base })
    }

    /// One page: the first with `anchor` `None`, else the page after it.
    async fn page(
        &self,
        credential: &TokenCredential,
        anchor: Option<&str>,
    ) -> anyhow::Result<SharedPageDto> {
        let mut url = self.base.join("v2/sharedwithme")?;
        if let Some(anchor) = anchor {
            url.query_pairs_mut().append_pair("AnchorID", anchor);
        }
        let (access, _) = credential.get_tokens().await?;
        let response = self
            .http
            .get(url)
            .header("Authorization", format!("Bearer {access}"))
            .header("x-pm-uid", credential.session_id().raw())
            .send()
            .await?;
        let status = response.status();
        let headers = response.headers().clone();
        let body = response.text().await?;
        parse_api_page(
            "shared-with-me",
            status,
            &headers,
            &body,
            |page: &SharedPageDto| page.code,
        )
    }
}

/// A shared-with-me page as the server sends it. `Links` has no default:
/// an error object with a 2xx status must not read as an empty listing,
/// which would close every share.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
struct SharedPageDto {
    #[serde(default)]
    code: Option<u64>,
    links: Vec<SharedLinkDto>,
    #[serde(default, rename = "AnchorID")]
    anchor_id: Option<String>,
    #[serde(default)]
    more: bool,
}

#[derive(Debug, serde::Deserialize)]
struct SharedLinkDto {
    #[serde(rename = "VolumeID")]
    volume_id: String,
    #[serde(rename = "ShareID")]
    share_id: String,
    #[serde(rename = "LinkID")]
    link_id: String,
    /// A number, also one PdMount does not know.
    #[serde(default, rename = "ShareTargetType")]
    target_type: serde_json::Value,
}

/// Share target types already warned about, once per type and process.
static UNKNOWN_SHARE_TYPES: SeenNodes = SeenNodes::new();

impl SharedLinkDto {
    /// Share, volume, root and kind of a link PdMount mounts: a folder (1)
    /// or a file (2). Root (0), album (3), photo (4) and Proton documents
    /// (5) are left out quietly, as before; a type the official clients do
    /// not know either is left out with a warning, once per type.
    fn entry(self) -> Option<(String, String, String, NodeKind)> {
        let kind = match self.target_type.as_u64() {
            Some(1) => NodeKind::Folder,
            Some(2) => NodeKind::File,
            Some(0 | 3..=5) => return None,
            _ => {
                let raw = self.target_type.to_string();
                let first = {
                    let mut seen = UNKNOWN_SHARE_TYPES.0.get_or_init(Default::default).lock();
                    if seen.len() >= SeenNodes::MAX {
                        seen.clear();
                    }
                    seen.insert(raw.clone())
                };
                if first {
                    tracing::warn!(
                        share_type = %raw,
                        link = %self.link_id,
                        "a shared item of a type PdMount does not know is left out"
                    );
                }
                return None;
            }
        };
        Some((self.share_id, self.volume_id, self.link_id, kind))
    }
}

/// The links of the shared-with-me listing PdMount mounts, page by page as
/// the official clients read it (`More` and `AnchorID`), at most
/// [`MAX_SHARED_PAGES`]; one refresh of a rejected token per page.
async fn shared_links(
    api: &SharedApi,
    credential: &TokenCredential,
) -> Result<Vec<(String, String, String, NodeKind)>> {
    let mut links = Vec::new();
    let mut anchor: Option<String> = None;
    for _ in 0..MAX_SHARED_PAGES {
        let page = with_reauth(credential, || async {
            api.page(credential, anchor.as_deref())
                .await
                .map_err(map_error)
        })
        .await?;
        links.extend(page.links.into_iter().filter_map(SharedLinkDto::entry));
        match page.anchor_id {
            Some(next) if page.more && !next.is_empty() => anchor = Some(next),
            _ => break,
        }
    }
    Ok(links)
}

/// PdMount's client of the timeline endpoint (`volumes/<id>/photos`), sent
/// as the SDK's `DefaultPhotosApiClient::get_timeline_photos` sends it, read
/// into PdMount's own types ([`TimelineDto`]): the SDK's take tags as a
/// closed enum (0-9) and capture times as what chrono holds, so one photo
/// with a newer tag or a far capture time failed the whole page, and with it
/// the timeline and the Photos events, for good (D119).
struct TimelineClient {
    http: reqwest_middleware::ClientWithMiddleware,
    base: reqwest::Url,
    credential: TokenCredential,
}

impl TimelineClient {
    /// One page: the first with `after` `None`, else the page after the
    /// item `after`.
    async fn page(&self, volume: &str, after: Option<&LinkId>) -> anyhow::Result<TimelineDto> {
        let mut url = self.base.join(&format!("volumes/{volume}/photos"))?;
        if let Some(after) = after {
            url.query_pairs_mut()
                .append_pair("PreviousPageLastLinkID", after.as_str());
        }
        let (access, _) = self.credential.get_tokens().await?;
        let response = self
            .http
            .get(url)
            .header("Authorization", format!("Bearer {access}"))
            .header("x-pm-uid", self.credential.session_id().raw())
            .send()
            .await?;
        let status = response.status();
        let headers = response.headers().clone();
        // A body cut off is the connection's failure, not an empty page.
        let body = response.text().await?;
        parse_timeline(status, &headers, &body)
    }
}

/// A timeline page as the server sends it, with the fields PdMount uses.
/// `Photos` has no default: an error object with a 2xx status must not read
/// as an empty page, which ends the timeline.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
struct TimelineDto {
    #[serde(default)]
    code: Option<u64>,
    photos: Vec<TimelinePhotoDto>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
struct TimelinePhotoDto {
    #[serde(rename = "LinkID")]
    link_id: String,
    /// Seconds, as claimed: any number (or none) is read.
    #[serde(default)]
    capture_time: serde_json::Value,
    #[serde(default)]
    related_photos: Vec<RelatedPhotoDto>,
    /// Numbers, also the ones PdMount does not know.
    #[serde(default)]
    tags: Vec<serde_json::Value>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RelatedPhotoDto {
    #[serde(rename = "LinkID")]
    link_id: String,
    #[serde(default)]
    capture_time: serde_json::Value,
}

impl TimelinePhotoDto {
    fn item(self) -> TimelineItem {
        TimelineItem {
            link: LinkId::new(self.link_id),
            capture_time: seconds(&self.capture_time),
            // A tag over 255 means nothing to PdMount: left out.
            tags: self
                .tags
                .iter()
                .filter_map(serde_json::Value::as_u64)
                .filter_map(|tag| u8::try_from(tag).ok())
                .collect(),
            related: self
                .related_photos
                .into_iter()
                .map(|related| RelatedPhoto {
                    link: LinkId::new(related.link_id),
                    capture_time: seconds(&related.capture_time),
                })
                .collect(),
        }
    }
}

/// A claimed capture time in seconds: an integer as it is (beyond `i64`
/// saturated), a fraction cut, anything else the epoch, as the SDK reads a
/// missing one. The tree places what no four-digit year holds at the epoch.
fn seconds(value: &serde_json::Value) -> i64 {
    if let Some(seconds) = value.as_i64() {
        return seconds;
    }
    if value.as_u64().is_some() {
        return i64::MAX;
    }
    // `as` saturates, and NaN becomes 0.
    value.as_f64().map_or(0, |seconds| seconds as i64)
}

/// A timeline response: a non-2xx status or a `Code` other than 1000 is the
/// API's error, classified as any SDK error ([`map_error`]).
fn parse_timeline(
    status: reqwest::StatusCode,
    headers: &reqwest::header::HeaderMap,
    body: &str,
) -> anyhow::Result<TimelineDto> {
    parse_api_page("timeline", status, headers, body, |page: &TimelineDto| {
        page.code
    })
}

/// A page of PdMount's own readers: a non-2xx status or a `Code` other than
/// 1000 (`code` reads it from the page) is the API's error, classified as
/// any SDK error ([`map_error`]); a body that does not parse is an error
/// with its start, `what` naming the endpoint.
fn parse_api_page<T: serde::de::DeserializeOwned>(
    what: &str,
    status: reqwest::StatusCode,
    headers: &reqwest::header::HeaderMap,
    body: &str,
    code: impl Fn(&T) -> Option<u64>,
) -> anyhow::Result<T> {
    if !status.is_success() {
        return Err(ApiError::from_http(status, headers, body).into());
    }
    let refused = || {
        ApiError::from_body(body)
            .filter(|api| api.code != Some(1000))
            .map(anyhow::Error::from)
    };
    match serde_json::from_str::<T>(body) {
        Ok(page) if code(&page).is_none_or(|code| code == 1000) => Ok(page),
        Ok(_) => Err(refused().unwrap_or_else(|| anyhow::anyhow!("the {what} answered an error"))),
        Err(error) => Err(refused().unwrap_or_else(|| {
            let cut = body
                .char_indices()
                .nth(512)
                .map_or(body.len(), |(at, _)| at);
            anyhow::anyhow!("{what}: JSON parse error: {error}. Body: {}", &body[..cut])
        })),
    }
}

pub struct ProtonBackend {
    /// Shared by the backends of every tree of the session
    /// ([`ProtonBackend::tree`]), so a new login reaches all of them.
    live: Arc<parking_lot::RwLock<Live>>,
    username: String,
    volume: VolumeId,
    root: LinkId,
    /// Whether `root` is My Files' root (`get_my_files_folder`); a further
    /// tree's root is fetched by its id.
    my_files: bool,
    /// A tree of the Photos volume ([`ProtonBackend::photos_tree`]): its
    /// calls go through the Photos clients' drive client.
    photos_tree: bool,
    block_lists: ProtonBlockLists,
    /// Creator and creation time by share id: they never change, so the
    /// shared-with-me listing asks for each share once (M5.12).
    share_info: Mutex<HashMap<String, ShareInfo>>,
}

/// What `get_share` says about a share that the listing uses.
#[derive(Debug, Clone)]
struct ShareInfo {
    creator: String,
    created: Option<SystemTime>,
}

/// Pages of the shared-with-me listing read at most (each holds up to
/// 150 links): a bound on what the server can make PdMount hold.
const MAX_SHARED_PAGES: usize = 64;

/// Access tokens for [`with_reauth`].
#[async_trait::async_trait]
trait Reauth {
    /// The access token a call is about to use.
    async fn access_token(&self) -> anyhow::Result<String>;
    /// A token that replaces `rejected`; one refresh serves every caller that
    /// was rejected with the same token.
    async fn refreshed(&self, rejected: String) -> anyhow::Result<String>;
}

#[async_trait::async_trait]
impl Reauth for TokenCredential {
    async fn access_token(&self) -> anyhow::Result<String> {
        self.get_tokens().await.map(|(access, _)| access)
    }

    async fn refreshed(&self, rejected: String) -> anyhow::Result<String> {
        self.get_refreshed_access_token(rejected).await
    }
}

/// Runs `call`; when the server rejects the access token, refreshes it and
/// runs `call` once more. The drive API client sends the token but does not
/// refresh it, so this is the only place an expired token is renewed between
/// the daemon's proactive checks. A failed refresh is classified as any SDK
/// error: an expired refresh token is `Unauthorized`, anything else transient.
async fn with_reauth<T, F, Fut>(reauth: &impl Reauth, call: F) -> Result<T>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let token = reauth.access_token().await.map_err(map_error)?;
    match call().await {
        Err(BackendError::Unauthorized) => {
            reauth.refreshed(token).await.map_err(map_error)?;
            call().await
        }
        other => other,
    }
}

impl ProtonBackend {
    /// Resolves the My Files root; fails if the drive is not reachable.
    pub async fn new(
        client: ProtonDriveClient,
        credential: TokenCredential,
        username: String,
    ) -> Result<Self> {
        let root = with_reauth(&credential, || async {
            client.get_my_files_folder().await.map_err(map_error)
        })
        .await?;
        Ok(Self {
            volume: root.base.uid.volume_id.clone(),
            root: LinkId::new(root.base.uid.link_id.raw()),
            my_files: true,
            photos_tree: false,
            live: Arc::new(parking_lot::RwLock::new(Live {
                client,
                credential,
                photos: None,
                shared: None,
            })),
            username,
            block_lists: ProtonBlockLists::default(),
            share_info: Mutex::new(HashMap::new()),
        })
    }

    /// A backend for a drive whose volume and root are known from an
    /// earlier run, so nothing is asked of the server: mounting without a
    /// connection (§9.6). Calls fail with `Offline` until it is reachable.
    pub fn with_root(
        client: ProtonDriveClient,
        credential: TokenCredential,
        username: String,
        volume: impl Into<String>,
        root: LinkId,
    ) -> Self {
        Self {
            live: Arc::new(parking_lot::RwLock::new(Live {
                client,
                credential,
                photos: None,
                shared: None,
            })),
            username,
            volume: VolumeId::new(volume.into()),
            root,
            my_files: true,
            photos_tree: false,
            block_lists: ProtonBlockLists::default(),
            share_info: Mutex::new(HashMap::new()),
        }
    }

    /// The backend of a further tree (M5.10, D080): the folder `root` on
    /// `volume`, over this backend's session, client and transfer limits.
    /// Every node id it builds is on that volume; `root` and `account`
    /// name the tree's root, and `poll_events` follows that volume.
    pub fn tree(&self, volume: impl Into<String>, root: LinkId) -> Self {
        Self {
            live: Arc::clone(&self.live),
            username: self.username.clone(),
            volume: VolumeId::new(volume.into()),
            root,
            my_files: false,
            photos_tree: false,
            block_lists: ProtonBlockLists::default(),
            share_info: Mutex::new(HashMap::new()),
        }
    }

    /// The backend of the Photos tree (M5.13, D113): [`ProtonBackend::tree`]
    /// for the Photos volume and root, whose calls go through the Photos
    /// clients ([`ProtonBackend::set_photos`]), as the official Photos
    /// client reads photos.
    pub fn photos_tree(&self, volume: impl Into<String>, root: LinkId) -> Self {
        Self {
            photos_tree: true,
            ..self.tree(volume, root)
        }
    }

    /// Continues with a new login's connection (the same account), the
    /// Photos clients of the new session with it in one write, so no call
    /// finds one session's drive client beside another's Photos clients
    /// (D119). Calls already running finish on the old session.
    pub fn replace_session(
        &self,
        client: ProtonDriveClient,
        credential: TokenCredential,
        photos: Option<PhotosClients>,
    ) {
        let mut live = self.live.write();
        *live = Live {
            client,
            credential,
            photos,
            shared: live.shared.take(),
        };
    }

    /// The shared-with-me reader for `session`'s API (F1); a Shared mount's
    /// listing fails without it (logged).
    pub fn set_shared_api(&self, session: &ProtonAPISession) {
        match SharedApi::new(session) {
            Ok(api) => self.live.write().shared = Some(api),
            Err(error) => tracing::warn!(
                error = %format!("{error:#}"),
                "cannot make the shared-with-me client"
            ),
        }
    }

    /// The session's Photos clients (M5.13), for a Photos mount.
    pub fn set_photos(&self, photos: Option<PhotosClients>) {
        self.live.write().photos = photos;
    }

    /// The Photos clients, with the Photos volume found: `NotFound` when
    /// the account has no Photos. A call uses the clients this returns,
    /// and their credential, from its start to its end, its retry
    /// included, whatever a new login replaces meanwhile.
    async fn photos_ready(&self) -> Result<(PhotosClients, PhotosVolume)> {
        let photos = self
            .live
            .read()
            .photos
            .clone()
            .ok_or(BackendError::Unsupported("Photos without its clients"))?;
        let volume = photos
            .ready
            .get_or_try_init(|| async {
                let root = with_reauth(&photos.credential, || async {
                    photos
                        .client
                        .get_photos_root_folder()
                        .await
                        .map_err(map_error)
                })
                .await?;
                Ok::<_, BackendError>(PhotosVolume {
                    volume: root.base.uid.volume_id.raw().to_owned(),
                    root: link_of(&root.base.uid),
                })
            })
            .await?
            .clone();
        Ok((photos, volume))
    }

    /// The volume id, kept for [`ProtonBackend::with_root`].
    pub fn volume_id(&self) -> &str {
        self.volume.raw()
    }

    /// The drive client of a call and the credential of its session, read
    /// together. A Photos tree's goes through the Photos API: the clients
    /// [`ProtonBackend::photos_ready`] gives, never My Files' client (D119).
    async fn session_for_call(&self) -> Result<(ProtonDriveClient, TokenCredential)> {
        if self.photos_tree {
            let (photos, _) = self.photos_ready().await?;
            return Ok((photos.drive, photos.credential));
        }
        let live = self.live.read();
        Ok((live.client.clone(), live.credential.clone()))
    }

    /// Runs `call` on the session's client, with one token refresh on a
    /// rejected token ([`with_reauth`]); the retry uses the same client and
    /// credential. A Photos tree finds the Photos share first, once per
    /// session.
    async fn authed<T, F, Fut>(&self, call: F) -> Result<T>
    where
        F: Fn(ProtonDriveClient) -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
    {
        let (client, credential) = self.session_for_call().await?;
        with_reauth(&credential, || call(client.clone())).await
    }

    /// Deletes everything in the account's trash for good (`pdmount trash
    /// empty`, D036). Not part of `Backend`: the mount never needs it.
    pub async fn empty_trash(&self) -> Result<()> {
        self.authed(|client| async move { client.empty_trash().await.map_err(map_error) })
            .await
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

    /// The block pages of one revision, through the session's client.
    fn pages(&self, file: &LinkId, revision: &RevisionId) -> ProtonPages<'_> {
        ProtonPages {
            backend: self,
            uid: self.revision_uid(file, revision),
        }
    }

    /// The claimed plaintext block sizes of `revision` from its node, when
    /// it is the node's active revision and declares them (fork P4): the
    /// layout without a block listing. The node usually comes from the
    /// SDK's cache.
    async fn node_block_sizes(
        &self,
        file: &LinkId,
        revision: &RevisionId,
    ) -> Result<Option<Vec<u32>>> {
        let uid = self.node_uid(file);
        let node = self
            .authed(|client| {
                let uid = uid.clone();
                async move { client.get_node(uid).await.map_err(map_error) }
            })
            .await?;
        let PotentialObject::Node(Node::File(file) | Node::Photo(file)) = node else {
            return Ok(None);
        };
        let active = file.active_revision;
        if active.uid.revision_id.raw() != revision.as_str() {
            return Ok(None);
        }
        Ok(active
            .claimed_block_sizes
            .filter(|sizes| !sizes.is_empty())
            .and_then(|sizes| sizes.iter().map(|&size| u32::try_from(size).ok()).collect()))
    }
}

/// [`PageSource`] of one revision on the Proton API (fork P30).
struct ProtonPages<'a> {
    backend: &'a ProtonBackend,
    uid: RevisionUid,
}

#[async_trait::async_trait]
impl PageSource for ProtonPages<'_> {
    type Target = BlockTarget;
    type Meta = RevisionDto;

    async fn page(&self, from: u32, size: u32) -> Result<Page<BlockTarget, RevisionDto>> {
        let from = i32::try_from(from).map_err(|_| BackendError::NotFound)?;
        let size = i32::try_from(size).unwrap_or(i32::MAX);
        let (listing, content_key) = self
            .backend
            .authed(|client| {
                let uid = self.uid.clone();
                async move {
                    let listing = client
                        .get_block_page(&uid, from, size)
                        .await
                        .map_err(map_error)?;
                    let secrets = FileOperations::get_secrets(&client, uid.node_uid.clone())
                        .await
                        .map_err(map_error)?;
                    Ok((listing, secrets.content_key))
                }
            })
            .await?;
        let blocks = listing
            .blocks
            .into_iter()
            .map(|block| Listed {
                index: block.index.max(0) as u32,
                hash: block.hash_digest,
                target: BlockTarget {
                    bare_url: block.bare_url,
                    token: block.token,
                    content_key: content_key.clone(),
                },
            })
            .collect();
        Ok(Page {
            blocks,
            meta: listing.revision,
        })
    }

    async fn verify(&self, meta: &RevisionDto, hashes: &[&[u8]]) -> Result<()> {
        self.backend
            .authed(|client| {
                let uid = self.uid.clone();
                async move {
                    client
                        .verify_listed_block_hashes(&uid, meta, hashes)
                        .await
                        .map_err(map_error)
                }
            })
            .await
    }
}

/// A file's size from what its uploader claims. The claimed block sizes win
/// over the claimed size: blocks are what reads and staging use, and a
/// claimed size no block list backs must not size a staging file (S-5).
fn file_size(claimed_size: Option<i64>, block_sizes: Option<&[i32]>) -> Option<u64> {
    let claimed = claimed_size.and_then(|size| u64::try_from(size).ok());
    let Some(sizes) = block_sizes.filter(|sizes| !sizes.is_empty()) else {
        return claimed;
    };
    let sum = sizes.iter().fold(0u64, |sum, &size| {
        sum.saturating_add(u64::try_from(size).unwrap_or(0))
    });
    if claimed.is_some_and(|claimed| claimed != sum) {
        tracing::debug!(
            claimed = claimed_size,
            blocks = sum,
            "claimed size differs from the block sizes; using the block sizes"
        );
    }
    Some(sum)
}

/// The size of a file whose revision claims neither a size nor block
/// sizes (fork P4 leaves it unknown since M6.0.4.11): its encrypted storage
/// size, an upper bound, logged once per node at WARN. Reads of such a file
/// end in EIO at the last block instead of showing it empty; the official
/// clients show its size as unknown (DESIGN §6.1).
fn estimated_size(uid: &NodeUid, storage_size: i64) -> Option<u64> {
    let size = u64::try_from(storage_size).ok()?;
    if first_for_node(&ESTIMATED_SIZES, uid) {
        tracing::warn!(
            link = %link_of(uid),
            storage_size,
            "a file claims no size; its encrypted size stands in, and its last block will not read"
        );
    }
    Some(size)
}

/// Nodes already logged by [`estimated_size`].
static ESTIMATED_SIZES: SeenNodes = SeenNodes::new();

/// Nodes a warning was logged for, so that it is logged once per node and
/// process; at most [`SeenNodes::MAX`], then it starts again.
struct SeenNodes(std::sync::OnceLock<Mutex<std::collections::HashSet<String>>>);

impl SeenNodes {
    const MAX: usize = 10_000;

    const fn new() -> Self {
        Self(std::sync::OnceLock::new())
    }
}

/// Whether `uid` is new to `seen` (and now in it).
fn first_for_node(seen: &SeenNodes, uid: &NodeUid) -> bool {
    let mut set = seen.0.get_or_init(Default::default).lock();
    if set.len() >= SeenNodes::MAX {
        set.clear();
    }
    set.insert(uid.to_string())
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

/// Which of a node's signatures did not verify (fork P27): its name's, its
/// key's (passphrase, or a folder's hash key) and its content key's, each
/// with the signer it claims.
fn unverified_signatures(node: &Node) -> Vec<(SignaturePart, Option<String>)> {
    fn claimed(error: &proton_drive_sdk::protobuf::SignatureVerificationError) -> Option<String> {
        error
            .claimed_author
            .as_ref()
            .map(|author| author.email_address.clone())
            .filter(|email| !email.is_empty())
    }
    let base = node.base();
    let mut unverified = Vec::new();
    if let PotentialObject::Degraded(error) = &base.name_author {
        unverified.push((SignaturePart::Name, claimed(error)));
    }
    if let PotentialObject::Degraded(error) = &base.author {
        unverified.push((SignaturePart::Key, claimed(error)));
    }
    if let Node::File(file) | Node::Photo(file) = node
        && let Some(PotentialObject::Degraded(error)) = &file.active_revision.content_author
    {
        unverified.push((SignaturePart::ContentKey, claimed(error)));
    }
    unverified
}

/// Nodes already logged by [`unverified_of`].
static UNVERIFIED: SeenNodes = SeenNodes::new();

/// The node's failed signatures for [`RemoteNode::unverified`] (D132),
/// logged at WARN once per node and process.
fn unverified_of(node: &Node) -> Option<Unverified> {
    let failed = unverified_signatures(node);
    let parts: Vec<(SignaturePart, Option<&str>)> = failed
        .iter()
        .map(|(part, signer)| (*part, signer.as_deref()))
        .collect();
    let unverified = Unverified::of(&link_of(node.uid()), &node.base().name, &parts)?;
    warn_unverified(node.uid(), &unverified);
    Some(unverified)
}

/// Logs a node whose signatures do not verify at WARN, once per node and
/// process. Returns whether it logged.
fn warn_unverified(uid: &NodeUid, unverified: &Unverified) -> bool {
    if !first_for_node(&UNVERIFIED, uid) {
        return false;
    }
    tracing::warn!(
        link = %link_of(uid),
        signatures = ?unverified.parts().iter().map(|part| part.as_str()).collect::<Vec<_>>(),
        "a node's signatures do not verify; the root's `signatures` mode decides whether it opens"
    );
    true
}

fn convert_node(node: Node) -> RemoteNode {
    let unverified = unverified_of(&node);
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
            unverified,
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
                size: file_size(
                    revision.claimed_size,
                    revision.claimed_block_sizes.as_deref(),
                )
                .or_else(|| estimated_size(&base.uid, revision.size_on_cloud_storage)),
                // Without a claimed mtime, the revision's commit time.
                mtime: Some(system_time(
                    revision
                        .claimed_modification_time
                        .unwrap_or(revision.creation_time),
                )),
                mode,
                sha1: revision
                    .claimed_digests
                    .sha1
                    .and_then(|digest| <[u8; 20]>::try_from(digest).ok()),
                name_hash: None,
                trashed: base.trash_time.is_some(),
                unverified,
            }
        }
    }
}

/// Lowercase hex of a name hash digest.
fn hash_to_hex(digest: &[u8]) -> NameHash {
    NameHash::new(hex::encode(digest))
}

/// The digest behind a hex name hash; `None` when `text` is not hex.
fn hash_from_hex(text: &str) -> Option<Vec<u8>> {
    hex::decode(text).ok()
}

/// The first cause of type `T` in an SDK error's chain.
fn cause<T: std::error::Error + 'static>(error: &anyhow::Error) -> Option<&T> {
    error.chain().find_map(|cause| cause.downcast_ref::<T>())
}

/// The plaintext did not fit the buffer it was written to: a full
/// `Cursor<&mut [u8]>` refuses the rest with `WriteZero`.
fn overflowed(error: &anyhow::Error) -> bool {
    cause::<std::io::Error>(error).is_some_and(|e| e.kind() == std::io::ErrorKind::WriteZero)
}

/// Classifies an SDK error (DESIGN.md Appendix A).
fn map_error(error: anyhow::Error) -> BackendError {
    classify(&error)
}

/// [`map_error`] for an error the caller keeps: a session check at the
/// start, say, whose rate limit a retry must honour (D106).
pub fn classify(error: &anyhow::Error) -> BackendError {
    if let Some(conflict) = cause::<NodeWithSameNameExistsError>(error) {
        return BackendError::AlreadyExists {
            existing: conflict
                .conflict
                .as_ref()
                .and_then(|c| c.link_id.as_ref())
                .map(|link| LinkId::new(link.raw())),
        };
    }
    if let Some(conflict) = cause::<RevisionDraftConflictError>(error) {
        return BackendError::RevisionConflict {
            current: conflict
                .conflict
                .as_ref()
                .and_then(|c| c.revision_id.as_ref())
                .map(|revision| RevisionId::new(revision.raw())),
        };
    }
    if cause::<DataIntegrityException>(error).is_some()
        || cause::<CompletedDownloadManifestVerificationException>(error).is_some()
        || cause::<proton_drive_sdk::error::ChecksumMismatchIntegrityException>(error).is_some()
        || cause::<proton_drive_sdk::error::ContentSizeMismatchIntegrityException>(error).is_some()
        // A block that fails its check before upload, twice (P20).
        || cause::<proton_drive_sdk::error::IntegrityException>(error).is_some()
    {
        return BackendError::Integrity(format!("{error:#}"));
    }
    if let Some(refresh) = cause::<TokenRefreshError>(error) {
        return if refresh.is_refresh_token_expired() {
            BackendError::Unauthorized
        } else {
            BackendError::Transient(format!("{error:#}"))
        };
    }
    if let Some(api) = ApiError::find(error) {
        return map_api_error(api, error);
    }
    if let Some(too_many) = cause::<TooManyRequestsException>(error) {
        return BackendError::RateLimited {
            retry_after: too_many.retry_after,
        };
    }
    if let Some(transfer) = cause::<HttpTransferError>(error) {
        return map_transfer_error(transfer, error);
    }
    if let Some(http) = transport_error(error) {
        if http.is_connect() {
            return BackendError::Offline(format!("{error:#}"));
        }
        // A connection that broke while the request or the answer was on
        // its way (reset, closed, cut short): the next attempt may succeed,
        // and the next connect tells whether the drive is unreachable.
        if http.is_timeout() || http.is_request() || http.is_body() || http.is_decode() {
            return BackendError::Transient(format!("{error:#}"));
        }
    }
    if error
        .chain()
        .any(|cause| cause.to_string().contains(USER_KEYS_LOCKED))
    {
        tracing::warn!("the account keys cannot be unlocked; a new login is needed");
        return BackendError::Unauthorized;
    }
    BackendError::Other(format!("{error:#}"))
}

/// `ProtonAccountClient::get_user_keys` when no passphrase opens an active
/// key. The SDK has no error type for it; only a new login can help, as with
/// a rejected session (D085). A test runs the SDK's own path.
const USER_KEYS_LOCKED: &str = "none could be unlocked";

/// Whether an SDK error (a session check, say) is a failure to connect, as
/// opposed to an answer from the server.
pub fn is_connection_error(error: &anyhow::Error) -> bool {
    transport_error(error).is_some_and(reqwest::Error::is_connect)
}

/// The SDK's adopted server time offset in this process (server time minus
/// device time, in seconds), which dates keys and signatures and the time they
/// are verified at; `None` while no Drive API answer has been adopted and the
/// device clock is used (fork P19 amended, P29).
pub fn server_time_offset() -> Option<i64> {
    proton_drive_sdk::server_time::offset_seconds()
}

/// Passes a Drive API answer's server time (Unix seconds) through the SDK's
/// offset rules, as the answers of a mount's clients are.
pub fn observe_server_time(server_unix_seconds: i64) {
    proton_drive_sdk::server_time::update(server_unix_seconds);
}

/// The `reqwest::Error` behind an SDK error. Drive API calls go through
/// `reqwest_middleware`, whose `Error::Reqwest` is `#[error(transparent)]`: the
/// `reqwest::Error` is then not part of `anyhow::Error::chain()` and has to be
/// taken out of the middleware error.
fn transport_error(error: &anyhow::Error) -> Option<&reqwest::Error> {
    error.chain().find_map(|cause| {
        cause.downcast_ref::<reqwest::Error>().or_else(|| {
            match cause.downcast_ref::<reqwest_middleware::Error>() {
                Some(reqwest_middleware::Error::Reqwest(http)) => Some(http),
                _ => None,
            }
        })
    })
}

/// Runs a block download, and once more when its body was cut off on the
/// way (parity R6): the fork retries a failed connect, a timeout and a
/// retryable status itself, refreshes an expired target and fetches a block
/// whose hash does not match once more (P2), but fails at once when the
/// connection breaks while the block is read. The JS SDK retries any failed
/// block once. A second cut is the error (`Transient`).
async fn once_more_if_cut<T, F, Fut>(index: i32, download: F) -> anyhow::Result<T>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<T>>,
{
    match download().await {
        Err(error) if cut_off(&error) => {
            tracing::warn!(
                block = index,
                error = %format!("{error:#}"),
                "a block's transfer was cut off; fetching it once more"
            );
            download().await
        }
        other => other,
    }
}

/// The connection broke while an answer's body was on its way.
fn cut_off(error: &anyhow::Error) -> bool {
    transport_error(error).is_some_and(|http| http.is_body() || http.is_decode())
}

/// A failed block transfer (download, or upload to the storage endpoint).
fn map_transfer_error(transfer: &HttpTransferError, error: &anyhow::Error) -> BackendError {
    match transfer.status {
        401 => BackendError::Unauthorized,
        429 => BackendError::RateLimited {
            retry_after: transfer.retry_after,
        },
        _ if transfer.is_retryable() => BackendError::Transient(format!("{error:#}")),
        _ => BackendError::Other(format!("{error:#}")),
    }
}

/// Code of a refused rename or move (M3.1, S6–S8); the node's state tells
/// the cases apart ([`refused_move`]), the message only as a last resort.
const CODE_REFUSED: u32 = 2000;
/// Code of a commit whose draft was removed meanwhile (M3.1, S3).
const CODE_NOT_IN_DRAFT: u32 = 2511;
/// Folders read at most, from a move's target up, to find a cycle.
const MAX_ANCESTORS: usize = 128;

/// Whether an SDK error is the server's refusal of a rename or move.
fn is_refused(error: &anyhow::Error) -> bool {
    ApiError::find(error)
        .is_some_and(|api| api.code == Some(CODE_REFUSED) && !api.is_unauthorized())
}

/// What a rename or move that the server refused with code 2000 ran into,
/// read from the node's state as C#'s `NodeMoveOperation` reads the node
/// again (`Nodes/Move/NodeMoveOperation.cs:252-253`, `:313-344`), not from
/// the English message, which the server may reword (parity N2):
/// - the node is gone or trashed, or the move's target folder is: `NotFound`
///   (the journal tells them apart);
/// - its name hash is no longer `expected`: renamed or moved meanwhile, also
///   by this device's earlier attempt whose answer was lost: `NameConflict`;
/// - a move into the node itself or one of its descendants: `InvalidMove`;
/// - another node has `new_name` in the target folder: `AlreadyExists`;
/// - none of these (an unchanged node whose hash was refused): `NameConflict`,
///   which the journal parks after reading the node again (§8.3).
///
/// When a read fails, or a node on the way cannot be decrypted, the
/// message's reading (`by_message`) decides.
async fn refused_move(
    backend: &dyn Backend,
    node: &LinkId,
    new_parent: Option<&LinkId>,
    new_name: &str,
    expected: Option<&NameHash>,
    by_message: BackendError,
) -> BackendError {
    match refusal_by_state(backend, node, new_parent, new_name, expected).await {
        Ok(decided) => decided,
        Err(error) => {
            tracing::debug!(%node, %error, "a refused rename could not be checked; its message decides");
            by_message
        }
    }
}

async fn refusal_by_state(
    backend: &dyn Backend,
    node: &LinkId,
    new_parent: Option<&LinkId>,
    new_name: &str,
    expected: Option<&NameHash>,
) -> Result<BackendError> {
    let Some(current) = live_node(backend, node).await? else {
        return Ok(BackendError::NotFound);
    };
    if expected.is_some() && current.name_hash.as_ref() != expected {
        return Ok(BackendError::NameConflict);
    }
    let target = match new_parent {
        Some(parent) => {
            let mut at = parent.clone();
            let mut levels = 0;
            loop {
                if at == *node {
                    return Ok(BackendError::InvalidMove);
                }
                let Some(folder) = live_node(backend, &at).await? else {
                    return Ok(BackendError::NotFound);
                };
                match folder.parent_link_id {
                    Some(up) if levels < MAX_ANCESTORS => at = up,
                    Some(_) => return Err(BackendError::Other("too many ancestors".into())),
                    None => break,
                }
                levels += 1;
            }
            parent.clone()
        }
        None => match current.parent_link_id {
            Some(parent) => parent,
            None => return Ok(BackendError::NameConflict),
        },
    };
    let taken = backend
        .list_children(&target)
        .await?
        .into_iter()
        .find(|child| child.name == new_name && child.link_id != *node);
    Ok(match taken {
        Some(child) => BackendError::AlreadyExists {
            existing: Some(child.link_id),
        },
        None => BackendError::NameConflict,
    })
}

/// The node `link`, fresh; `None` when it is gone or trashed. One that
/// cannot be decrypted is an error: its state cannot be read.
async fn live_node(backend: &dyn Backend, link: &LinkId) -> Result<Option<RemoteNode>> {
    match backend
        .get_nodes_checked(std::slice::from_ref(link))
        .await?
        .pop()
    {
        Some(NodeLookup::Found(node)) => Ok((!node.trashed).then_some(node)),
        Some(NodeLookup::Absent) | None => Ok(None),
        Some(NodeLookup::Degraded) => {
            Err(BackendError::Other(format!("{link} cannot be decrypted")))
        }
    }
}

fn map_api_error(api: &ApiError, error: &anyhow::Error) -> BackendError {
    // An expired token can also arrive as HTTP 200 with `Code` 401 in the body
    // (`proton-sdk-rs2` `session.rs` `ensure_authenticated`).
    if api.is_unauthorized() || api.code == Some(401) {
        BackendError::Unauthorized
    } else if api.code == Some(CODE_REFUSED) && api.message.contains("out of date") {
        // Rename answers HTTP 422, move a 2xx body: the status is no signal.
        BackendError::NameConflict
    } else if api.code == Some(CODE_REFUSED) && api.message.contains("sub-folder") {
        BackendError::InvalidMove
    } else if api.code == Some(CODE_NOT_IN_DRAFT) {
        BackendError::Transient(format!("{error:#}"))
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

/// Content and size of an upload.
async fn open_source(
    source: UploadSource,
) -> Result<(u64, Box<dyn tokio::io::AsyncRead + Unpin + Send>)> {
    match source {
        UploadSource::Bytes(bytes) => {
            Ok((bytes.len() as u64, Box::new(std::io::Cursor::new(bytes))))
        }
        UploadSource::File(path) => {
            let file = tokio::fs::File::open(&path).await.map_err(|e| {
                BackendError::local_io(format_args!("cannot read {}", path.display()), e)
            })?;
            let size = file
                .metadata()
                .await
                .map_err(|e| {
                    BackendError::local_io(format_args!("cannot stat {}", path.display()), e)
                })?
                .len();
            Ok((size, Box::new(file)))
        }
    }
}

/// Additional revision metadata: the permission bits (F10).
fn mode_metadata(
    mode: Option<u32>,
) -> Option<Vec<proton_drive_sdk::meta::AdditionalMetadataProperty>> {
    mode.map(|mode| {
        vec![proton_drive_sdk::meta::AdditionalMetadataProperty {
            key: MODE_METADATA_KEY.into(),
            value: format!("{:o}", mode & 0o7777),
        }]
    })
}

impl ProtonBackend {
    async fn upload(
        uploader: proton_drive_sdk::node::file::upload::FileUploader,
        reader: Box<dyn tokio::io::AsyncRead + Unpin + Send>,
    ) -> Result<proton_drive_sdk::node::file::upload::UploadResult> {
        uploader
            .upload_from_stream_with_revision(reader, Vec::new(), Box::new(|_, _| {}))
            .await
            .map_err(map_error)
    }

    /// One verified, decrypted block, straight into `buf`; returns its
    /// length. Its page is listed when needed and, on the revision's first
    /// read, the rest of the listing for the manifest beside the download
    /// (D135); the block counts once the manifest verified. A block that
    /// decrypts to more than `buf` holds is an integrity failure; the cache
    /// checks the length against the layout.
    async fn download_block_into(
        &self,
        file: &LinkId,
        revision: &RevisionId,
        index: u32,
        buf: &mut [u8],
    ) -> Result<usize> {
        let kept = self.block_lists.revision(file, revision);
        let pages = self.pages(file, revision);
        let uid = pages.uid.clone();
        self.block_lists
            .fetch(&kept, &pages, index, |listed| {
                self.download_listed(uid, listed, buf)
            })
            .await
    }

    /// Downloads one listed block into `buf` (the fork's downloader checks
    /// its hash and refreshes an expired target, P2).
    async fn download_listed(
        &self,
        uid: RevisionUid,
        listed: Listed<BlockTarget>,
        buf: &mut [u8],
    ) -> Result<usize> {
        let index = i32::try_from(listed.index).map_err(|_| BackendError::NotFound)?;
        let capacity = buf.len();
        let too_long = || {
            BackendError::Integrity(format!(
                "block {index} of revision {uid} has more bytes than the {capacity} expected"
            ))
        };
        // `authed` runs the call again after a token refresh, so its
        // closure is `Fn`: it borrows the buffer through a lock.
        let buf = tokio::sync::Mutex::new(buf);
        let (uid, listed) = (&uid, &listed);
        self.authed(|client| {
            let buf = &buf;
            async move {
                let client = &client;
                let download = move || async move {
                    let mut buf = buf.lock().await;
                    let mut out = std::io::Cursor::new(&mut **buf);
                    client
                        .block_downloader()
                        .download_verified(
                            client,
                            uid.clone(),
                            index,
                            listed.target.bare_url.clone(),
                            listed.target.token.clone(),
                            &listed.hash,
                            listed.target.content_key.clone(),
                            &mut out,
                        )
                        .await?;
                    Ok(out.position())
                };
                let len = once_more_if_cut(index, download).await.map_err(|error| {
                    if overflowed(&error) {
                        too_long()
                    } else {
                        map_error(error)
                    }
                })?;
                Ok(len as usize)
            }
        })
        .await
    }
}

#[async_trait::async_trait]
impl Backend for ProtonBackend {
    /// The SDK's revision writer reads this many plaintext bytes per block
    /// (`sdk/node/revision.rs:458`), whatever its doc says about encrypted
    /// sizes; the cache seeds its layout from it.
    fn upload_block_size(&self) -> u64 {
        // The SDK's constant, the same for every client of the session.
        self.live.read().client.target_block_size() as u64
    }

    async fn account(&self) -> Result<AccountInfo> {
        Ok(AccountInfo {
            username: self.username.clone(),
            root: self.root.clone(),
        })
    }

    async fn root(&self) -> Result<RemoteNode> {
        if !self.my_files {
            // A further tree: its root by id, as any node of the volume. A
            // root that cannot be decrypted is not gone (D119).
            return match self
                .get_nodes_checked(std::slice::from_ref(&self.root))
                .await?
                .pop()
            {
                Some(NodeLookup::Found(root)) => Ok(root),
                Some(NodeLookup::Degraded) => Err(BackendError::Transient(
                    "the tree's root cannot be decrypted".into(),
                )),
                Some(NodeLookup::Absent) | None => Err(BackendError::NotFound),
            };
        }
        let root = self
            .authed(|client| async move { client.get_my_files_folder().await.map_err(map_error) })
            .await?;
        Ok(convert_node(Node::Folder(root)))
    }

    async fn list_children(&self, folder: &LinkId) -> Result<Vec<RemoteNode>> {
        Ok(self.list_children_checked(folder).await?.children)
    }

    /// A child that does not decrypt comes back degraded (fork P26) and is
    /// named in [`Listing::degraded`], not left out as if it were gone.
    async fn list_children_checked(&self, folder: &LinkId) -> Result<Listing> {
        let children = self
            .authed(|client| async move {
                client
                    .list_children(
                        self.volume.clone(),
                        Some(proton_drive_sdk::links::LinkId::new(
                            folder.as_str().to_owned(),
                        )),
                    )
                    .await
                    .map_err(map_error)
            })
            .await?;
        let mut listing = Listing::default();
        for child in children {
            match child {
                PotentialObject::Node(node) => {
                    let node = convert_node(node);
                    if !node.trashed {
                        listing.children.push(node);
                    }
                }
                PotentialObject::Degraded(degraded) => {
                    let trashed = match &degraded {
                        DegradedNode::Folder(n) | DegradedNode::Album(n) => n.base.trash_time,
                        DegradedNode::File(n) | DegradedNode::Photo(n) => n.base.trash_time,
                    }
                    .is_some();
                    if !trashed {
                        let link = link_of(degraded.uid());
                        tracing::warn!(
                            %link,
                            folder = %folder,
                            "a child's metadata cannot be decrypted; what is known of it stays"
                        );
                        listing.degraded.push(link);
                    }
                }
            }
        }
        Ok(listing)
    }

    /// Fresh from the API, 50 links per request (P13): `enumerate_nodes`
    /// answers from the SDK's entity cache, which neither our own revisions
    /// nor other devices' changes update, so events would look like echoes
    /// (D035). `get_nodes_uncached` refreshes those cache entries on the way.
    /// Each node carries its name hash (P14), the precondition of `rename`.
    /// The API leaves out links that no longer exist; a node whose
    /// metadata cannot be decrypted is `None` here too
    /// ([`Backend::get_nodes_checked`] tells them apart).
    async fn get_nodes(&self, ids: &[LinkId]) -> Result<Vec<Option<RemoteNode>>> {
        Ok(self
            .get_nodes_checked(ids)
            .await?
            .into_iter()
            .map(NodeLookup::found)
            .collect())
    }

    /// [`Backend::get_nodes`], with a node the API returns but whose
    /// metadata cannot be decrypted `Degraded`, not absent (D119).
    async fn get_nodes_checked(&self, ids: &[LinkId]) -> Result<Vec<NodeLookup>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let fetched = self
            .authed(|client| async move {
                let links = ids
                    .iter()
                    .map(|id| proton_drive_sdk::links::LinkId::new(id.as_str().to_owned()))
                    .collect();
                client
                    .get_nodes_uncached_with_name_hashes(self.volume.clone(), links)
                    .await
                    .map_err(map_error)
            })
            .await?;
        let mut found: HashMap<LinkId, NodeLookup> = fetched
            .into_iter()
            .map(|(object, digest)| match object {
                PotentialObject::Node(node) => {
                    let node = RemoteNode {
                        name_hash: Some(hash_to_hex(&digest)),
                        ..convert_node(node)
                    };
                    (node.link_id.clone(), NodeLookup::Found(node))
                }
                PotentialObject::Degraded(degraded) => {
                    let link = link_of(degraded.uid());
                    tracing::warn!(
                        %link,
                        "a node's metadata cannot be decrypted; what is known of it stays"
                    );
                    (link, NodeLookup::Degraded)
                }
            })
            .collect();
        Ok(ids
            .iter()
            .map(|id| found.remove(id).unwrap_or(NodeLookup::Absent))
            .collect())
    }

    /// From the node's claimed block sizes when it has them for this
    /// revision, without a listing; otherwise the whole listing, verified,
    /// with the sizes of the listed revision (D135).
    async fn blocks(&self, file: &LinkId, revision: &RevisionId) -> Result<Vec<BlockInfo>> {
        let kept = self.block_lists.revision(file, revision);
        if let Some(sizes) = self.node_block_sizes(file, revision).await? {
            kept.set_count(sizes.len() as u32);
            return Ok(sizes
                .into_iter()
                .zip(1..)
                .map(|(size, index)| BlockInfo {
                    index,
                    plain_size: Some(size),
                })
                .collect());
        }
        let pages = self.pages(file, revision);
        let count = self.block_lists.verified(&kept, &pages).await?.len();
        let meta = kept.meta().ok_or(BackendError::NotFound)?;
        let uid = pages.uid.clone();
        let sizes = self
            .authed(|client| {
                let (uid, meta) = (uid.clone(), Arc::clone(&meta));
                async move {
                    client
                        .listed_block_sizes(&uid, &meta)
                        .await
                        .map_err(map_error)
                }
            })
            .await?
            .and_then(|sizes| {
                sizes
                    .iter()
                    .map(|&size| u32::try_from(size).ok())
                    .collect::<Option<Vec<u32>>>()
            })
            .filter(|sizes| {
                let matches = sizes.len() == count;
                if !matches {
                    tracing::warn!(
                        %file, %revision, sizes = sizes.len(), blocks = count,
                        "ignoring claimed block sizes that do not match the listing"
                    );
                }
                matches
            });
        Ok((1..=count as u32)
            .map(|index| BlockInfo {
                index,
                plain_size: sizes.as_ref().map(|sizes| sizes[index as usize - 1]),
            })
            .collect())
    }

    async fn fetch_block_into(
        &self,
        file: &LinkId,
        revision: &RevisionId,
        index: u32,
        buf: &mut [u8],
    ) -> Result<usize> {
        self.download_block_into(file, revision, index, buf).await
    }

    /// Into a buffer sized once, from the claimed size when there is one.
    async fn fetch_block(&self, file: &LinkId, revision: &RevisionId, index: u32) -> Result<Bytes> {
        let size = self
            .blocks(file, revision)
            .await?
            .iter()
            .find(|block| block.index == index)
            .ok_or(BackendError::NotFound)?
            .plain_size
            .map_or(crate::MAX_BLOCK_BYTES, |size| {
                (size as usize).min(crate::MAX_BLOCK_BYTES)
            });
        let mut buf = vec![0; size];
        let len = self
            .download_block_into(file, revision, index, &mut buf)
            .await?;
        buf.truncate(len);
        Ok(Bytes::from(buf))
    }

    async fn upload_new(
        &self,
        parent: &LinkId,
        name: &str,
        source: UploadSource,
        meta: FileMeta,
    ) -> Result<(LinkId, RevisionId)> {
        let result = self
            .authed(|client| {
                let source = source.clone();
                let meta = meta.clone();
                async move {
                    let (size, reader) = open_source(source).await?;
                    let mut uploader = client
                        .get_file_uploader(
                            self.node_uid(parent),
                            name.to_owned(),
                            meta.media_type,
                            size as i64,
                            Some(meta.mtime),
                            mode_metadata(meta.mode),
                            None,
                            false,
                        )
                        .await
                        .map_err(map_error)?;
                    if let Some(sha1) = meta.sha1 {
                        uploader.set_expected_sha1(sha1.to_vec());
                    }
                    Self::upload(uploader, reader).await
                }
            })
            .await?;
        Ok((
            link_of(&result.node_uid),
            RevisionId::new(result.revision_uid.revision_id.raw()),
        ))
    }

    async fn upload_revision(
        &self,
        file: &LinkId,
        base: &RevisionId,
        source: UploadSource,
        meta: FileMeta,
    ) -> Result<RevisionId> {
        let result = self
            .authed(|client| {
                let source = source.clone();
                let meta = meta.clone();
                async move {
                    let (size, reader) = open_source(source).await?;
                    let mut uploader = client
                        .get_file_revision_uploader(
                            self.revision_uid(file, base),
                            size as i64,
                            Some(meta.mtime),
                            mode_metadata(meta.mode),
                            None,
                        )
                        .await
                        .map_err(map_error)?;
                    if let Some(sha1) = meta.sha1 {
                        uploader.set_expected_sha1(sha1.to_vec());
                    }
                    Self::upload(uploader, reader).await
                }
            })
            .await?;
        Ok(RevisionId::new(result.revision_uid.revision_id.raw()))
    }

    async fn create_folder(
        &self,
        parent: &LinkId,
        name: &str,
        mtime: SystemTime,
    ) -> Result<LinkId> {
        let folder = self
            .authed(|client| async move {
                client
                    .create_folder(self.node_uid(parent), name.to_owned(), Some(mtime))
                    .await
                    .map_err(map_error)
            })
            .await?;
        Ok(link_of(&folder.base.uid))
    }

    async fn rename(
        &self,
        node: &LinkId,
        new_parent: Option<&LinkId>,
        new_name: &str,
        expected: Option<&NameHash>,
    ) -> Result<()> {
        let original = match expected {
            Some(hash) => Some(hash_from_hex(hash.as_str()).ok_or_else(|| {
                BackendError::Other(format!("malformed name hash {:?}", hash.as_str()))
            })?),
            None => None,
        };
        // A refusal (code 2000) comes back as its message's reading, the
        // last resort of `refused_move`.
        let refused = self
            .authed(|client| {
                let uid = self.node_uid(node);
                let new_name = new_name.to_owned();
                let original = original.clone();
                async move {
                    match (new_parent, original) {
                        (Some(parent), None) => {
                            client
                                .move_node(uid, self.node_uid(parent), Some(new_name))
                                .await
                        }
                        (Some(parent), Some(hash)) => {
                            client
                                .move_node_with_original_hash(
                                    uid,
                                    self.node_uid(parent),
                                    Some(new_name),
                                    hash,
                                )
                                .await
                        }
                        (None, None) => client.rename_node(uid, new_name, None).await,
                        (None, Some(hash)) => {
                            client
                                .rename_node_with_original_hash(uid, new_name, None, hash)
                                .await
                        }
                    }
                    .map_or_else(
                        |error| {
                            if is_refused(&error) {
                                Ok(Some(map_error(error)))
                            } else {
                                Err(map_error(error))
                            }
                        },
                        |()| Ok(None),
                    )
                }
            })
            .await?;
        match refused {
            None => Ok(()),
            Some(by_message) => {
                Err(refused_move(self, node, new_parent, new_name, expected, by_message).await)
            }
        }
    }

    async fn trash(&self, nodes: &[LinkId]) -> Result<Vec<Result<()>>> {
        let uids: Vec<NodeUid> = nodes.iter().map(|link| self.node_uid(link)).collect();
        let mut results = self
            .authed(|client| {
                let uids = uids.clone();
                async move { client.trash_nodes(uids).await.map_err(map_error) }
            })
            .await?;
        Ok(uids
            .iter()
            .map(|uid| match results.remove(uid) {
                Some(Ok(())) => Ok(()),
                Some(Err(error)) => Err(map_error(error)),
                None => Err(BackendError::Other(format!(
                    "the server did not report on trashing {}",
                    uid.link_id.raw()
                ))),
            })
            .collect())
    }

    async fn latest_event_id(&self) -> Result<EventId> {
        self.authed(|client| async move {
            client
                .get_volume_latest_event_id(self.volume.clone())
                .await
                .map(EventId::new)
                .map_err(map_error)
        })
        .await
    }

    async fn poll_events(&self, since: &EventId) -> Result<EventPage> {
        let response = self
            .authed(|client| async move {
                client
                    .poll_volume_events(self.volume.clone(), since.as_str())
                    .await
                    .map_err(map_error)
            })
            .await?;
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
            .authed(|client| async move { client.get_user_storage_info().await.map_err(map_error) })
            .await?;
        Ok(StorageInfo {
            used: u64::try_from(used).unwrap_or(0),
            max: u64::try_from(max).unwrap_or(0),
        })
    }
}

/// A share root's name, kind and the user's role; `None` for a trashed
/// root or one that is not a file or folder (Photos).
fn share_root(node: &Node) -> Option<(LinkId, String, NodeKind, ShareRole)> {
    let (base, kind) = match node {
        Node::Folder(folder) => (&folder.base, NodeKind::Folder),
        Node::File(file) => (&file.base.base, NodeKind::File),
        Node::Album(_) | Node::Photo(_) => return None,
    };
    if base.trash_time.is_some() {
        return None;
    }
    let role = match base.direct_role {
        NodeMemberRole::Admin => ShareRole::Admin,
        NodeMemberRole::Editor => ShareRole::Editor,
        // `Inherited` on a share's root would be the server's error: the
        // least role.
        NodeMemberRole::Viewer | NodeMemberRole::Inherited => ShareRole::Viewer,
    };
    Some((link_of(&base.uid), base.name.clone(), kind, role))
}

/// Shared-with-me as the official clients read it (M5.9): the listing's
/// pages, then each share's creator once (`get_share`), then the roots
/// fresh, one batch per volume, for their names and the user's role. A
/// share whose root is not returned (revoked meanwhile) is left out; one
/// whose root's metadata cannot be decrypted is listed as degraded (D119).
#[async_trait::async_trait]
impl SharedWithMe for ProtonBackend {
    async fn shared_with_me(&self) -> Result<Vec<SharedEntry>> {
        Ok(self.shared_listing().await?.entries)
    }

    async fn shared_listing(&self) -> Result<SharedListing> {
        let api = self
            .live
            .read()
            .shared
            .clone()
            .ok_or(BackendError::Unsupported(
                "shared-with-me without its client",
            ))?;
        let (_, credential) = self.session_for_call().await?;
        let links = shared_links(&api, &credential).await?;
        for (share, ..) in &links {
            if self.share_info.lock().contains_key(share) {
                continue;
            }
            let id = ShareId::new(share.clone());
            let fetched = self
                .authed(|client| {
                    let id = id.clone();
                    async move { client.api().shares().get_share(id).await.map_err(map_error) }
                })
                .await;
            match fetched {
                Ok(response) => {
                    self.share_info.lock().insert(
                        share.clone(),
                        ShareInfo {
                            creator: response.creator_email_address,
                            created: response.creation_time.map(system_time),
                        },
                    );
                }
                // Gone since the listing: left out below.
                Err(BackendError::NotFound | BackendError::PermissionDenied) => {}
                Err(error) => return Err(error),
            }
        }
        let mut by_volume: HashMap<String, Vec<String>> = HashMap::new();
        for (_, volume, link, _) in &links {
            by_volume
                .entry(volume.clone())
                .or_default()
                .push(link.clone());
        }
        let mut roots = HashMap::new();
        let mut unreadable = HashSet::new();
        for (volume, ids) in by_volume {
            let fetched = self
                .authed(|client| {
                    let volume = VolumeId::new(volume.clone());
                    let ids = ids
                        .iter()
                        .map(|id| proton_drive_sdk::links::LinkId::new(id.clone()))
                        .collect();
                    async move {
                        client
                            .get_nodes_uncached(volume, ids)
                            .await
                            .map_err(map_error)
                    }
                })
                .await?;
            for object in fetched {
                match object {
                    PotentialObject::Node(node) => {
                        if let Some((link, name, kind, role)) = share_root(&node) {
                            roots.insert((volume.clone(), link), (name, kind, role));
                        }
                    }
                    PotentialObject::Degraded(degraded) => {
                        let link = link_of(degraded.uid());
                        tracing::warn!(
                            %link,
                            "a shared root's metadata cannot be decrypted; listed as degraded"
                        );
                        unreadable.insert((volume.clone(), link));
                    }
                }
            }
        }
        let infos = self.share_info.lock().clone();
        let mut degraded = Vec::new();
        let entries = links
            .into_iter()
            .filter_map(|(share, volume, link, listed_kind)| {
                let link = LinkId::new(link);
                if unreadable.contains(&(volume.clone(), link.clone())) {
                    degraded.push(DegradedShare {
                        share,
                        volume,
                        root: link,
                    });
                    return None;
                }
                let (name, kind, role) = roots.remove(&(volume.clone(), link.clone()))?;
                let info = infos.get(&share)?;
                (kind == listed_kind).then(|| SharedEntry {
                    share,
                    volume,
                    root: link,
                    kind,
                    name,
                    owner: info.creator.clone(),
                    role,
                    created: info.created,
                })
            })
            .collect();
        Ok(SharedListing { entries, degraded })
    }

    async fn latest_core_event_id(&self) -> Result<EventId> {
        self.authed(|client| async move {
            client
                .get_core_latest_event_id()
                .await
                .map(EventId::new)
                .map_err(map_error)
        })
        .await
    }

    async fn poll_core_events(&self, since: &EventId) -> Result<CorePage> {
        let response = self
            .authed(|client| async move {
                client
                    .poll_core_events(since.as_str())
                    .await
                    .map_err(map_error)
            })
            .await?;
        Ok(CorePage {
            next: EventId::new(response.event_id),
            more: response.more != 0,
            refresh: response.refresh != 0,
            // Action 2 is the only one seen (M5.9); any action means the
            // listing changed.
            shares_changed: response.drive_share_refresh.is_some(),
        })
    }
}

/// Photos as the official Photos client reads them (M5.9, D113): the
/// Photos share's volume and root, and the timeline with each item's
/// related photos.
#[async_trait::async_trait]
impl Photos for ProtonBackend {
    async fn photos_volume(&self) -> Result<Option<PhotosVolume>> {
        match self.photos_ready().await {
            Ok((_, volume)) => Ok(Some(volume)),
            Err(BackendError::NotFound) => Ok(None),
            Err(error) => Err(error),
        }
    }

    async fn timeline_page(
        &self,
        volume: &str,
        after: Option<&LinkId>,
    ) -> Result<Vec<TimelineItem>> {
        let (photos, _) = self.photos_ready().await?;
        let page = with_reauth(&photos.credential, || async {
            photos.timeline.page(volume, after).await.map_err(map_error)
        })
        .await?;
        Ok(page
            .photos
            .into_iter()
            .map(TimelinePhotoDto::item)
            .collect())
    }

    /// One request (DESIGN §14.5, D135): the revision's first page of
    /// blocks, without their storage URLs. When that page is the whole
    /// listing (fewer than [`BLOCK_PAGE_SIZE`] blocks, a photo), its
    /// manifest is verified as `get_block_map` verifies it: the
    /// thumbnails' digests, then the blocks', signed by the node key or by
    /// the signer's address keys. A larger revision (a video) is not listed
    /// further for its thumbnail: its thumbnail is checked as the official
    /// clients check every thumbnail, by decrypting with the file's content
    /// key (the JS SDK's `thumbnailDownloader.ts` checks no digest; D115).
    async fn thumbnail_ref(
        &self,
        volume: &str,
        link: &LinkId,
        revision: &RevisionId,
    ) -> Result<Option<ThumbnailRef>> {
        let (photos, _) = self.photos_ready().await?;
        let credential = photos.credential.clone();
        let uid = RevisionUid::new(
            NodeUid::from_parts(volume, link.as_str()),
            proton_drive_sdk::revision::RevisionId::new(revision.as_str().to_owned()),
        );
        let (drive, account) = (photos.drive(), &photos.account);
        let listed = with_reauth(&credential, || async {
            let secrets = FileOperations::get_secrets(drive, uid.node_uid.clone())
                .await
                .map_err(map_error)?;
            let listing = drive
                .api()
                .files()
                .get_revision(
                    uid.node_uid.volume_id.clone(),
                    uid.node_uid.link_id.clone(),
                    uid.revision_id.clone(),
                    Some(1),
                    Some(BLOCK_PAGE_SIZE as i32),
                    true,
                )
                .await
                .map_err(map_error)?
                .revision;
            if listing.blocks.len() >= BLOCK_PAGE_SIZE as usize {
                tracing::debug!(%link, "a large revision's thumbnail: its manifest is not listed");
                return Ok(listing
                    .revision
                    .thumbnails
                    .into_iter()
                    .flatten()
                    .find(|thumbnail| thumbnail.r#type == ThumbnailType::Thumbnail));
            }
            // The keys of the SDK's `manifest_verification_keys`.
            let keys = match listing.revision.signature_email_address.as_deref() {
                None | Some("") => vec![secrets.base.key.0.as_public_key().clone()],
                Some(email) => account
                    .get_address_public_keys(email)
                    .await
                    .map_err(map_error)?,
            };
            verify_block_listing(&listing, &keys)
                .map_err(|error| map_error(error.context("the thumbnail's revision manifest")))?;
            Ok(listing
                .revision
                .thumbnails
                .into_iter()
                .flatten()
                .find(|thumbnail| thumbnail.r#type == ThumbnailType::Thumbnail))
        })
        .await?;
        Ok(listed.map(|thumbnail| ThumbnailRef {
            link: link.clone(),
            revision: revision.clone(),
            id: thumbnail.id,
            digest: thumbnail.hash_digest,
            size: u64::try_from(thumbnail.size_on_cloud_storage).unwrap_or(u64::MAX),
        }))
    }

    async fn fetch_thumbnails(
        &self,
        volume: &str,
        thumbnails: &[ThumbnailRef],
    ) -> Result<Vec<Result<Bytes>>> {
        use futures::StreamExt;
        if thumbnails.is_empty() {
            return Ok(Vec::new());
        }
        let (photos, _) = self.photos_ready().await?;
        let credential = photos.credential.clone();
        let drive = photos.drive();
        let ids: Vec<String> = thumbnails.iter().map(|t| t.id.clone()).collect();
        let listed = with_reauth(&credential, || async {
            drive
                .api()
                .files()
                .get_thumbnail_blocks(VolumeId::new(volume.to_owned()), ids.clone())
                .await
                .map_err(map_error)
        })
        .await?;
        let urls: HashMap<String, (String, String)> = listed
            .blocks
            .into_iter()
            .map(|block| (block.thumbnail_id, (block.bare_url, block.token)))
            .collect();
        let downloads = thumbnails.iter().cloned().map(|thumbnail| {
            let url = urls.get(&thumbnail.id).cloned();
            async move {
                let (url, token) = url.ok_or(BackendError::NotFound)?;
                download_thumbnail(drive, volume, &thumbnail, &url, &token).await
            }
        });
        Ok(futures::stream::iter(downloads)
            .buffered(THUMBNAIL_DOWNLOADS)
            .collect()
            .await)
    }
}

/// Thumbnails downloaded at once: the SDK's `enumerate_thumbnails` (8),
/// below the official web client's 10.
const THUMBNAIL_DOWNLOADS: usize = 8;

/// One thumbnail: at most [`MAX_THUMBNAIL_BYTES`] from storage, whose
/// SHA-256 must be the one the revision's verified manifest lists, then
/// decrypted with the file's content key.
async fn download_thumbnail(
    drive: &ProtonDriveClient,
    volume: &str,
    thumbnail: &ThumbnailRef,
    url: &str,
    token: &str,
) -> Result<Bytes> {
    use sha2::Digest;
    let too_large = |size: u64| {
        BackendError::Integrity(format!(
            "the thumbnail of {} has {size} bytes, more than a thumbnail's {MAX_THUMBNAIL_BYTES}",
            thumbnail.link
        ))
    };
    if thumbnail.size > MAX_THUMBNAIL_BYTES as u64 {
        return Err(too_large(thumbnail.size));
    }
    let mut response = drive
        .api()
        .storage()
        .get_blob_stream(url, token)
        .await
        .map_err(map_error)?;
    let mut blob = Vec::with_capacity(thumbnail.size as usize);
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| map_error(anyhow::Error::from(error).context("thumbnail download")))?
    {
        if blob.len() + chunk.len() > MAX_THUMBNAIL_BYTES {
            return Err(too_large((blob.len() + chunk.len()) as u64));
        }
        blob.extend_from_slice(&chunk);
    }
    if sha2::Sha256::digest(&blob).as_slice() != thumbnail.digest.as_slice() {
        return Err(BackendError::Integrity(format!(
            "the thumbnail of {} is not the one its revision's manifest signs",
            thumbnail.link
        )));
    }
    let secrets =
        FileOperations::get_secrets(drive, NodeUid::from_parts(volume, thumbnail.link.as_str()))
            .await
            .map_err(map_error)?;
    let plain = secrets.content_key.decrypt(&blob).map_err(|error| {
        BackendError::Integrity(format!(
            "the thumbnail of {} does not decrypt: {error:#}",
            thumbnail.link
        ))
    })?;
    if plain.len() > MAX_THUMBNAIL_BYTES {
        return Err(too_large(plain.len() as u64));
    }
    Ok(Bytes::from(plain))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// D132 (fork P27): a node whose name signature does not verify is
    /// logged once and converted with its failed part and claimed signer
    /// (M6.0.6); a verified node has none.
    #[test]
    fn an_unverified_node_is_logged_once_and_shown() {
        use proton_drive_sdk::node::NodeBase;
        use proton_drive_sdk::node::folder::FolderNode;
        let folder = |link: &str, verified: bool| {
            Node::Folder(FolderNode {
                base: NodeBase {
                    uid: NodeUid::from_parts("volume", link),
                    parent_uid: Some(NodeUid::from_parts("volume", "parent")),
                    name: link.to_owned(),
                    name_author: if verified {
                        PotentialObject::Node(Default::default())
                    } else {
                        PotentialObject::Degraded(
                            proton_drive_sdk::protobuf::SignatureVerificationError {
                                claimed_author: None,
                                message: "Could not verify the author of this item".into(),
                            },
                        )
                    },
                    ..NodeBase::default()
                },
                claimed_modification_time: None,
            })
        };
        let unverified = folder("p27-unverified", false);
        assert_eq!(
            unverified_signatures(&unverified),
            [(SignaturePart::Name, None)]
        );
        let converted = convert_node(unverified.clone());
        assert_eq!(converted.name, "p27-unverified");
        let found = converted.unverified.expect("unverified");
        assert_eq!(found.parts(), [SignaturePart::Name]);
        assert_eq!(found.signer, "anonymous");
        assert!(
            !warn_unverified(unverified.uid(), &found),
            "logged once per node, by the conversion"
        );
        assert_eq!(convert_node(folder("p27-verified", true)).unverified, None);
    }

    /// The SDK's own path: an active key that no stored passphrase opens.
    mod locked_keys {
        use proton_drive_sdk::proton_sdk_rs2::account::ProtonAccountClient;
        use proton_drive_sdk::proton_sdk_rs2::addresses::AddressesApiClient;
        use proton_drive_sdk::proton_sdk_rs2::api::cache::DefaultAccountClientCache;
        use proton_drive_sdk::proton_sdk_rs2::api::client::{
            AccountApiClients, DefaultAccountApiClients,
        };
        use proton_drive_sdk::proton_sdk_rs2::cache::InMemoryCacheRepository;
        use proton_drive_sdk::proton_sdk_rs2::keys::KeysApiClient;
        use proton_drive_sdk::proton_sdk_rs2::secret::SessionSecretCache;
        use proton_drive_sdk::proton_sdk_rs2::users::{UserResponse, UsersApiClient};
        use std::sync::Arc;

        struct OneLockedKey;

        #[async_trait::async_trait]
        impl UsersApiClient for OneLockedKey {
            async fn get_user(&self) -> anyhow::Result<UserResponse> {
                Ok(serde_json::from_value(serde_json::json!({
                    "Code": 1000,
                    "User": {"Keys": [{"ID": "k", "PrivateKey": "not armored", "Active": 1}]},
                }))?)
            }
        }

        struct Clients(DefaultAccountApiClients);

        impl AccountApiClients for Clients {
            fn keys(&self) -> Arc<dyn KeysApiClient> {
                self.0.keys()
            }
            fn users(&self) -> Arc<dyn UsersApiClient> {
                Arc::new(OneLockedKey)
            }
            fn addresses(&self) -> Arc<dyn AddressesApiClient> {
                self.0.addresses()
            }
        }

        struct NoPassphrase;

        #[async_trait::async_trait]
        impl SessionSecretCache for NoPassphrase {
            async fn set_account_key_passphrase(
                &self,
                _key_id: &String,
                _passphrase: &[u8],
            ) -> anyhow::Result<()> {
                Ok(())
            }
            async fn try_get_account_key_passphrase(
                &self,
                _key_id: &String,
            ) -> anyhow::Result<Option<Vec<u8>>> {
                Ok(None)
            }
        }

        pub async fn error() -> anyhow::Error {
            let client = ProtonAccountClient {
                api: Arc::new(Clients(DefaultAccountApiClients::new(
                    reqwest::Client::new(),
                ))),
                cache: Arc::new(DefaultAccountClientCache::new(
                    Arc::new(InMemoryCacheRepository::new()),
                    Arc::new(InMemoryCacheRepository::new()),
                    Arc::new(NoPassphrase),
                )),
            };
            client.get_user_keys().await.expect_err("locked")
        }
    }

    #[tokio::test]
    async fn keys_that_cannot_be_unlocked_ask_for_a_login() {
        assert_eq!(
            map_error(locked_keys::error().await),
            BackendError::Unauthorized
        );
        assert_eq!(
            map_error(locked_keys::error().await.context("get my files")),
            BackendError::Unauthorized,
            "with context around it"
        );
    }

    /// A block longer than the buffer is told apart from other failures.
    #[tokio::test]
    async fn a_full_block_buffer_is_an_overflow() {
        use tokio::io::AsyncWriteExt;
        let mut buf = [0u8; 2];
        let mut out = std::io::Cursor::new(&mut buf[..]);
        let error = out.write_all(b"abc").await.expect_err("full");
        assert!(overflowed(&anyhow::Error::from(error)));
        assert!(!overflowed(&anyhow::anyhow!("other")));
    }

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
    fn refused_renames_and_moves_are_told_apart_by_message() {
        let refused = |status: Option<u16>, message: &str| -> anyhow::Error {
            let api = match status {
                Some(status) => ApiError::from_http(
                    reqwest::StatusCode::from_u16(status).expect("status"),
                    &reqwest::header::HeaderMap::new(),
                    &format!(r#"{{"Code":2000,"Error":"{message}"}}"#),
                ),
                None => ApiError::from_code(2000, message),
            };
            anyhow::Error::from(api).context("while renaming")
        };
        // Rename: HTTP 422. Move: a 2xx body with an error code.
        for status in [Some(422), None] {
            assert_eq!(
                map_error(refused(
                    status,
                    "This file or folder was out of date, rename failed."
                )),
                BackendError::NameConflict
            );
        }
        assert_eq!(
            map_error(refused(
                None,
                "This file or folder was out of date, move failed."
            )),
            BackendError::NameConflict
        );
        assert_eq!(
            map_error(refused(None, "Cannot move a folder into a sub-folder")),
            BackendError::InvalidMove
        );
        assert!(matches!(
            map_error(refused(Some(422), "something else")),
            BackendError::Other(_)
        ));
        assert!(is_refused(&refused(Some(422), "reworded")));
        assert!(is_refused(&refused(None, "reworded")));
        assert!(!is_refused(&anyhow::Error::from(ApiError::from_code(
            2500, "exists"
        ))));
    }

    /// A refusal (code 2000) is decided by the node's state on the fake, as
    /// the server would have it, never by the message (N2); `by_message`
    /// is a reading that is wrong for each case.
    #[tokio::test]
    async fn refused_renames_and_moves_are_decided_by_the_nodes_state() {
        use crate::fake::{FakeBackend, FakeOp};
        let fake = FakeBackend::new();
        let root = fake.root().await.expect("root").link_id;
        let folder = |parent: LinkId, name: &'static str| {
            let fake = fake.clone();
            async move {
                fake.create_folder(&parent, name, SystemTime::UNIX_EPOCH)
                    .await
                    .expect("folder")
            }
        };
        let (a, other) = (
            folder(root.clone(), "a").await,
            folder(root.clone(), "other").await,
        );
        let b = folder(a.clone(), "b").await;
        let c = folder(b.clone(), "c").await;
        let file = |name: &'static str| {
            let (fake, root) = (fake.clone(), root.clone());
            async move {
                fake.upload_new(
                    &root,
                    name,
                    UploadSource::Bytes(Bytes::from_static(b"x")),
                    FileMeta {
                        mtime: SystemTime::UNIX_EPOCH,
                        mode: None,
                        media_type: "text/plain".into(),
                        sha1: None,
                    },
                )
                .await
                .expect("file")
                .0
            }
        };
        let (f, taken) = (file("f").await, file("taken").await);
        let hash = |link: LinkId| {
            let fake = fake.clone();
            async move {
                fake.get_nodes(&[link]).await.expect("get")[0]
                    .as_ref()
                    .and_then(|node| node.name_hash.clone())
                    .expect("hash")
            }
        };
        let decide = |node: LinkId, parent: Option<LinkId>, name: &'static str, expected| {
            let fake = fake.clone();
            async move {
                refused_move(
                    &fake,
                    &node,
                    parent.as_ref(),
                    name,
                    Some(&expected),
                    BackendError::Other("the message".into()),
                )
                .await
            }
        };
        let (hash_a, hash_f) = (hash(a.clone()).await, hash(f.clone()).await);

        // A move into the node's own subtree, or into itself.
        assert_eq!(
            decide(a.clone(), Some(c.clone()), "a", hash_a.clone()).await,
            BackendError::InvalidMove
        );
        assert_eq!(
            decide(a.clone(), Some(a.clone()), "a", hash_a.clone()).await,
            BackendError::InvalidMove
        );
        // The name is taken in the target folder, by a rename or a move.
        assert_eq!(
            decide(f.clone(), None, "taken", hash_f.clone()).await,
            BackendError::AlreadyExists {
                existing: Some(taken.clone())
            }
        );
        let in_other = fake
            .upload_new(
                &other,
                "x",
                UploadSource::Bytes(Bytes::from_static(b"y")),
                FileMeta {
                    mtime: SystemTime::UNIX_EPOCH,
                    mode: None,
                    media_type: "text/plain".into(),
                    sha1: None,
                },
            )
            .await
            .expect("x")
            .0;
        assert_eq!(
            decide(f.clone(), Some(other.clone()), "x", hash_f.clone()).await,
            BackendError::AlreadyExists {
                existing: Some(in_other)
            }
        );
        // A move that is no cycle and finds its name free: an unchanged
        // node whose hash was refused.
        assert_eq!(
            decide(f.clone(), Some(c.clone()), "f", hash_f.clone()).await,
            BackendError::NameConflict
        );
        // Renamed meanwhile (another device, or this one's lost answer).
        fake.rename(&f, None, "g", None).await.expect("rename");
        assert_eq!(
            decide(f.clone(), None, "h", hash_f.clone()).await,
            BackendError::NameConflict
        );
        // The target folder is gone, then the node itself.
        let hash_g = hash(f.clone()).await;
        fake.trash(std::slice::from_ref(&other))
            .await
            .expect("trash");
        assert_eq!(
            decide(f.clone(), Some(other.clone()), "g", hash_g.clone()).await,
            BackendError::NotFound
        );
        fake.trash(std::slice::from_ref(&f)).await.expect("trash");
        assert_eq!(
            decide(f.clone(), None, "h", hash_g.clone()).await,
            BackendError::NotFound
        );
        // A read that fails leaves it to the message.
        fake.inject_failure(FakeOp::GetNodes, BackendError::Offline("down".into()));
        assert_eq!(
            decide(a.clone(), Some(c.clone()), "a", hash_a.clone()).await,
            BackendError::Other("the message".into())
        );
        // So does a listing that fails.
        fake.inject_failure(FakeOp::ListChildren, BackendError::Offline("down".into()));
        assert_eq!(
            decide(b.clone(), None, "taken", hash(b.clone()).await).await,
            BackendError::Other("the message".into())
        );
    }

    #[test]
    fn a_draft_removed_before_the_commit_is_transient() {
        let error = api_error(
            422,
            r#"{"Code":2511,"Error":"Given revision is not in draft"}"#,
        );
        assert!(matches!(
            map_error(error.context("while committing")),
            BackendError::Transient(_)
        ));
    }

    #[test]
    fn name_hashes_are_lowercase_hex_and_round_trip() {
        let digest = [0x00, 0x0a, 0xab, 0xff, 0x10];
        let hash = hash_to_hex(&digest);
        assert_eq!(hash.as_str(), "000aabff10");
        assert_eq!(hash_from_hex(hash.as_str()), Some(digest.to_vec()));
        assert_eq!(hash_from_hex("ABff"), Some(vec![0xab, 0xff]));
        assert_eq!(hash_from_hex("abc"), None);
        assert_eq!(hash_from_hex("zz"), None);
        assert_eq!(hash_from_hex("é"), None);
        assert_eq!(hash_from_hex(""), Some(Vec::new()));
    }

    #[test]
    fn a_200_body_with_code_401_is_unauthorized() {
        let body_401: anyhow::Error = ApiError::from_code(401, "Invalid access token").into();
        assert_eq!(
            map_error(body_401.context("while listing")),
            BackendError::Unauthorized
        );
        // Other body codes without an HTTP status stay unclassified.
        let other: anyhow::Error = ApiError::from_code(2001, "bad").into();
        assert!(matches!(map_error(other), BackendError::Other(_)));
    }

    /// Nothing listens on port 1: the connection is refused at once.
    const REFUSED: &str = "http://127.0.0.1:1/";

    #[tokio::test]
    async fn connection_errors_through_the_middleware_are_offline() {
        let client = reqwest_middleware::ClientBuilder::new(reqwest::Client::new()).build();
        let failure = client.get(REFUSED).send().await.expect_err("refused");
        // The middleware error hides the reqwest error from the chain.
        let error = anyhow::Error::from(failure).context("while listing");
        assert!(
            error
                .chain()
                .all(|cause| cause.downcast_ref::<reqwest::Error>().is_none()),
            "the chain exposes the reqwest error, so the helper is not needed"
        );
        assert!(matches!(map_error(error), BackendError::Offline(_)));
    }

    #[tokio::test]
    async fn plain_reqwest_connection_errors_are_offline() {
        let failure = reqwest::get(REFUSED).await.expect_err("refused");
        let error = anyhow::Error::from(failure).context("while downloading");
        assert!(matches!(map_error(error), BackendError::Offline(_)));
    }

    #[tokio::test]
    async fn timeouts_are_transient() {
        // Accepts the connection and never answers.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let url = format!("http://{}/", listener.local_addr().expect("address"));
        let _held = tokio::spawn(async move {
            let _connection = listener.accept().await;
            std::future::pending::<()>().await;
        });
        let http = reqwest::Client::builder()
            .timeout(Duration::from_millis(100))
            .build()
            .expect("client");
        let plain = http.get(&url).send().await.expect_err("timeout");
        assert!(matches!(
            map_error(anyhow::Error::from(plain)),
            BackendError::Transient(_)
        ));
        let wrapped = reqwest_middleware::ClientBuilder::new(http).build();
        let failure = wrapped.get(&url).send().await.expect_err("timeout");
        assert!(matches!(
            map_error(anyhow::Error::from(failure)),
            BackendError::Transient(_)
        ));
    }

    /// A connection that breaks before the answer is complete is retried, not
    /// a permanent failure.
    #[tokio::test]
    async fn connections_that_break_mid_request_are_transient() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        // Closes after reading the request, before any answer.
        let closing = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let closing_url = format!("http://{}/", closing.local_addr().expect("address"));
        tokio::spawn(async move {
            if let Ok((mut socket, _)) = closing.accept().await {
                let mut request = [0u8; 1024];
                let _ = socket.read(&mut request).await;
            }
        });
        let failure = reqwest::get(&closing_url).await.expect_err("closed");
        assert!(
            matches!(
                map_error(anyhow::Error::from(failure)),
                BackendError::Transient(_)
            ),
            "closed before the answer"
        );
        // Promises 100 bytes, sends 10, closes.
        let short = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let short_url = format!("http://{}/", short.local_addr().expect("address"));
        tokio::spawn(async move {
            if let Ok((mut socket, _)) = short.accept().await {
                let mut request = [0u8; 1024];
                let _ = socket.read(&mut request).await;
                let _ = socket
                    .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 100\r\n\r\n0123456789")
                    .await;
            }
        });
        let answer = reqwest::get(&short_url).await.expect("headers");
        let failure = answer.bytes().await.expect_err("cut short");
        assert!(
            matches!(
                map_error(anyhow::Error::from(failure)),
                BackendError::Transient(_)
            ),
            "body cut short"
        );
    }

    #[test]
    fn block_transfer_errors_are_classified() {
        let transfer = |status: u16, retry_after| -> anyhow::Error {
            HttpTransferError::new(
                reqwest::StatusCode::from_u16(status).expect("status"),
                "blob",
                retry_after,
            )
            .into()
        };
        let wait = Some(Duration::from_secs(7));
        assert_eq!(
            map_error(transfer(429, wait).context("block 3")),
            BackendError::RateLimited { retry_after: wait }
        );
        for status in [408, 425, 500, 503] {
            assert!(
                matches!(
                    map_error(transfer(status, None)),
                    BackendError::Transient(_)
                ),
                "{status}"
            );
        }
        assert_eq!(map_error(transfer(401, None)), BackendError::Unauthorized);
        for status in [400, 404] {
            assert!(
                matches!(map_error(transfer(status, None)), BackendError::Other(_)),
                "{status}"
            );
        }
        let throttled: anyhow::Error = TooManyRequestsException { retry_after: wait }.into();
        assert_eq!(
            map_error(throttled.context("block 1")),
            BackendError::RateLimited { retry_after: wait }
        );
        let unhinted: anyhow::Error = TooManyRequestsException { retry_after: None }.into();
        assert_eq!(
            map_error(unhinted),
            BackendError::RateLimited { retry_after: None }
        );
    }

    #[test]
    fn the_block_sizes_decide_a_file_size() {
        // A claimed size no block list backs (S-5).
        let block = 4 << 20;
        assert_eq!(
            file_size(Some(1 << 62), Some(&[block, 1])),
            Some((4 << 20) + 1)
        );
        assert_eq!(file_size(Some(7), None), Some(7));
        assert_eq!(file_size(Some(7), Some(&[])), Some(7));
        assert_eq!(file_size(None, Some(&[3, 4])), Some(7));
        assert_eq!(file_size(Some(-1), None), None);
        assert_eq!(
            file_size(None, Some(&[i32::MAX; 4])),
            Some(4 * u64::from(i32::MAX as u32))
        );
    }

    /// A revision that claims neither a size nor block sizes (fork P4 as
    /// amended in M6.0.4.11) shows its encrypted size, logged once.
    #[test]
    fn a_file_without_a_claimed_size_shows_its_encrypted_size() {
        let uid = NodeUid::from_parts("volume", "p4-no-size");
        assert_eq!(file_size(None, None), None);
        assert_eq!(estimated_size(&uid, 5_100_000), Some(5_100_000));
        assert!(!first_for_node(&ESTIMATED_SIZES, &uid), "logged once");
        assert_eq!(estimated_size(&uid, -1), None);
    }

    /// Fork P2 amended (M6.0.4.12): a block still missing after its target
    /// was refreshed comes as a `DataIntegrityException` and reads as an
    /// integrity error (EIO, not cached, logged at ERROR); before, the raw
    /// 404 was a plain failure.
    #[test]
    fn a_block_missing_after_its_refresh_is_an_integrity_error() {
        let missing: anyhow::Error = DataIntegrityException::new(
            "File contents are incomplete: block #3 is still missing after its target was refreshed",
        )
        .into();
        assert!(matches!(
            map_error(missing.context("block 3")),
            BackendError::Integrity(_)
        ));
        let raw: anyhow::Error =
            HttpTransferError::new(reqwest::StatusCode::NOT_FOUND, "blob", None).into();
        assert!(matches!(map_error(raw), BackendError::Other(_)));
    }

    #[test]
    fn integrity_and_unknown_errors_are_classified() {
        let integrity: anyhow::Error = DataIntegrityException::new("hash mismatch").into();
        assert!(matches!(map_error(integrity), BackendError::Integrity(_)));
        // A block map whose listing fails the manifest check (P17): every
        // read of that revision is EIO.
        let manifest: anyhow::Error =
            CompletedDownloadManifestVerificationException::new("not signed").into();
        assert!(matches!(
            map_error(manifest.context("get_block_map")),
            BackendError::Integrity(_)
        ));
        // A block that does not decrypt to its plaintext with the
        // verification session key, after one more encryption (P20): the
        // upload is retried and parked as for any integrity failure.
        let block: anyhow::Error =
            proton_drive_sdk::error::IntegrityException::new("block failed verification").into();
        assert!(matches!(
            map_error(block.context("Block 3 failed verification after 2 encryptions")),
            BackendError::Integrity(_)
        ));
        assert!(matches!(
            map_error(anyhow::anyhow!("something else")),
            BackendError::Other(_)
        ));
    }

    /// A credential whose refreshes succeed and are counted.
    #[derive(Default)]
    struct MockReauth {
        refreshes: std::sync::atomic::AtomicU32,
    }

    impl MockReauth {
        fn refreshes(&self) -> u32 {
            self.refreshes.load(std::sync::atomic::Ordering::Relaxed)
        }
    }

    #[async_trait::async_trait]
    impl Reauth for MockReauth {
        async fn access_token(&self) -> anyhow::Result<String> {
            Ok(format!("token{}", self.refreshes()))
        }

        async fn refreshed(&self, rejected: String) -> anyhow::Result<String> {
            assert_eq!(rejected, format!("token{}", self.refreshes()));
            let n = self
                .refreshes
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(format!("token{}", n + 1))
        }
    }

    /// Calls that answer from a script, counting how often they ran.
    fn scripted(
        answers: Vec<Result<u32>>,
    ) -> (
        Arc<std::sync::atomic::AtomicU32>,
        impl Fn() -> std::future::Ready<Result<u32>>,
    ) {
        let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let seen = Arc::clone(&calls);
        let call = move || {
            let at = seen.fetch_add(1, std::sync::atomic::Ordering::Relaxed) as usize;
            std::future::ready(answers[at.min(answers.len() - 1)].clone())
        };
        (calls, call)
    }

    fn call_count(calls: &std::sync::atomic::AtomicU32) -> u32 {
        calls.load(std::sync::atomic::Ordering::Relaxed)
    }

    #[tokio::test]
    async fn a_rejected_token_is_refreshed_once_and_the_call_repeated() {
        let reauth = MockReauth::default();
        let (calls, call) = scripted(vec![Err(BackendError::Unauthorized), Ok(7)]);
        assert_eq!(with_reauth(&reauth, call).await, Ok(7));
        assert_eq!(reauth.refreshes(), 1);
        assert_eq!(call_count(&calls), 2);
    }

    #[tokio::test]
    async fn a_second_rejection_is_unauthorized_after_one_retry() {
        let reauth = MockReauth::default();
        let (calls, call) = scripted(vec![Err(BackendError::Unauthorized)]);
        assert_eq!(
            with_reauth(&reauth, call).await,
            Err(BackendError::Unauthorized)
        );
        assert_eq!(reauth.refreshes(), 1);
        assert_eq!(call_count(&calls), 2);
    }

    #[tokio::test]
    async fn other_answers_pass_without_a_refresh() {
        let reauth = MockReauth::default();
        let (calls, call) = scripted(vec![Err(BackendError::NotFound)]);
        assert_eq!(
            with_reauth(&reauth, call).await,
            Err(BackendError::NotFound)
        );
        let (_, call) = scripted(vec![Ok(1)]);
        assert_eq!(with_reauth(&reauth, call).await, Ok(1));
        assert_eq!(reauth.refreshes(), 0);
        assert_eq!(call_count(&calls), 1);
    }

    /// A credential whose refresh fails with `error`.
    struct FailingReauth(fn() -> anyhow::Error);

    #[async_trait::async_trait]
    impl Reauth for FailingReauth {
        async fn access_token(&self) -> anyhow::Result<String> {
            Ok("token".into())
        }

        async fn refreshed(&self, _: String) -> anyhow::Result<String> {
            Err((self.0)())
        }
    }

    #[tokio::test]
    async fn a_refresh_the_server_refuses_is_unauthorized() {
        let reauth = FailingReauth(|| api_error(401, "{}").context("refreshing the token"));
        let (calls, call) = scripted(vec![Err(BackendError::Unauthorized)]);
        assert_eq!(
            with_reauth(&reauth, call).await,
            Err(BackendError::Unauthorized)
        );
        assert_eq!(call_count(&calls), 1, "no retry without a new token");
    }

    #[tokio::test]
    async fn a_failed_refresh_that_is_not_a_refusal_is_transient() {
        let reauth = FailingReauth(|| api_error(503, "busy"));
        let (_, call) = scripted(vec![Err(BackendError::Unauthorized)]);
        assert!(matches!(
            with_reauth(&reauth, call).await,
            Err(BackendError::Transient(_))
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

    /// One photo with a tag the SDK's enum does not know (10 and up) or a
    /// capture time chrono cannot hold failed the SDK's whole page, and so
    /// the timeline and the Photos events for good; PdMount's own types
    /// read every item (D119).
    #[test]
    fn a_timeline_page_with_new_tags_and_far_capture_times_is_read() {
        let body = r#"{"Code":1000,"Photos":[
            {"LinkID":"a","CaptureTime":1700000000,"Hash":"h1","ContentHash":null,
             "Tags":[0,3,10,300,-1],
             "RelatedPhotos":[{"LinkID":"v","CaptureTime":-99999999999999999,"Hash":"h2"}]},
            {"LinkID":"b","CaptureTime":99999999999999999,"Hash":"h3","Tags":[]},
            {"LinkID":"c","CaptureTime":1.7e9,"Hash":"h4"},
            {"LinkID":"d","CaptureTime":null,"Hash":"h5"},
            {"LinkID":"e","CaptureTime":18446744073709551615,"Hash":"h6"}]}"#;
        assert!(
            serde_json::from_str::<proton_drive_sdk::api::file::photos::TimelinePhotoListResponse>(
                body
            )
            .is_err(),
            "the SDK's types refuse the page (else this test needs a new case)"
        );
        let page = parse_timeline(reqwest::StatusCode::OK, &Default::default(), body)
            .expect("the page is read");
        let items: Vec<TimelineItem> = page
            .photos
            .into_iter()
            .map(TimelinePhotoDto::item)
            .collect();
        let links: Vec<&str> = items.iter().map(|item| item.link.as_str()).collect();
        assert_eq!(links, ["a", "b", "c", "d", "e"]);
        assert_eq!(
            items[0].tags,
            [0, 3, 10],
            "known and new tags; none over 255"
        );
        assert_eq!(items[0].capture_time, 1_700_000_000);
        assert_eq!(
            items[0].related,
            [RelatedPhoto {
                link: LinkId::new("v"),
                capture_time: -99_999_999_999_999_999,
            }]
        );
        assert_eq!(items[1].capture_time, 99_999_999_999_999_999);
        assert_eq!(items[2].capture_time, 1_700_000_000);
        assert_eq!(items[3].capture_time, 0);
        assert_eq!(items[4].capture_time, i64::MAX);
    }

    /// An error answer is the API's error, never an empty page: an empty
    /// first page would end the timeline with nothing in it.
    #[test]
    fn a_timeline_error_is_never_an_empty_page() {
        let read = |status: u16, body: &str| {
            parse_timeline(
                reqwest::StatusCode::from_u16(status).expect("status"),
                &Default::default(),
                body,
            )
            .map(|page| page.photos.len())
            .map_err(map_error)
        };
        assert_eq!(
            read(200, r#"{"Code":2501,"Error":"Volume not found"}"#),
            Err(BackendError::NotFound)
        );
        assert_eq!(
            read(422, r#"{"Code":2501,"Error":"Volume not found"}"#),
            Err(BackendError::NotFound)
        );
        assert!(matches!(
            read(429, r#"{"Code":2028,"Error":"Too many requests"}"#),
            Err(BackendError::RateLimited { .. })
        ));
        assert!(read(200, r#"{"Code":1000}"#).is_err(), "no Photos, no page");
        assert!(read(200, "not json").is_err());
        assert_eq!(read(200, r#"{"Code":1000,"Photos":[]}"#), Ok(0));
    }

    /// A local HTTP server that answers its connections in order, each with
    /// the next of `answers` (a whole raw response), and closes them; it
    /// returns its URL and the request lines it got.
    async fn serve(answers: Vec<Vec<u8>>) -> (String, Arc<Mutex<Vec<String>>>) {
        let (listener, url) = listen().await;
        (url, serve_on(listener, answers))
    }

    /// A local listener and its URL, for [`serve_on`].
    async fn listen() -> (tokio::net::TcpListener, String) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let url = format!("http://{}/", listener.local_addr().expect("address"));
        (listener, url)
    }

    /// [`serve`] on a listener whose URL the answers need.
    fn serve_on(
        listener: tokio::net::TcpListener,
        answers: Vec<Vec<u8>>,
    ) -> Arc<Mutex<Vec<String>>> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&requests);
        tokio::spawn(async move {
            for answer in answers {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let mut head = Vec::new();
                let mut chunk = [0u8; 4096];
                while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                    match socket.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => head.extend_from_slice(&chunk[..n]),
                    }
                }
                let line = String::from_utf8_lossy(&head)
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .to_owned();
                seen.lock().push(line);
                let _ = socket.write_all(&answer).await;
                let _ = socket.shutdown().await;
            }
        });
        requests
    }

    /// A complete HTTP answer with `status` and `body`.
    fn answer(status: &str, body: &[u8]) -> Vec<u8> {
        let mut answer = format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        answer.extend_from_slice(body);
        answer
    }

    /// A shared-with-me page with a link of a type nobody knows yet, and
    /// types PdMount does not mount, keeps its folders and files: the SDK's
    /// types refused the whole page, and a Shared mount stayed empty (F1).
    #[test]
    fn shared_links_of_unknown_types_are_left_out_alone() {
        let body = r#"{"Code":1000,"More":false,"AnchorID":null,"Links":[
            {"VolumeID":"v1","ShareID":"s1","LinkID":"l1","ShareTargetType":1},
            {"VolumeID":"v1","ShareID":"s2","LinkID":"l2","ShareTargetType":9},
            {"VolumeID":"v2","ShareID":"s3","LinkID":"l3","ShareTargetType":2},
            {"VolumeID":"v2","ShareID":"s4","LinkID":"l4","ShareTargetType":"new"},
            {"VolumeID":"v2","ShareID":"s5","LinkID":"l5","ShareTargetType":3},
            {"VolumeID":"v2","ShareID":"s6","LinkID":"l6","ShareTargetType":5},
            {"VolumeID":"v3","ShareID":"s7","LinkID":"l7"}]}"#;
        assert!(
            serde_json::from_str::<proton_drive_sdk::api::share::SharedWithMeResponse>(body)
                .is_err(),
            "the SDK's types refuse the page (else this test needs a new case)"
        );
        let read = |body: &str| {
            parse_api_page(
                "shared-with-me",
                reqwest::StatusCode::OK,
                &Default::default(),
                body,
                |page: &SharedPageDto| page.code,
            )
        };
        let entries: Vec<_> = read(body)
            .expect("the page is read")
            .links
            .into_iter()
            .filter_map(SharedLinkDto::entry)
            .collect();
        assert_eq!(
            entries,
            [
                ("s1".into(), "v1".into(), "l1".into(), NodeKind::Folder),
                ("s3".into(), "v2".into(), "l3".into(), NodeKind::File),
            ]
        );
        assert!(read(r#"{"Code":1000}"#).is_err(), "no links, no listing");
        assert!(
            read(r#"{"Code":2011,"Error":"Not allowed"}"#)
                .map_err(map_error)
                .is_err_and(|error| error == BackendError::PermissionDenied)
        );
    }

    /// The listing is read page by page, by `More` and `AnchorID`, and an
    /// unknown type on the first page does not end it.
    #[tokio::test]
    async fn the_shared_listing_is_read_by_its_anchors() {
        let first = br#"{"Code":1000,"More":true,"AnchorID":"next-1","Links":[
            {"VolumeID":"v1","ShareID":"s1","LinkID":"l1","ShareTargetType":42},
            {"VolumeID":"v1","ShareID":"s2","LinkID":"l2","ShareTargetType":1}]}"#;
        let last = br#"{"Code":1000,"More":false,"AnchorID":"next-2","Links":[
            {"VolumeID":"v2","ShareID":"s3","LinkID":"l3","ShareTargetType":2}]}"#;
        let (url, requests) = serve(vec![answer("200 OK", first), answer("200 OK", last)]).await;
        let api = SharedApi {
            http: reqwest_middleware::ClientBuilder::new(reqwest::Client::new()).build(),
            base: reqwest::Url::parse(&url).expect("url"),
        };
        let session = offline::session("s");
        let links = shared_links(&api, &session.token_credential)
            .await
            .expect("listing");
        assert_eq!(
            links,
            [
                ("s2".into(), "v1".into(), "l2".into(), NodeKind::Folder),
                ("s3".into(), "v2".into(), "l3".into(), NodeKind::File),
            ]
        );
        assert_eq!(
            *requests.lock(),
            [
                "GET /v2/sharedwithme HTTP/1.1",
                "GET /v2/sharedwithme?AnchorID=next-1 HTTP/1.1"
            ]
        );
    }

    /// Reads block 1 of a one-block revision through the adapter, the fork's
    /// downloader and a local server for the storage and the API: `answers`
    /// gets the server's URL and the encrypted block and gives the answers
    /// in the order the requests come. Returns the plaintext or the error,
    /// and the request lines.
    /// A listed revision, as a block page carries it.
    fn listed_revision() -> RevisionDto {
        serde_json::from_value(serde_json::json!({
            "ID": "rev", "CreateTime": 0, "Size": 1, "State": 1,
            "ManifestSignature": null, "SignatureEmail": null, "XAttr": null,
            "Thumbnails": [],
        }))
        .expect("revision")
    }

    async fn read_one_block(
        answers: impl FnOnce(&str, &[u8]) -> Vec<Vec<u8>>,
    ) -> (Result<Vec<u8>>, Vec<String>) {
        use proton_drive_sdk::proton_sdk_rs2::crypto::proton_rpgp;
        use proton_rpgp::pgp::ser::Serialize;
        use sha2::Digest;
        let plain = b"the block's plaintext".to_vec();
        let key = proton_drive_sdk::crypto::CryptoGenerator::generate_session_key();
        let encrypted = proton_rpgp::Encryptor::default()
            .with_session_key(key.to_rpgp_sk().expect("key"))
            .encrypt(&plain)
            .expect("encrypt")
            .to_bytes()
            .expect("bytes");
        let (listener, url) = listen().await;
        let requests = serve_on(listener, answers(&url, &encrypted));
        let session = offline::session_at("s", &url);
        let backend = ProtonBackend::with_root(
            offline::drive(&session),
            session.token_credential.clone(),
            "user".into(),
            "volume",
            LinkId::new("root"),
        );
        let (file, revision) = (LinkId::new("file"), RevisionId::new("rev"));
        // Block 1 listed and its manifest verified already.
        backend.block_lists.seed(
            &file,
            &revision,
            vec![Listed {
                index: 1,
                hash: sha2::Sha256::digest(&encrypted).to_vec(),
                target: BlockTarget {
                    bare_url: format!("{url}blob/old"),
                    token: "token".into(),
                    content_key: key,
                },
            }],
            listed_revision(),
            true,
        );
        let mut buf = vec![0; plain.len()];
        let read = backend
            .fetch_block_into(&file, &revision, 1, &mut buf)
            .await
            .map(|len| buf[..len].to_vec());
        if let Ok(read) = &read {
            assert_eq!(*read, plain);
        }
        let requests = requests.lock().clone();
        (read, requests)
    }

    /// The headers and half of `body`, then the connection closes.
    fn cut(body: &[u8]) -> Vec<u8> {
        let mut answer = answer("200 OK", body);
        answer.truncate(answer.len() - body.len() / 2);
        answer
    }

    /// The revision's block listing of the fork's target refresh, with a
    /// fresh target for block 1.
    fn fresh_target(url: &str, encrypted: &[u8]) -> Vec<u8> {
        use sha2::Digest;
        let body = serde_json::json!({
            "Code": 1000,
            "Revision": {
                "ID": "rev", "CreateTime": 0, "Size": encrypted.len(), "State": 1,
                "ManifestSignature": null, "SignatureEmail": null, "XAttr": null,
                "Thumbnails": [],
                "Blocks": [{
                    "Index": 1,
                    "Hash": hex::encode(sha2::Sha256::digest(encrypted)),
                    "BareURL": format!("{url}blob/fresh"),
                    "Token": "fresh",
                }],
            },
        });
        answer("200 OK", body.to_string().as_bytes())
    }

    const OLD_BLOB: &str = "GET /blob/old HTTP/1.1";
    const FRESH_BLOB: &str = "GET /blob/fresh HTTP/1.1";
    const TARGET_REFRESH: &str = "GET /drive/v2/volumes/volume/files/file/revisions/rev?FromBlockIndex=1&PageSize=1&NoBlockUrls=0 HTTP/1.1";

    /// A block whose transfer is cut off is fetched once more and read
    /// (R6); the fork alone failed at once.
    #[tokio::test]
    async fn a_block_cut_off_on_the_way_is_fetched_once_more() {
        let (read, requests) =
            read_one_block(|_, encrypted| vec![cut(encrypted), answer("200 OK", encrypted)]).await;
        assert!(read.is_ok(), "{read:?}");
        assert_eq!(requests, [OLD_BLOB, OLD_BLOB]);
    }

    /// A second cut is a failed read (`Transient`, EIO), after two
    /// requests, not more.
    #[tokio::test]
    async fn a_block_cut_off_twice_fails() {
        let (read, requests) = read_one_block(|_, encrypted| {
            vec![cut(encrypted), cut(encrypted), answer("200 OK", encrypted)]
        })
        .await;
        assert!(matches!(read, Err(BackendError::Transient(_))), "{read:?}");
        assert_eq!(requests, [OLD_BLOB, OLD_BLOB]);
    }

    /// An expired target (404) is refreshed through the revision's block
    /// listing and the block read from the fresh one (the fork's refresh,
    /// P2), within one read of the adapter.
    #[tokio::test]
    async fn an_expired_block_target_is_refreshed_and_read() {
        let (read, requests) = read_one_block(|url, encrypted| {
            vec![
                answer("404 Not Found", br#"{"Code":2501,"Error":"Expired"}"#),
                fresh_target(url, encrypted),
                answer("200 OK", encrypted),
            ]
        })
        .await;
        assert!(read.is_ok(), "{read:?}");
        assert_eq!(requests, [OLD_BLOB, TARGET_REFRESH, FRESH_BLOB]);
    }

    /// A fresh target that is missing too is an integrity error (EIO, the
    /// block not cached), with no further request.
    #[tokio::test]
    async fn a_block_missing_after_its_refresh_fails() {
        let (read, requests) = read_one_block(|url, encrypted| {
            vec![
                answer("404 Not Found", b""),
                fresh_target(url, encrypted),
                answer("404 Not Found", b""),
                answer("200 OK", encrypted),
            ]
        })
        .await;
        assert!(matches!(read, Err(BackendError::Integrity(_))), "{read:?}");
        assert_eq!(requests, [OLD_BLOB, TARGET_REFRESH, FRESH_BLOB]);
    }

    /// Sessions made offline, for the clients' bookkeeping.
    mod offline {
        use super::*;
        use proton_drive_sdk::proton_sdk_rs2::cache::InMemoryCacheRepository;
        use proton_drive_sdk::proton_sdk_rs2::{
            AppVersionConfiguration, PasswordMode, SessionId, UserId,
        };

        /// A session whose API is the local server at `base` (its block
        /// listing for a refreshed target, say), never Proton's.
        pub fn session_at(id: &str, base: &str) -> ProtonAPISession {
            ProtonAPISession::resume_with_options(
                SessionId::new(id.to_owned()),
                "user",
                UserId::new("user-id".to_owned()),
                "access".to_owned(),
                "refresh".to_owned(),
                Vec::new(),
                false,
                PasswordMode::Single,
                AppVersionConfiguration::new("pdmount-test", 0, 0, 1),
                Arc::new(InMemoryCacheRepository::new()),
                proton_drive_sdk::proton_sdk_rs2::client::ProtonClientOptions {
                    base_url: Some(base.parse().expect("uri")),
                    ..Default::default()
                },
            )
        }

        /// A session with id `id` that never reaches a server.
        pub fn session(id: &str) -> ProtonAPISession {
            ProtonAPISession::resume(
                SessionId::new(id.to_owned()),
                "user",
                UserId::new("user-id".to_owned()),
                "access".to_owned(),
                "refresh".to_owned(),
                Vec::new(),
                false,
                PasswordMode::Single,
                AppVersionConfiguration::new("pdmount-test", 0, 0, 1),
                Arc::new(InMemoryCacheRepository::new()),
            )
        }

        /// The Photos clients of `session`, their Photos volume found.
        pub fn photos(session: &ProtonAPISession) -> PhotosClients {
            let photos = PhotosClients::new(session, None, |client| client).expect("clients");
            photos
                .ready
                .set(PhotosVolume {
                    volume: "photos-volume".into(),
                    root: LinkId::new("photos-root"),
                })
                .expect("not set yet");
            photos
        }

        pub fn drive(session: &ProtonAPISession) -> ProtonDriveClient {
            ProtonDriveClient::new(session, None).expect("drive client")
        }
    }

    /// A Photos call takes the drive client and credential of one session's
    /// Photos clients, never My Files' client and never another session's
    /// credential; a new login replaces both in one write (D119).
    #[tokio::test]
    async fn a_photos_call_uses_one_sessions_photos_clients() {
        let (a, b) = (offline::session("a"), offline::session("b"));
        let backend = ProtonBackend::with_root(
            offline::drive(&a),
            a.token_credential.clone(),
            "user".into(),
            "volume",
            LinkId::new("root"),
        );
        let photos_tree = backend.photos_tree("photos-volume", LinkId::new("photos-root"));
        let session_of = |call: Result<(ProtonDriveClient, TokenCredential)>| {
            call.map(|(_, credential)| credential.session_id().raw().clone())
        };
        // Without Photos clients a Photos call fails: My Files' client
        // cannot read the Photos volume.
        assert!(matches!(
            photos_tree.session_for_call().await,
            Err(BackendError::Unsupported(_))
        ));
        backend.set_photos(Some(offline::photos(&a)));
        assert_eq!(
            session_of(photos_tree.session_for_call().await),
            Ok("a".into())
        );
        // A new login: its drive client, credential and Photos clients at once.
        backend.replace_session(
            offline::drive(&b),
            b.token_credential.clone(),
            Some(offline::photos(&b)),
        );
        assert_eq!(
            session_of(photos_tree.session_for_call().await),
            Ok("b".into())
        );
        assert_eq!(session_of(backend.session_for_call().await), Ok("b".into()));
        // A login whose Photos clients could not be made: Photos calls fail
        // rather than use another session's.
        backend.replace_session(offline::drive(&a), a.token_credential.clone(), None);
        assert!(photos_tree.session_for_call().await.is_err());
        assert_eq!(session_of(backend.session_for_call().await), Ok("a".into()));
    }
}
