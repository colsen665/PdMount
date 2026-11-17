//! M5.9 probe of the further roots (docs/plans/m5.md, "M5.9"): invitations,
//! shares, devices, Photos and the event streams that would keep them
//! current. Read-only, on the real account (D028, D030): it never accepts,
//! rejects, leaves, uploads, renames or trashes anything. Calls are
//! sequential, and the probe stops at the first HTTP 429.
//!
//! ```sh
//! PROBE_PROFILE=default PROBE_SECRETS=force-file PROBE_PHASE=snapshot \
//!   cargo test -p pdmount probe_roots -- --ignored --nocapture
//! PROBE_PROFILE=default PROBE_SECRETS=force-file PROBE_PHASE=watch \
//!   PROBE_WATCH_SECS=600 cargo test -p pdmount probe_roots -- --ignored --nocapture
//! ```
//!
//! `snapshot` prints one pass over every root and saves the volume and event
//! ids to `target/probe-roots/state.json`. `watch` polls from those ids
//! (`PROBE_FRESH=1`: from the latest ones) for `PROBE_WATCH_SECS` (default
//! 600): own volumes and the core events every 30 s, other users' volumes and
//! the invitation, shared-with-me and device listings every 60 s, as the
//! official JS client. Each run's output also goes to
//! `target/probe-roots/<phase>-<unix time>.log`. The results are interpreted
//! in docs/plans/m5.md, "M5.9".

use crate::credentials::{self, SecretsMode};
use crate::session::{self, Resumed, TokenSaver};
use anyhow::{Context, bail};
use futures::StreamExt;
use pdmount_core::ProfilePaths;
use proton_drive_sdk::api::devices::RawDeviceInfo;
use proton_drive_sdk::api::events::VolumeEventsResponse;
use proton_drive_sdk::api::file::photos::TimelinePhotoListResponse;
use proton_drive_sdk::api::share::ShareTargetType;
use proton_drive_sdk::client::ProtonDriveClient;
use proton_drive_sdk::error::ApiError;
use proton_drive_sdk::links::LinkId;
use proton_drive_sdk::node::{DegradedNode, Node, NodeUid};
use proton_drive_sdk::photo::ProtonPhotosClient;
use proton_drive_sdk::proton_sdk_rs2::session::ProtonAPISession;
use proton_drive_sdk::sharing::SharingOperations;
use proton_drive_sdk::utils::PotentialObject;
use proton_drive_sdk::volume::VolumeId;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

const CLIENT_UID: &str = "pdmount-rootsprobe";
/// Polling intervals of the official JS client (`internal/events/index.ts`).
const OWN_VOLUME_POLL: Duration = Duration::from_secs(30);
const OTHER_VOLUME_POLL: Duration = Duration::from_secs(60);
const CORE_POLL: Duration = Duration::from_secs(30);
/// Invitations, shared-with-me, devices and the share roots' roles.
const LISTING_POLL: Duration = Duration::from_secs(60);
/// Invitation listings of every kind, Photos included.
const TARGET_TYPES: &[ShareTargetType] = &[
    ShareTargetType::Folder,
    ShareTargetType::File,
    ShareTargetType::ProtonVendor,
    ShareTargetType::Album,
    ShareTargetType::Photo,
];
/// At most this many photos are fetched one by one, with their thumbnails.
const MAX_PHOTOS: usize = 10;
/// At most this many children are listed below a share or device root.
const MAX_CHILDREN: usize = 50;

// ---------------------------------------------------------------------------
// Call accounting and the rate-limit stop

/// SDK calls made by the probe (one call may send several requests).
static CALLS: AtomicU64 = AtomicU64::new(0);
/// HTTP/2 requests sent, counted from h2's `send_headers` trace events.
static REQUESTS: AtomicU64 = AtomicU64::new(0);
/// Set when the SDK's retry layer saw a 429 (it retries those silently).
static SAW_429: AtomicBool = AtomicBool::new(false);

/// Counts requests and notices 429 responses the SDK retried.
struct CountLayer;

#[derive(Default)]
struct Fields {
    message: String,
    outcome: String,
}

impl tracing::field::Visit for Fields {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        match field.name() {
            "message" => self.message = format!("{value:?}"),
            "outcome" => self.outcome = format!("{value:?}"),
            _ => {}
        }
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CountLayer {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let target = event.metadata().target();
        let mut fields = Fields::default();
        event.record(&mut fields);
        if target.starts_with("h2") {
            if fields.message.starts_with("send_headers;") {
                REQUESTS.fetch_add(1, Ordering::Relaxed);
            }
        } else if fields.outcome.contains("429") {
            SAW_429.store(true, Ordering::Relaxed);
        }
    }
}

/// The error that ends the probe.
#[derive(Debug)]
struct RateLimited;

impl std::fmt::Display for RateLimited {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HTTP 429: rate limited, stopping (AGENTS.md)")
    }
}

impl std::error::Error for RateLimited {}

fn is_stop(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| cause.is::<RateLimited>())
}

fn is_not_found(error: &anyhow::Error) -> bool {
    ApiError::find(error).is_some_and(ApiError::is_not_found)
}

fn describe_error(error: &anyhow::Error) -> String {
    match ApiError::find(error) {
        Some(api) => format!(
            "http={:?} code={:?} message={:?}",
            api.http_status, api.code, api.message
        ),
        None => format!("{error:#}"),
    }
}

// ---------------------------------------------------------------------------
// Pure helpers (unit-tested)

/// Format and pixel size of an image, read from its header.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ImageInfo {
    format: String,
    size: Option<(u32, u32)>,
}

fn be16(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from(u16::from_be_bytes(
        b.get(at..at + 2)?.try_into().ok()?,
    )))
}

fn le16(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from(u16::from_le_bytes(
        b.get(at..at + 2)?.try_into().ok()?,
    )))
}

fn le24(b: &[u8], at: usize) -> Option<u32> {
    let s = b.get(at..at + 3)?;
    Some(u32::from(s[0]) | u32::from(s[1]) << 8 | u32::from(s[2]) << 16)
}

fn image_info(b: &[u8]) -> ImageInfo {
    let info = |format: &str, size| ImageInfo {
        format: format.into(),
        size,
    };
    if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        let size = (b.get(12..16) == Some(b"IHDR")).then(|| {
            let w = u32::from_be_bytes(b.get(16..20)?.try_into().ok()?);
            let h = u32::from_be_bytes(b.get(20..24)?.try_into().ok()?);
            Some((w, h))
        });
        return info("png", size.flatten());
    }
    if b.starts_with(&[0xFF, 0xD8]) {
        return info("jpeg", jpeg_size(b));
    }
    if b.starts_with(b"RIFF") && b.get(8..12) == Some(b"WEBP") {
        let size = match b.get(12..16) {
            Some(b"VP8 ") if b.get(23..26) == Some(&[0x9d, 0x01, 0x2a]) => le16(b, 26)
                .zip(le16(b, 28))
                .map(|(w, h)| (w & 0x3fff, h & 0x3fff)),
            Some(b"VP8L") if b.get(20) == Some(&0x2f) => b.get(21..25).map(|s| {
                let bits = u32::from_le_bytes([s[0], s[1], s[2], s[3]]);
                (1 + (bits & 0x3fff), 1 + ((bits >> 14) & 0x3fff))
            }),
            Some(b"VP8X") => le24(b, 24).zip(le24(b, 27)).map(|(w, h)| (w + 1, h + 1)),
            _ => None,
        };
        return info("webp", size);
    }
    if b.get(4..8) == Some(b"ftyp") {
        let brand = b
            .get(8..12)
            .map(String::from_utf8_lossy)
            .unwrap_or_default();
        return info(&format!("isobmff/{brand}"), None);
    }
    if b.starts_with(b"GIF8") {
        return info("gif", le16(b, 6).zip(le16(b, 8)));
    }
    let head: String = b.iter().take(8).map(|x| format!("{x:02x}")).collect();
    info(&format!("unknown({head})"), None)
}

/// Width and height from a JPEG's first start-of-frame segment.
fn jpeg_size(b: &[u8]) -> Option<(u32, u32)> {
    let mut i = 2;
    while i + 4 <= b.len() {
        if b[i] != 0xFF {
            return None;
        }
        let marker = b[i + 1];
        if marker == 0xFF {
            i += 1;
            continue;
        }
        if (0xD0..=0xD9).contains(&marker) || marker == 0x01 {
            i += 2;
            continue;
        }
        let length = be16(b, i + 2)? as usize;
        if (0xC0..=0xCF).contains(&marker) && ![0xC4, 0xC8, 0xCC].contains(&marker) {
            let h = be16(b, i + 5)?;
            let w = be16(b, i + 7)?;
            return Some((w, h));
        }
        i += 2 + length;
    }
    None
}

/// Lines describing how listing `new` differs from `old`.
fn diff(
    label: &str,
    old: &BTreeMap<String, String>,
    new: &BTreeMap<String, String>,
) -> Vec<String> {
    let mut lines = Vec::new();
    for (key, value) in new {
        match old.get(key) {
            None => lines.push(format!("{label} ADDED {key}: {value}")),
            Some(before) if before != value => {
                lines.push(format!("{label} CHANGED {key}: {before}  ->  {value}"))
            }
            Some(_) => {}
        }
    }
    for (key, value) in old {
        if !new.contains_key(key) {
            lines.push(format!("{label} REMOVED {key}: {value}"));
        }
    }
    lines
}

/// Where a node sits.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Place {
    MyFiles,
    Device(String),
    Photos,
    Share(String),
}

impl std::fmt::Display for Place {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Place::MyFiles => write!(f, "my-files"),
            Place::Device(name) => write!(f, "device:{name}"),
            Place::Photos => write!(f, "photos"),
            Place::Share(name) => write!(f, "share:{name}"),
        }
    }
}

/// Known roots and parent links, keyed by node uid (`volume~link`).
#[derive(Debug, Default)]
struct Tree {
    roots: HashMap<String, Place>,
    parents: HashMap<String, Option<String>>,
}

/// Why a node could not be placed.
#[derive(Debug, PartialEq, Eq)]
enum Unplaced {
    /// The parent of this uid is not known yet.
    Missing(String),
    /// The chain ends at this uid, which is no known root.
    Top(String),
}

impl Tree {
    fn place_of(&self, uid: &str) -> Result<Place, Unplaced> {
        let mut current = uid.to_owned();
        for _ in 0..64 {
            if let Some(place) = self.roots.get(&current) {
                return Ok(place.clone());
            }
            match self.parents.get(&current) {
                Some(Some(parent)) => current = parent.clone(),
                Some(None) => return Err(Unplaced::Top(current)),
                None => return Err(Unplaced::Missing(current)),
            }
        }
        Err(Unplaced::Top(current))
    }
}

/// A volume event, reduced to what the probe prints.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Change {
    event_id: String,
    kind: &'static str,
    uid: String,
    parent: Option<String>,
    shared: bool,
    trashed: bool,
}

fn changes(volume: &str, page: &VolumeEventsResponse) -> Vec<Change> {
    page.events
        .iter()
        .map(|event| Change {
            event_id: event.event_id.clone(),
            kind: match event.event_type {
                0 => "Delete",
                1 => "Create",
                2 => "UpdateMetadata",
                3 => "UpdateContent",
                _ => "Unknown",
            },
            uid: format!("{volume}~{}", event.link.link_id.raw()),
            parent: event
                .link
                .parent_link_id
                .as_ref()
                .map(|parent| format!("{volume}~{}", parent.raw())),
            shared: event.link.is_shared,
            trashed: event.link.is_trashed,
        })
        .collect()
}

fn change_line(volume_kind: &str, change: &Change, place: &str, node: Option<&str>) -> String {
    format!(
        "EVENT vol={volume_kind} id={} type={} node={} parent={} shared={} trashed={} place={place} | {}",
        change.event_id,
        change.kind,
        change.uid,
        change.parent.as_deref().unwrap_or("-"),
        change.shared,
        change.trashed,
        node.unwrap_or("(not fetched)")
    )
}

/// A short, stable description of a decrypted node; it changes when the
/// node's name, role or membership does.
fn node_summary(node: &PotentialObject<Node, DegradedNode>) -> String {
    match node {
        PotentialObject::Node(node) => {
            let base = node.base();
            let mut text = format!(
                "{:?} {:?} direct_role={:?} membership={} shared={} parent={}",
                node.ty(),
                base.name,
                base.direct_role,
                base.membership
                    .as_ref()
                    .map_or("-".into(), |m| format!("{:?}", m.role)),
                base.is_shared,
                base.parent_uid
                    .as_ref()
                    .map_or("-".into(), ToString::to_string),
            );
            if let Node::File(file) | Node::Photo(file) = node {
                text.push_str(&format!(
                    " mime={} size={:?} trashed={}",
                    file.base.media_type,
                    file.active_revision.claimed_size,
                    base.trash_time.is_some()
                ));
            }
            text
        }
        PotentialObject::Degraded(node) => format!(
            "DEGRADED uid={} parent={}",
            node.uid(),
            node.parent_uid().map_or("-".into(), ToString::to_string)
        ),
    }
}

fn node_parent(node: &PotentialObject<Node, DegradedNode>) -> Option<String> {
    match node {
        PotentialObject::Node(node) => node.base().parent_uid.as_ref().map(ToString::to_string),
        PotentialObject::Degraded(node) => node.parent_uid().map(ToString::to_string),
    }
}

fn node_uid(node: &PotentialObject<Node, DegradedNode>) -> String {
    match node {
        PotentialObject::Node(node) => node.uid().to_string(),
        PotentialObject::Degraded(node) => node.uid().to_string(),
    }
}

fn node_name(node: &PotentialObject<Node, DegradedNode>) -> String {
    match node {
        PotentialObject::Node(node) => node.base().name.clone(),
        PotentialObject::Degraded(node) => format!("<degraded {}>", node.uid()),
    }
}

/// `volume~link` into a `NodeUid`.
fn parse_uid(uid: &str) -> Option<NodeUid> {
    let (volume, link) = uid.split_once('~')?;
    Some(NodeUid::from_parts(volume, link))
}

/// The field names of a JSON object, with each value's type.
fn field_types(value: &serde_json::Value) -> String {
    let Some(object) = value.as_object() else {
        return format!("(not an object: {value})");
    };
    object
        .iter()
        .map(|(key, value)| {
            let kind = match value {
                serde_json::Value::Null => "null",
                serde_json::Value::Bool(_) => "bool",
                serde_json::Value::Number(_) => "number",
                serde_json::Value::String(_) => "string",
                serde_json::Value::Array(_) => "array",
                serde_json::Value::Object(_) => "object",
            };
            format!("{key}:{kind}")
        })
        .collect::<Vec<_>>()
        .join(", ")
}

// ---------------------------------------------------------------------------
// The saved state

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
struct State {
    saved_at: i64,
    main_volume: String,
    my_files_root: String,
    main_event: String,
    core_event: Option<String>,
    photos_volume: Option<String>,
    photos_root: Option<String>,
    photos_event: Option<String>,
}

fn out_dir() -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../target")))
        .join("probe-roots")
}

fn now_unix() -> i64 {
    chrono::Utc::now().timestamp()
}

// ---------------------------------------------------------------------------
// The probe

struct Probe {
    session: ProtonAPISession,
    drive: ProtonDriveClient,
    photos: ProtonPhotosClient,
    start: Instant,
    log: parking_lot::Mutex<Option<std::fs::File>>,
}

impl Probe {
    fn say(&self, line: impl AsRef<str>) {
        let line = format!(
            "[+{:6.1}s] {}",
            self.start.elapsed().as_secs_f64(),
            line.as_ref()
        );
        println!("{line}");
        if let Some(file) = self.log.lock().as_mut() {
            let _ = writeln!(file, "{line}");
        }
    }

    /// Runs one SDK call; a 429 anywhere in it ends the probe.
    async fn call<T>(
        &self,
        label: &str,
        call: impl std::future::Future<Output = anyhow::Result<T>>,
    ) -> anyhow::Result<T> {
        CALLS.fetch_add(1, Ordering::Relaxed);
        let result = call.await;
        let limited = SAW_429.load(Ordering::Relaxed)
            || result
                .as_ref()
                .err()
                .and_then(ApiError::find)
                .is_some_and(|api| api.is_rate_limited() || api.code == Some(2028));
        if limited {
            return Err(anyhow::Error::new(RateLimited).context(label.to_owned()));
        }
        result.with_context(|| label.to_owned())
    }

    /// Like `call`, but an error other than a 429 is printed and gives `None`.
    async fn soft<T>(
        &self,
        label: &str,
        call: impl std::future::Future<Output = anyhow::Result<T>>,
    ) -> anyhow::Result<Option<T>> {
        match self.call(label, call).await {
            Ok(value) => Ok(Some(value)),
            Err(error) if is_stop(&error) => Err(error),
            Err(error) => {
                self.say(format!("  ERR {label}: {}", describe_error(&error)));
                Ok(None)
            }
        }
    }

    /// The client whose link endpoints serve `volume`.
    fn client_for(&self, volume: &str, photos_volume: Option<&str>) -> &ProtonDriveClient {
        if photos_volume == Some(volume) {
            self.photos.drive()
        } else {
            &self.drive
        }
    }

    /// One GET on the Drive API, for the fields the SDK's types drop. Same
    /// endpoint, headers and session as the SDK; a 429 ends the probe.
    async fn raw_get(&self, path: &str) -> anyhow::Result<serde_json::Value> {
        let http = self.session.get_http_client(
            Some("drive/".into()),
            Some(Duration::from_secs(30)),
            None,
        )?;
        let base = reqwest::Url::parse(&self.session.client_config.base_url.to_string())?
            .join("drive/")?;
        let credential = &self.session.token_credential;
        let (access, _) = credential.get_tokens().await?;
        let response = http
            .get(base.join(path)?)
            .header("Authorization", format!("Bearer {access}"))
            .header("x-pm-uid", credential.session_id().raw().as_str())
            .send()
            .await?;
        let status = response.status();
        let body = response.text().await?;
        if status.as_u16() == 429 {
            return Err(anyhow::Error::new(RateLimited));
        }
        if !status.is_success() {
            bail!(
                "HTTP {status}: {}",
                body.chars().take(300).collect::<String>()
            );
        }
        Ok(serde_json::from_str(&body)?)
    }

    fn report_counts(&self) {
        self.say(format!(
            "COUNTS: {} SDK calls, {} HTTP/2 requests (h2 send_headers; 0 means HTTP/1.1 or no trace)",
            CALLS.load(Ordering::Relaxed),
            REQUESTS.load(Ordering::Relaxed)
        ));
    }
}

// ---------------------------------------------------------------------------
// Listings shared by both phases

/// Pending invitations, keyed by invitation id. Details and the decrypted
/// name are fetched once per invitation (`known` caches them).
async fn invitations(
    p: &Probe,
    known: &mut BTreeMap<String, String>,
    verbose: bool,
) -> anyhow::Result<Option<BTreeMap<String, String>>> {
    let shares = p.drive.api().shares();
    let mut listing = BTreeMap::new();
    let mut anchor: Option<String> = None;
    loop {
        let Some(page) = p
            .soft(
                "list_invitations",
                shares.list_invitations(TARGET_TYPES, anchor.as_deref()),
            )
            .await?
        else {
            return Ok(None);
        };
        for pending in &page.invitations {
            let id = pending.invitation_id.clone();
            if let Some(summary) = known.get(&id) {
                listing.insert(id, summary.clone());
                continue;
            }
            let mut summary = format!(
                "share={} list_target_type={:?}",
                pending.share_id.raw(),
                pending.share_target_type
            );
            if let Some(details) = p.soft("get_invitation", shares.get_invitation(&id)).await? {
                let i = &details.invitation;
                summary.push_str(&format!(
                    " inviter={} invitee={} permissions={} role={:?} created={:?} share_creator={} \
                     volume={} share_target_type={:?} link={} link_type={:?} mime={:?} encrypted_name={}",
                    i.inviter_email,
                    i.invitee_email,
                    i.permissions.bits(),
                    i.permissions.to_role(),
                    i.create_time,
                    details.share.creator_email,
                    details.share.volume_id.raw(),
                    details.share.share_target_type,
                    details.link.link_id.raw(),
                    details.link.r#type,
                    details.link.mime_type,
                    details.link.name.is_some(),
                ));
            }
            let uid = proton_drive_sdk::sharing::invitation_uid(&pending.share_id, &id);
            if let Some(decrypted) = p
                .soft(
                    "decrypt invitation name",
                    SharingOperations::get_invitation(&p.drive, &uid),
                )
                .await?
            {
                summary.push_str(&format!(
                    " name={:?} node={} sdk_role={:?}",
                    decrypted.node_name, decrypted.node_uid, decrypted.invitation.role
                ));
            }
            if verbose {
                p.say(format!("  invitation {id}: {summary}"));
            }
            known.insert(id.clone(), summary.clone());
            listing.insert(id, summary);
        }
        if !page.more || page.anchor_id.is_none() {
            break;
        }
        anchor = page.anchor_id.clone();
    }
    Ok(Some(listing))
}

/// One shared-with-me entry.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SharedEntry {
    volume: String,
    share: String,
    target: String,
}

/// Shared-with-me, keyed by root node uid.
async fn shared_with_me(p: &Probe) -> anyhow::Result<Option<BTreeMap<String, SharedEntry>>> {
    let shares = p.drive.api().shares();
    let mut listing = BTreeMap::new();
    let mut anchor = None;
    loop {
        let Some(page) = p
            .soft("sharedwithme", shares.get_shared_with_me(anchor.clone()))
            .await?
        else {
            return Ok(None);
        };
        for link in &page.links {
            listing.insert(
                format!("{}~{}", link.volume_id.raw(), link.link_id.raw()),
                SharedEntry {
                    volume: link.volume_id.raw().to_owned(),
                    share: link.share_id.raw().clone(),
                    target: format!("{:?}", link.share_target_type),
                },
            );
        }
        if !page.more || page.anchor_id.is_none() {
            break;
        }
        anchor = page.anchor_id.clone();
    }
    Ok(Some(listing))
}

fn shared_summaries(listing: &BTreeMap<String, SharedEntry>) -> BTreeMap<String, String> {
    listing
        .iter()
        .map(|(uid, e)| {
            (
                uid.clone(),
                format!("share={} target={}", e.share, e.target),
            )
        })
        .collect()
}

/// The first level below a folder: name, kind and role of each child.
async fn children(p: &Probe, client: &ProtonDriveClient, folder: NodeUid) -> anyhow::Result<()> {
    let listed = p
        .soft("enumerate_folder_children", async {
            let stream = client.enumerate_folder_children(folder.clone()).await?;
            Ok(stream.take(MAX_CHILDREN).collect::<Vec<_>>().await)
        })
        .await?;
    let Some(listed) = listed else {
        return Ok(());
    };
    p.say(format!(
        "    children of {folder}: {} (at most {MAX_CHILDREN})",
        listed.len()
    ));
    for child in listed {
        match child {
            Ok(child) => p.say(format!("      - {}", node_summary(&child))),
            Err(error) => p.say(format!("      - ERR {}", describe_error(&error))),
        }
    }
    Ok(())
}

/// Prints a share and its root, and returns the root's name.
async fn describe_share(p: &Probe, uid: &str, entry: &SharedEntry) -> anyhow::Result<String> {
    p.say(format!(
        "  SHARE root={uid} share={} target={}",
        entry.share, entry.target
    ));
    let share_id = proton_drive_sdk::share::ShareId::new(entry.share.clone());
    if let Some(share) = p
        .soft("get_share", p.drive.api().shares().get_share(share_id))
        .await?
    {
        p.say(format!(
            "    share: type={:?} state={:?} creator={} locked={} created={:?} root_link={} \
             root_link_type={:?} editors_can_share={}",
            share.r#type,
            share.state,
            share.creator_email_address,
            share.is_locked,
            share.creation_time,
            share.root_link_id.raw(),
            share.root_link_type,
            share.editors_can_share
        ));
        for m in &share.memberships {
            p.say(format!(
                "    membership: member={} inviter={} permissions={} role={:?} state={:?} unlockable={:?} created={}",
                m.id.raw(),
                m.inviter_email_address,
                m.permissions.bits(),
                m.permissions.to_role(),
                m.state,
                m.can_be_unlocked,
                m.creation_time
            ));
        }
    }
    let Some(root_uid) = parse_uid(uid) else {
        return Ok(uid.to_owned());
    };
    let Some(root) = p
        .soft(
            "get_node share root",
            p.drive.get_node_uncached(root_uid.clone()),
        )
        .await?
    else {
        return Ok(uid.to_owned());
    };
    p.say(format!("    root: {}", node_summary(&root)));
    if let PotentialObject::Node(Node::Folder(_)) = &root {
        children(p, &p.drive, root_uid).await?;
    }
    Ok(node_name(&root))
}

/// Devices, keyed by device id.
async fn devices(p: &Probe) -> anyhow::Result<Option<BTreeMap<String, RawDeviceInfo>>> {
    let Some(response) = p
        .soft("get_devices", p.drive.api().devices().get_devices())
        .await?
    else {
        return Ok(None);
    };
    let mut listing = BTreeMap::new();
    for entry in response.devices {
        match RawDeviceInfo::try_from(entry) {
            Ok(device) => {
                listing.insert(device.device_id.clone(), device);
            }
            Err(error) => p.say(format!("  device entry unreadable: {error:#}")),
        }
    }
    Ok(Some(listing))
}

fn device_summaries(listing: &BTreeMap<String, RawDeviceInfo>) -> BTreeMap<String, String> {
    listing
        .iter()
        .map(|(id, d)| {
            (
                id.clone(),
                format!(
                    "volume={} share={} root_link={} type={:?} created={} last_sync={:?} deprecated_name={}",
                    d.volume_id.raw(),
                    d.share_id.raw(),
                    d.root_link_id.raw(),
                    d.device_type,
                    d.create_time,
                    d.last_sync_time,
                    d.has_deprecated_name
                ),
            )
        })
        .collect()
}

/// Prints a device's root and first level; returns its name.
async fn describe_device(
    p: &Probe,
    device: &RawDeviceInfo,
    main_volume: &str,
) -> anyhow::Result<String> {
    let root_uid = NodeUid::new(device.volume_id.clone(), device.root_link_id.clone());
    p.say(format!(
        "  DEVICE {} root={root_uid} volume={} same_volume_as_my_files={}",
        device.device_id,
        device.volume_id.raw(),
        device.volume_id.raw() == main_volume
    ));
    // The device's share key must be known before its root can be decrypted.
    let _ = p
        .soft(
            "get_share device",
            proton_drive_sdk::share_ops::ShareOperations::get_share(
                &p.drive,
                device.share_id.clone(),
            ),
        )
        .await?;
    let Some(root) = p
        .soft(
            "get_node device root",
            p.drive.get_node_uncached(root_uid.clone()),
        )
        .await?
    else {
        return Ok(device.device_id.clone());
    };
    p.say(format!("    root: {}", node_summary(&root)));
    children(p, &p.drive, root_uid).await?;
    Ok(node_name(&root))
}

// ---------------------------------------------------------------------------
// Snapshot

async fn snapshot(p: &Probe) -> anyhow::Result<State> {
    p.say("== Own volume (My Files)");
    let my_files = p
        .call("get_my_files_folder", p.drive.get_my_files_folder())
        .await?;
    let main_volume = my_files.base.uid.volume_id.clone();
    let main_event = p
        .call(
            "volume latest event (main)",
            p.drive.get_volume_latest_event_id(main_volume.clone()),
        )
        .await?;
    p.say(format!(
        "  my files root={} volume={} latest_event={main_event}",
        my_files.base.uid,
        main_volume.raw()
    ));
    let core_event = p
        .soft("core latest event", p.drive.get_core_latest_event_id())
        .await?;
    p.say(format!("  core latest_event={core_event:?}"));

    p.say("== Invitations (pending; none is accepted or rejected)");
    let listed = invitations(p, &mut BTreeMap::new(), true).await?;
    p.say(format!(
        "  {} pending invitation(s)",
        listed.as_ref().map_or(0, BTreeMap::len)
    ));

    p.say("== Shared with me");
    if let Some(shared) = shared_with_me(p).await? {
        p.say(format!("  {} entr(y/ies)", shared.len()));
        for (uid, entry) in &shared {
            describe_share(p, uid, entry).await?;
        }
    }

    p.say("== Devices (Computers)");
    if let Some(listing) = devices(p).await? {
        p.say(format!("  {} device(s)", listing.len()));
        for (id, summary) in device_summaries(&listing) {
            p.say(format!("  device {id}: {summary}"));
        }
        for device in listing.values() {
            describe_device(p, device, main_volume.raw()).await?;
        }
    }

    p.say("== Photos");
    let mut state = State {
        saved_at: now_unix(),
        main_volume: main_volume.raw().to_owned(),
        my_files_root: my_files.base.uid.to_string(),
        main_event,
        core_event,
        ..State::default()
    };
    if let Some(root) = p
        .soft("photos root", p.photos.get_photos_root_folder())
        .await?
    {
        let volume = root.base.uid.volume_id.clone();
        let event = p
            .soft(
                "volume latest event (photos)",
                p.drive.get_volume_latest_event_id(volume.clone()),
            )
            .await?;
        p.say(format!(
            "  photos root={} name={:?} volume={} separate_volume={} latest_event={event:?}",
            root.base.uid,
            root.base.name,
            volume.raw(),
            volume != main_volume
        ));
        state.photos_volume = Some(volume.raw().to_owned());
        state.photos_root = Some(root.base.uid.to_string());
        state.photos_event = event;
        photos(p, &volume).await?;
    }
    Ok(state)
}

async fn photos(p: &Probe, volume: &VolumeId) -> anyhow::Result<()> {
    p.say("  -- timeline, first page (raw response, then typed)");
    let Some(raw) = p
        .soft(
            "timeline page 1",
            p.raw_get(&format!("volumes/{}/photos", volume.raw())),
        )
        .await?
    else {
        return Ok(());
    };
    p.say(format!("  response fields: {}", field_types(&raw)));
    if let Some(first) = raw.get("Photos").and_then(|photos| photos.get(0)) {
        p.say(format!("  item fields: {}", field_types(first)));
        if let Some(related) = first.get("RelatedPhotos").and_then(|r| r.get(0)) {
            p.say(format!("  related item fields: {}", field_types(related)));
        }
    }
    for item in raw
        .get("Photos")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
    {
        p.say(format!("  raw item: {item}"));
    }
    let page: TimelinePhotoListResponse = serde_json::from_value(raw)?;
    let times: Vec<_> = page.photos.iter().map(|photo| photo.capture_time).collect();
    p.say(format!(
        "  page: {} item(s); API default PageSize 500 (driveTypes ListPhotosParameters); \
         capture times {times:?}; descending={}",
        page.photos.len(),
        times.windows(2).all(|pair| pair[0] >= pair[1])
    ));

    p.say("  -- albums");
    if let Some(albums) = p.soft("iterate_albums", p.photos.iterate_albums()).await? {
        p.say(format!("  {} album(s)", albums.len()));
        for album in &albums {
            let name = p
                .soft("get_node album", p.photos.get_node(album.uid.clone()))
                .await?
                .map(|node| node_summary(&node));
            p.say(format!(
                "  album {} photos={} last_activity={} cover={:?} node={name:?}",
                album.uid, album.photo_count, album.last_activity_time, album.cover_uid
            ));
            if album.photo_count <= 50
                && let Some(items) = p
                    .soft("iterate_album", p.photos.iterate_album(album.uid.clone()))
                    .await?
            {
                for item in items {
                    p.say(format!(
                        "    member {} captured={}",
                        item.uid, item.capture_time
                    ));
                }
            }
        }
    }

    p.say("  -- photos one by one");
    let mut names: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for photo in page.photos.iter().take(MAX_PHOTOS) {
        let uid = NodeUid::new(volume.clone(), photo.id.clone());
        p.say(format!(
            "  PHOTO {uid} captured={} name_hash={} content_hash={:?} tags={:?} related={}",
            photo.capture_time,
            photo.name_hash,
            photo.content_hash,
            photo.tags,
            photo.related_photos.len()
        ));
        for related in &photo.related_photos {
            p.say(format!("    related: {related:?}"));
        }
        // The normal drive client uses `v2/volumes/{volume}/links`; the
        // Photos client `photos/volumes/{volume}/links`.
        let normal = p
            .soft(
                "get_node via drive client",
                p.drive.get_node_uncached(uid.clone()),
            )
            .await?;
        p.say(format!(
            "    via drive client get_node: {}",
            normal.as_ref().map_or("failed".into(), node_summary)
        ));
        let Some(node) = p
            .soft("get_node via photos client", p.photos.get_node(uid.clone()))
            .await?
        else {
            continue;
        };
        p.say(format!(
            "    via photos client get_node: {}",
            node_summary(&node)
        ));
        names
            .entry(node_name(&node))
            .or_default()
            .push(node_parent(&node).unwrap_or_default());
        let PotentialObject::Node(Node::Photo(file) | Node::File(file)) = &node else {
            continue;
        };
        let revision = &file.active_revision;
        p.say(format!(
            "    revision={} created={} claimed_modification={:?} claimed_size={:?} additional={}",
            revision.uid.revision_id.raw(),
            revision.creation_time,
            revision.claimed_modification_time,
            revision.claimed_size,
            revision
                .additional_claimed_metadata
                .iter()
                .flatten()
                .map(
                    |property| if property.key.eq_ignore_ascii_case("location") {
                        format!("{}=<omitted>", property.key)
                    } else {
                        format!(
                            "{}={}",
                            property.key,
                            property.value.chars().take(80).collect::<String>()
                        )
                    }
                )
                .collect::<Vec<_>>()
                .join("; ")
        ));
        for header in &revision.thumbnails {
            let bytes = p
                .soft(
                    "fetch_thumbnail",
                    p.photos
                        .drive()
                        .fetch_thumbnail(uid.clone(), header.id.clone()),
                )
                .await?;
            match bytes {
                Some(bytes) => {
                    let info = image_info(&bytes);
                    p.say(format!(
                        "    thumbnail type={:?} bytes={} format={} size={:?}",
                        header.r#type,
                        bytes.len(),
                        info.format,
                        info.size
                    ));
                }
                None => p.say(format!(
                    "    thumbnail type={:?}: not fetched",
                    header.r#type
                )),
            }
        }
    }
    for (name, parents) in &names {
        p.say(format!(
            "  name {name:?}: {} photo(s), parents {parents:?}",
            parents.len()
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Watch

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VolumeKind {
    Own,
    Photos,
    Other,
}

struct Poller {
    volume: String,
    kind: VolumeKind,
    cursor: String,
    every: Duration,
    next: Instant,
    alive: bool,
}

struct Watch {
    state: State,
    tree: Tree,
    pollers: Vec<Poller>,
    core: Option<String>,
    core_next: Instant,
    listing_next: Instant,
    invitation_cache: BTreeMap<String, String>,
    invitations: BTreeMap<String, String>,
    shared: BTreeMap<String, SharedEntry>,
    devices: BTreeMap<String, String>,
    /// Share and device roots, with a summary that holds name and roles.
    roots: BTreeMap<String, String>,
}

impl Watch {
    fn photos_volume(&self) -> Option<&str> {
        self.state.photos_volume.as_deref()
    }

    fn add_poller(&mut self, volume: String, kind: VolumeKind, cursor: String) {
        if self.pollers.iter().any(|poller| poller.volume == volume) {
            return;
        }
        let every = if kind == VolumeKind::Other {
            OTHER_VOLUME_POLL
        } else {
            OWN_VOLUME_POLL
        };
        self.pollers.push(Poller {
            volume,
            kind,
            cursor,
            every,
            next: Instant::now() + every,
            alive: true,
        });
    }
}

async fn watch(p: &Probe, seconds: u64) -> anyhow::Result<()> {
    let saved = std::fs::read_to_string(out_dir().join("state.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<State>(&text).ok());
    let fresh = std::env::var("PROBE_FRESH").is_ok_and(|v| v == "1");
    let mut state = match saved {
        Some(state) if !fresh => {
            p.say(format!(
                "starting from the snapshot's event ids (saved {} s ago)",
                now_unix() - state.saved_at
            ));
            state
        }
        _ => {
            p.say("starting from the latest event ids");
            let my_files = p
                .call("get_my_files_folder", p.drive.get_my_files_folder())
                .await?;
            let volume = my_files.base.uid.volume_id.clone();
            let mut state = State {
                saved_at: now_unix(),
                main_volume: volume.raw().to_owned(),
                my_files_root: my_files.base.uid.to_string(),
                main_event: p
                    .call(
                        "volume latest event",
                        p.drive.get_volume_latest_event_id(volume),
                    )
                    .await?,
                core_event: p
                    .soft("core latest event", p.drive.get_core_latest_event_id())
                    .await?,
                ..State::default()
            };
            if let Some(root) = p
                .soft("photos root", p.photos.get_photos_root_folder())
                .await?
            {
                let volume = root.base.uid.volume_id.clone();
                state.photos_event = p
                    .soft(
                        "volume latest event (photos)",
                        p.drive.get_volume_latest_event_id(volume.clone()),
                    )
                    .await?;
                state.photos_volume = Some(volume.raw().to_owned());
                state.photos_root = Some(root.base.uid.to_string());
            }
            state
        }
    };
    // The SDK decides `direct_role` of own volumes from its cache of them.
    if p.soft("get_my_files_folder", p.drive.get_my_files_folder())
        .await?
        .is_none()
    {
        bail!("My Files is not reachable");
    }
    if state.photos_volume.is_some() {
        let _ = p
            .soft("photos root", p.photos.get_photos_root_folder())
            .await?;
    }
    let now = Instant::now();
    let mut w = Watch {
        tree: Tree::default(),
        pollers: Vec::new(),
        core: state.core_event.take(),
        core_next: now + CORE_POLL,
        listing_next: now + LISTING_POLL,
        invitation_cache: BTreeMap::new(),
        invitations: BTreeMap::new(),
        shared: BTreeMap::new(),
        devices: BTreeMap::new(),
        roots: BTreeMap::new(),
        state,
    };
    w.tree
        .roots
        .insert(w.state.my_files_root.clone(), Place::MyFiles);
    if let Some(root) = w.state.photos_root.clone() {
        w.tree.roots.insert(root, Place::Photos);
    }
    w.add_poller(
        w.state.main_volume.clone(),
        VolumeKind::Own,
        w.state.main_event.clone(),
    );
    if let (Some(volume), Some(event)) =
        (w.state.photos_volume.clone(), w.state.photos_event.clone())
        && volume != w.state.main_volume
    {
        w.add_poller(volume, VolumeKind::Photos, event);
    }

    p.say("== Initial listings");
    listings(p, &mut w, true).await?;
    let deadline = Instant::now() + Duration::from_secs(seconds);
    p.say(format!("== Watching for {seconds} s"));
    loop {
        enum Task {
            Volume(usize),
            Core,
            Listings,
        }
        let mut task = (Task::Listings, w.listing_next);
        if w.core.is_some() && w.core_next < task.1 {
            task = (Task::Core, w.core_next);
        }
        for (index, poller) in w.pollers.iter().enumerate() {
            if poller.alive && poller.next < task.1 {
                task = (Task::Volume(index), poller.next);
            }
        }
        if task.1 > deadline {
            break;
        }
        tokio::time::sleep_until(task.1.into()).await;
        match task.0 {
            Task::Volume(index) => {
                poll_volume(p, &mut w, index).await?;
                let poller = &mut w.pollers[index];
                poller.next = Instant::now() + poller.every;
            }
            Task::Core => {
                let signalled = poll_core(p, &mut w).await?;
                w.core_next = Instant::now() + CORE_POLL;
                if signalled {
                    p.say("  core event asks for a shared-with-me refresh: listing now");
                    listings(p, &mut w, false).await?;
                    w.listing_next = Instant::now() + LISTING_POLL;
                }
            }
            Task::Listings => {
                listings(p, &mut w, false).await?;
                w.listing_next = Instant::now() + LISTING_POLL;
            }
        }
    }
    p.say("== Watch ended");
    Ok(())
}

/// Polls the core event stream; returns whether it signalled a
/// shared-with-me change (`DriveShareRefresh.Action == 2` or `Refresh`).
async fn poll_core(p: &Probe, w: &mut Watch) -> anyhow::Result<bool> {
    let Some(cursor) = w.core.clone() else {
        return Ok(false);
    };
    let mut signalled = false;
    let mut cursor = cursor;
    for _ in 0..10 {
        let Some(page) = p
            .soft("core events", p.drive.poll_core_events(&cursor))
            .await?
        else {
            return Ok(signalled);
        };
        let share_refresh = page.drive_share_refresh.as_ref().map(|r| r.action);
        if share_refresh.is_some() || page.refresh != 0 {
            p.say(format!(
                "CORE event={} refresh={} more={} drive_share_refresh_action={share_refresh:?}",
                page.event_id, page.refresh, page.more
            ));
        }
        signalled |= page.refresh > 0 || share_refresh == Some(2);
        cursor = page.event_id.clone();
        if page.more == 0 {
            break;
        }
    }
    w.core = Some(cursor);
    Ok(signalled)
}

/// Fetches the nodes in `uids` (one volume) and records their parents;
/// returns their summaries. Nodes the API does not return are recorded as
/// tops, so the walk does not ask for them again.
async fn fetch_nodes(
    p: &Probe,
    w: &mut Watch,
    volume: &str,
    uids: &BTreeSet<String>,
) -> anyhow::Result<HashMap<String, String>> {
    let links: Vec<LinkId> = uids
        .iter()
        .filter_map(|uid| parse_uid(uid))
        .map(|uid| uid.link_id)
        .collect();
    if links.is_empty() {
        return Ok(HashMap::new());
    }
    let client = p.client_for(volume, w.photos_volume());
    let nodes = p
        .soft(
            "get_nodes_uncached",
            client.get_nodes_uncached(VolumeId::new(volume.to_owned()), links),
        )
        .await?
        .unwrap_or_default();
    let mut summaries = HashMap::new();
    for node in &nodes {
        let uid = node_uid(node);
        w.tree.parents.insert(uid.clone(), node_parent(node));
        summaries.insert(uid, node_summary(node));
    }
    for uid in uids {
        w.tree.parents.entry(uid.clone()).or_insert(None);
    }
    Ok(summaries)
}

async fn poll_volume(p: &Probe, w: &mut Watch, index: usize) -> anyhow::Result<()> {
    let (volume, kind, mut cursor) = {
        let poller = &w.pollers[index];
        (poller.volume.clone(), poller.kind, poller.cursor.clone())
    };
    let label = match kind {
        VolumeKind::Own => "own".to_owned(),
        VolumeKind::Photos => "photos".to_owned(),
        VolumeKind::Other => format!("other:{volume}"),
    };
    for _ in 0..20 {
        let page = match p
            .call(
                "volume events",
                p.drive
                    .poll_volume_events(VolumeId::new(volume.clone()), &cursor),
            )
            .await
        {
            Ok(page) => page,
            Err(error) if is_stop(&error) => return Err(error),
            Err(error) => {
                p.say(format!(
                    "EVENTS vol={label} ERR {}{}",
                    describe_error(&error),
                    if is_not_found(&error) {
                        " (not found: the JS client ends this tree, TreeRemove; polling stops)"
                    } else {
                        ""
                    }
                ));
                // As the JS client: only "not found" ends a volume's polling,
                // so a revoked share shows what its volume returns each minute.
                if is_not_found(&error) {
                    w.pollers[index].alive = false;
                }
                return Ok(());
            }
        };
        if page.refresh {
            p.say(format!(
                "EVENTS vol={label} REFRESH requested (event {})",
                page.event_id
            ));
        }
        let list = changes(&volume, &page);
        // Parents from the events themselves; fetch the nodes still alive.
        // An event's parent is the current one (a move changes it).
        for change in &list {
            if let Some(parent) = &change.parent {
                w.tree
                    .parents
                    .insert(change.uid.clone(), Some(parent.clone()));
            }
        }
        let alive: BTreeSet<String> = list
            .iter()
            .filter(|change| change.kind != "Delete")
            .map(|change| change.uid.clone())
            .collect();
        let summaries = fetch_nodes(p, w, &volume, &alive).await?;
        // Walk up to a known root, a few batched rounds at most.
        if kind != VolumeKind::Photos {
            for _ in 0..6 {
                let missing: BTreeSet<String> = list
                    .iter()
                    .filter_map(|change| match w.tree.place_of(&change.uid) {
                        Err(Unplaced::Missing(uid)) if uid.starts_with(&volume) => Some(uid),
                        _ => None,
                    })
                    .collect();
                if missing.is_empty() {
                    break;
                }
                fetch_nodes(p, w, &volume, &missing).await?;
            }
        }
        for change in &list {
            let place = if kind == VolumeKind::Photos {
                Place::Photos.to_string()
            } else {
                match w.tree.place_of(&change.uid) {
                    Ok(place) => place.to_string(),
                    Err(Unplaced::Top(uid)) => format!("unknown(top {uid})"),
                    Err(Unplaced::Missing(uid)) => format!("unknown(missing {uid})"),
                }
            };
            p.say(change_line(
                &label,
                change,
                &place,
                summaries.get(&change.uid).map(String::as_str),
            ));
            if w.roots.contains_key(&change.uid) {
                p.say("  the event touches a share or device root: re-reading the roots");
                roots(p, w).await?;
            }
        }
        cursor = page.event_id.clone();
        if !page.more {
            break;
        }
    }
    w.pollers[index].cursor = cursor;
    Ok(())
}

/// Re-reads every share and device root (name and roles), printing changes.
async fn roots(p: &Probe, w: &mut Watch) -> anyhow::Result<()> {
    let mut now = BTreeMap::new();
    for uid in w.roots.keys().cloned().collect::<Vec<_>>() {
        let Some(node_uid) = parse_uid(&uid) else {
            continue;
        };
        match p
            .call("get_node root", p.drive.get_node_uncached(node_uid))
            .await
        {
            Ok(node) => {
                now.insert(uid, node_summary(&node));
            }
            Err(error) if is_stop(&error) => return Err(error),
            Err(error) => {
                now.insert(uid, format!("UNREADABLE {}", describe_error(&error)));
            }
        }
    }
    for line in diff("ROOT", &w.roots, &now) {
        p.say(line);
    }
    w.roots = now;
    Ok(())
}

/// Invitations, shared-with-me, devices and the roots' roles.
async fn listings(p: &Probe, w: &mut Watch, initial: bool) -> anyhow::Result<()> {
    if let Some(now) = invitations(p, &mut w.invitation_cache, initial).await? {
        for line in diff("INVITATION", &w.invitations, &now) {
            p.say(line);
        }
        w.invitations = now;
    }

    if let Some(now) = shared_with_me(p).await? {
        for line in diff(
            "SHARED",
            &shared_summaries(&w.shared),
            &shared_summaries(&now),
        ) {
            p.say(line);
        }
        for (uid, entry) in &now {
            if w.shared.contains_key(uid) {
                continue;
            }
            let name = describe_share(p, uid, entry).await?;
            w.tree.roots.insert(uid.clone(), Place::Share(name));
            w.roots.insert(uid.clone(), String::new());
            if entry.volume != w.state.main_volume
                && let Some(event) = p
                    .soft(
                        "volume latest event (share)",
                        p.drive
                            .get_volume_latest_event_id(VolumeId::new(entry.volume.clone())),
                    )
                    .await?
            {
                p.say(format!(
                    "  polling volume {} every {} s from event {event}",
                    entry.volume,
                    OTHER_VOLUME_POLL.as_secs()
                ));
                w.add_poller(entry.volume.clone(), VolumeKind::Other, event);
            }
        }
        for uid in w.shared.keys() {
            if !now.contains_key(uid) {
                p.say(format!(
                    "  share {uid} left the listing (revoked or left); its volume poller stays, to see what the API returns"
                ));
            }
        }
        w.shared = now;
    }

    if let Some(listing) = devices(p).await? {
        let now = device_summaries(&listing);
        for line in diff("DEVICE", &w.devices, &now) {
            p.say(line);
        }
        for (id, device) in &listing {
            if w.devices.contains_key(id) {
                continue;
            }
            let name = describe_device(p, device, &w.state.main_volume).await?;
            let root =
                NodeUid::new(device.volume_id.clone(), device.root_link_id.clone()).to_string();
            w.tree.roots.insert(root.clone(), Place::Device(name));
            w.roots.insert(root, String::new());
        }
        w.devices = now;
    }

    if !w.roots.is_empty() {
        roots(p, w).await?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Entry point

#[test]
#[ignore = "real account (M5.9); run by hand"]
fn probe_roots() -> anyhow::Result<()> {
    // The SDK needs the daemon's worker stack size (main.rs).
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(crate::WORKER_STACK_SIZE)
        .build()?
        .block_on(async { tokio::spawn(run()).await? })
}

async fn run() -> anyhow::Result<()> {
    use tracing_subscriber::Layer;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    let _ = tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_test_writer()
                .with_filter(tracing_subscriber::EnvFilter::new("warn,pgp=error")),
        )
        .with(
            CountLayer.with_filter(
                tracing_subscriber::filter::Targets::new()
                    .with_target("h2", tracing::Level::TRACE)
                    .with_target("proton_drive_sdk::http::retry", tracing::Level::INFO),
            ),
        )
        .try_init();
    credentials::set_mode(match std::env::var("PROBE_SECRETS").as_deref() {
        Ok("force-file") => SecretsMode::ForceFile,
        Ok("allow-file") => SecretsMode::AllowFile,
        _ => SecretsMode::Keyring,
    });
    let profile = std::env::var("PROBE_PROFILE").unwrap_or_else(|_| "default".into());
    let phase = std::env::var("PROBE_PHASE").unwrap_or_else(|_| "snapshot".into());
    let seconds = std::env::var("PROBE_WATCH_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(600);
    if !matches!(phase.as_str(), "snapshot" | "watch") {
        bail!("PROBE_PHASE must be snapshot or watch");
    }

    let paths = ProfilePaths::for_current_user(profile.parse()?)?;
    let logged_in = match session::resume(&paths).await? {
        Resumed::Active(logged_in) => logged_in,
        Resumed::NotLoggedIn => bail!("profile {profile} is not logged in"),
        Resumed::NeedsLogin { error, .. } => return Err(error.context("needs login")),
        Resumed::Offline { error, .. } => return Err(error.context("offline")),
    };
    let mut saver = TokenSaver::of(&logged_in).await;
    let limits = session::TransferLimits::of(&pdmount_core::Config::default());
    let drive = limits.apply(ProtonDriveClient::new(
        &logged_in.session,
        Some(CLIENT_UID.into()),
    )?);
    let photos = ProtonPhotosClient::new(&logged_in.session, Some(CLIENT_UID.into()))?;
    let dir = out_dir();
    std::fs::create_dir_all(&dir)?;
    let log = std::fs::File::create(dir.join(format!("{phase}-{}.log", now_unix()))).ok();
    let session = logged_in.session;
    let p = Probe {
        session,
        drive,
        photos,
        start: Instant::now(),
        log: parking_lot::Mutex::new(log),
    };
    p.say(format!("probe_roots: phase={phase} profile={profile}"));

    let result = match phase.as_str() {
        "snapshot" => match snapshot(&p).await {
            Ok(state) => {
                std::fs::write(
                    dir.join("state.json"),
                    serde_json::to_string_pretty(&state)?,
                )?;
                p.say(format!("state saved: {state:?}"));
                Ok(())
            }
            Err(error) => Err(error),
        },
        _ => watch(&p, seconds).await,
    };
    if let Err(error) = &result {
        p.say(format!("probe stopped: {error:#}"));
    }
    p.report_counts();
    saver
        .save(&paths, TokenSaver::snapshot(&p.session).await?)
        .await?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_size_comes_from_ihdr() {
        let mut png = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR".to_vec();
        png.extend_from_slice(&512u32.to_be_bytes());
        png.extend_from_slice(&384u32.to_be_bytes());
        assert_eq!(
            image_info(&png),
            ImageInfo {
                format: "png".into(),
                size: Some((512, 384))
            }
        );
    }

    #[test]
    fn jpeg_size_comes_from_the_first_frame_segment() {
        // SOI, APP0 (length 4), DHT (length 3), SOF0 with height 300 and width 512.
        let jpeg = [
            0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x04, 0x00, 0x00, 0xFF, 0xC4, 0x00, 0x03, 0x00, 0xFF,
            0xC0, 0x00, 0x11, 0x08, 0x01, 0x2C, 0x02, 0x00, 0x03,
        ];
        assert_eq!(image_info(&jpeg).format, "jpeg");
        assert_eq!(image_info(&jpeg).size, Some((512, 300)));
        assert_eq!(image_info(&[0xFF, 0xD8, 0x00]).size, None);
    }

    #[test]
    fn webp_sizes_of_each_chunk_kind() {
        let riff = |chunk: &[u8], body: &[u8]| {
            let mut b = b"RIFF\x00\x00\x00\x00WEBP".to_vec();
            b.extend_from_slice(chunk);
            b.extend_from_slice(&[0, 0, 0, 0]);
            b.extend_from_slice(body);
            b
        };
        let lossy = riff(
            b"VP8 ",
            &[0, 0, 0, 0x9d, 0x01, 0x2a, 0x00, 0x02, 0x80, 0x01],
        );
        assert_eq!(image_info(&lossy).size, Some((512, 384)));
        // Width 512 and height 384, stored minus one in 14 bits each.
        let bits: u32 = 511 | (383 << 14);
        let mut lossless_body = vec![0x2f];
        lossless_body.extend_from_slice(&bits.to_le_bytes());
        assert_eq!(
            image_info(&riff(b"VP8L", &lossless_body)).size,
            Some((512, 384))
        );
        let extended = riff(b"VP8X", &[0, 0, 0, 0, 0xff, 0x01, 0x00, 0x7f, 0x01, 0x00]);
        assert_eq!(image_info(&extended).size, Some((512, 384)));
        assert_eq!(image_info(&extended).format, "webp");
    }

    #[test]
    fn other_formats_are_named() {
        assert_eq!(
            image_info(b"\x00\x00\x00\x18ftypheic").format,
            "isobmff/heic"
        );
        assert_eq!(image_info(b"GIF89a\x10\x00\x20\x00").size, Some((16, 32)));
        assert_eq!(image_info(b"abc").format, "unknown(616263)");
    }

    #[test]
    fn diff_lists_added_changed_and_removed() {
        let map = |entries: &[(&str, &str)]| {
            entries
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<BTreeMap<_, _>>()
        };
        let old = map(&[("a", "viewer"), ("b", "x")]);
        let new = map(&[("a", "editor"), ("c", "y")]);
        assert_eq!(
            diff("ROOT", &old, &new),
            vec![
                "ROOT CHANGED a: viewer  ->  editor",
                "ROOT ADDED c: y",
                "ROOT REMOVED b: x",
            ]
        );
        assert!(diff("ROOT", &old, &old).is_empty());
    }

    #[test]
    fn tree_places_nodes_under_the_nearest_known_root() {
        let mut tree = Tree::default();
        tree.roots.insert("v~root".into(), Place::MyFiles);
        tree.roots
            .insert("v~dev".into(), Place::Device("laptop".into()));
        tree.roots
            .insert("o~share".into(), Place::Share("docs".into()));
        tree.parents.insert("v~a".into(), Some("v~root".into()));
        tree.parents.insert("v~b".into(), Some("v~a".into()));
        tree.parents.insert("v~d1".into(), Some("v~dev".into()));
        tree.parents.insert("o~f".into(), Some("o~share".into()));
        tree.parents.insert("v~orphan".into(), None);
        tree.parents.insert("v~c".into(), Some("v~unknown".into()));
        assert_eq!(tree.place_of("v~b"), Ok(Place::MyFiles));
        assert_eq!(tree.place_of("v~d1"), Ok(Place::Device("laptop".into())));
        assert_eq!(tree.place_of("o~share"), Ok(Place::Share("docs".into())));
        assert_eq!(tree.place_of("o~f").unwrap().to_string(), "share:docs");
        assert_eq!(
            tree.place_of("v~orphan"),
            Err(Unplaced::Top("v~orphan".into()))
        );
        assert_eq!(
            tree.place_of("v~c"),
            Err(Unplaced::Missing("v~unknown".into()))
        );
        // A cycle ends rather than loops.
        tree.parents.insert("v~x".into(), Some("v~y".into()));
        tree.parents.insert("v~y".into(), Some("v~x".into()));
        assert!(matches!(tree.place_of("v~x"), Err(Unplaced::Top(_))));
    }

    #[test]
    fn volume_events_become_printed_changes() {
        let page: VolumeEventsResponse = serde_json::from_value(serde_json::json!({
            "Code": 1000,
            "EventID": "e9",
            "More": 0,
            "Refresh": false,
            "Events": [
                {"EventID": "e8", "EventType": 1,
                 "Link": {"LinkID": "new", "ParentLinkID": "share", "IsShared": 0, "IsTrashed": false}},
                {"EventID": "e9", "EventType": 0,
                 "Link": {"LinkID": "gone", "IsShared": false, "IsTrashed": true}}
            ]
        }))
        .unwrap();
        let list = changes("vol", &page);
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].kind, "Create");
        assert_eq!(list[0].uid, "vol~new");
        assert_eq!(list[0].parent.as_deref(), Some("vol~share"));
        assert_eq!(list[1].kind, "Delete");
        assert_eq!(list[1].parent, None);
        assert_eq!(
            change_line("other:vol", &list[0], "share:docs", Some("File \"a.txt\"")),
            "EVENT vol=other:vol id=e8 type=Create node=vol~new parent=vol~share shared=false \
             trashed=false place=share:docs | File \"a.txt\""
        );
        assert!(change_line("own", &list[1], "my-files", None).ends_with("(not fetched)"));
    }

    #[test]
    fn timeline_fields_are_listed_and_parsed() {
        let raw = serde_json::json!({
            "Code": 1000,
            "Photos": [{
                "LinkID": "l1", "CaptureTime": 1_700_000_000, "Hash": "h", "ContentHash": "c",
                "Tags": [3], "RelatedPhotos": [
                    {"LinkID": "l2", "CaptureTime": 1_700_000_000, "Hash": "h2", "ContentHash": null}
                ]
            }]
        });
        assert_eq!(field_types(&raw), "Code:number, Photos:array");
        let item = &raw["Photos"][0];
        assert!(field_types(item).contains("RelatedPhotos:array"));
        let page: TimelinePhotoListResponse = serde_json::from_value(raw).unwrap();
        assert_eq!(page.photos[0].related_photos[0].id.raw(), "l2");
        assert_eq!(format!("{:?}", page.photos[0].tags), "[LivePhoto]");
    }

    #[test]
    fn rate_limits_are_recognised_through_context() {
        let error = anyhow::Error::new(RateLimited).context("label");
        assert!(is_stop(&error));
        assert!(!is_stop(&anyhow::anyhow!("other")));
        assert_eq!(
            parse_uid("vol~link").map(|uid| uid.to_string()),
            Some("vol~link".into())
        );
        assert!(parse_uid("nolink").is_none());
    }
}
