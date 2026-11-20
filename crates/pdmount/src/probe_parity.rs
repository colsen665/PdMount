//! M6.0.4 real-account checks of the parity patches (docs/sdk-patches.md:
//! P2, P4, P21–P24, P27, P28). It writes only under `My Files/pdmount-test/`
//! (D028): a run folder `parity/run-<time>`, deleted at the end, and
//! `pdmount-test/p21-small.txt`, which stays for a look in the web client. It
//! reads My Files, the shares and Photos. Calls are sequential, listings are
//! paced at most two starts a second (DESIGN §5.3), and the probe stops at the
//! first HTTP 429.
//!
//! ```sh
//! PROBE_PROFILE=default PROBE_SECRETS=force-file [PROBE_ONLY=P28,P22,P2,P23,P21,P24,P27] \
//!   [PROBE_P2_WAIT=600] cargo test --release -p pdmount probe_parity -- --ignored --nocapture
//! ```
//!
//! Use a release build: P27's walk decrypts every node, which a debug build
//! does at a few nodes a second (M6.0.4: 3 600 nodes took about 7 minutes in
//! release, paced by the listings).
//!
//! The output holds counts, codes and kinds only: names, ids and addresses
//! are left out or redacted. The SDK's own log lines (WARN and above) go to
//! `target/probe-parity/<unix time>.log`, which may name nodes.
//!
//! - P28: the addresses' key versions and primary keys; a folder, an upload,
//!   renames and a move, read back with their signatures checked.
//! - P22: a commit sent again for an active revision: its answer, the
//!   revision's state afterwards, and so what the fork's seal path decides.
//! - P2: a block URL fetched at once and again after `PROBE_P2_WAIT` seconds
//!   (default 600): whether an expired target answers 404.
//! - P23: a new file's and a revision's draft announcing more than the
//!   remaining quota; no content is sent.
//! - P21: the small-file endpoints with `DriveSmallFileUpload` on in this
//!   client only: a new file, a revision, an empty file, a name conflict and a
//!   stale `CurrentRevisionID`, each read back.
//! - P24: the membership of each node shared with the account.
//! - P27, P4: every node of My Files, the shares and the Photos timeline,
//!   decoded without the entity cache: unverified authors, degraded nodes,
//!   XAttr fields that do not parse, pre-2025 block sizes.

use crate::credentials::{self, SecretsMode};
use crate::probe_roots::{CountLayer, REQUESTS, RateLimited, SAW_429};
use crate::session::{self, Resumed, TokenSaver};
use anyhow::{Context, bail};
use pdmount_core::ProfilePaths;
use proton_drive_sdk::api::revision::RevisionUpdateRequest;
use proton_drive_sdk::client::ProtonDriveClient;
use proton_drive_sdk::error::ApiError;
use proton_drive_sdk::links::LinkId;
use proton_drive_sdk::node::draft::{NodeWithSameNameExistsError, RevisionDraftConflictError};
use proton_drive_sdk::node::revision::{RevisionState, RevisionUid};
use proton_drive_sdk::node::{DegradedNode, Node, NodeUid};
use proton_drive_sdk::pgp::PgpPrivateKey;
use proton_drive_sdk::photo::ProtonPhotosClient;
use proton_drive_sdk::proton_sdk_rs2::account::ProtonAccountClient;
use proton_drive_sdk::proton_sdk_rs2::cache::InMemoryCacheRepository;
use proton_drive_sdk::proton_sdk_rs2::client::{FeatureFlagProvider, Telemetry};
use proton_drive_sdk::proton_sdk_rs2::crypto::proton_rpgp::AccessKeyInfo;
use proton_drive_sdk::utils::PotentialObject;
use proton_drive_sdk::volume::VolumeId;
use std::collections::{BTreeMap, VecDeque};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::task::{Context as TaskContext, Poll};
use std::time::{Duration, Instant, SystemTime};
use tokio::io::{AsyncRead, ReadBuf};

const CLIENT_UID: &str = "pdmount-parityprobe";
/// At most two request starts a second for listings (DESIGN §5.3).
const PACE: Duration = Duration::from_millis(500);
/// Links fetched per batch (Photos).
const BATCH: usize = 100;
/// 2025-01-01T00:00:00Z: block sizes of older revisions are sorted (P4).
const SORTED_SINCE: i64 = 1_735_689_600;

// ---------------------------------------------------------------------------
// What the SDK logs and reports

/// WARN lines by message (and `fields` / `field`), and the texts of the small
/// upload's fallback warnings.
static WARNINGS: parking_lot::Mutex<BTreeMap<String, u64>> =
    parking_lot::Mutex::new(BTreeMap::new());
static FALLBACKS: parking_lot::Mutex<Vec<String>> = parking_lot::Mutex::new(Vec::new());

#[derive(Default)]
struct WarnFields {
    message: String,
    extra: String,
}

impl tracing::field::Visit for WarnFields {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        match field.name() {
            "message" => self.message = format!("{value:?}"),
            "fields" | "field" => self.extra = format!("{value:?}"),
            _ => {}
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        match field.name() {
            "message" => self.message = value.to_owned(),
            "fields" | "field" => self.extra = value.to_owned(),
            _ => {}
        }
    }
}

struct WarnLayer;

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for WarnLayer {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        if *event.metadata().level() > tracing::Level::WARN {
            return;
        }
        let mut fields = WarnFields::default();
        event.record(&mut fields);
        if fields.message.starts_with("Small upload refused") {
            FALLBACKS.lock().push(fields.message.clone());
        }
        // The key leaves out the variable part after a colon (error texts).
        let head = fields
            .message
            .split(": ")
            .next()
            .unwrap_or_default()
            .to_owned();
        let key = if fields.extra.is_empty() {
            head
        } else {
            format!("{head} [{}]", fields.extra)
        };
        *WARNINGS.lock().entry(key).or_default() += 1;
    }
}

fn warnings_matching(text: &str) -> u64 {
    WARNINGS
        .lock()
        .iter()
        .filter(|(key, _)| key.contains(text))
        .map(|(_, count)| count)
        .sum()
}

/// Telemetry of the probe's clients: upload routes and integrity reports.
#[derive(Default)]
struct Recorder {
    routes: parking_lot::Mutex<Vec<String>>,
    integrity: parking_lot::Mutex<BTreeMap<String, u64>>,
}

#[async_trait::async_trait]
impl Telemetry for Recorder {
    async fn record_metric(&self, name: String, payload: Option<Vec<u8>>) {
        let json = payload
            .as_deref()
            .and_then(|p| serde_json::from_slice::<serde_json::Value>(p).ok());
        let field = |key: &str| {
            json.as_ref()
                .and_then(|j| j.get(key))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("-")
                .to_owned()
        };
        match name.as_str() {
            "uploadPerformance" => self.routes.lock().push(field("route")),
            "verificationError" | "decryptionError" => {
                *self
                    .integrity
                    .lock()
                    .entry(format!("{name} {}", field("field")))
                    .or_default() += 1;
            }
            _ => {}
        }
    }
}

impl Recorder {
    fn last_route(&self) -> String {
        self.routes
            .lock()
            .last()
            .cloned()
            .unwrap_or_else(|| "-".into())
    }
}

/// `DriveSmallFileUpload` on, every other flag off.
struct SmallUploadOn;

#[async_trait::async_trait]
impl FeatureFlagProvider for SmallUploadOn {
    async fn is_enabled(&self, flag_name: String) -> anyhow::Result<bool> {
        Ok(flag_name == proton_drive_sdk::node::file::upload::DRIVE_SMALL_FILE_UPLOAD)
    }
}

/// A source that fails at its first read: the draft is made, no content is sent.
struct NoContent;

impl AsyncRead for NoContent {
    fn poll_read(
        self: Pin<&mut Self>,
        _cx: &mut TaskContext<'_>,
        _buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Poll::Ready(Err(std::io::Error::other(
            "probe: no content is sent for this draft",
        )))
    }
}

// ---------------------------------------------------------------------------
// Output without account data

/// Replaces addresses and long identifiers in `text`.
fn redact(text: &str) -> String {
    // Runs of characters that may form an address or an id, and the rest.
    let is_part = |c: char| c.is_ascii_alphanumeric() || "-_=~+/.@".contains(c);
    let mut out = String::new();
    let mut run = String::new();
    let flush = |run: &mut String, out: &mut String| {
        if run.contains('@') {
            out.push_str("<address>");
        } else if run.len() >= 16 && run.chars().any(|c| c.is_ascii_digit()) {
            out.push_str("<id>");
        } else {
            out.push_str(run);
        }
        run.clear();
    };
    for c in text.chars() {
        if is_part(c) {
            run.push(c);
        } else {
            flush(&mut run, &mut out);
            out.push(c);
        }
    }
    flush(&mut run, &mut out);
    out
}

fn describe_error(error: &anyhow::Error) -> String {
    let typed = if error
        .downcast_ref::<NodeWithSameNameExistsError>()
        .is_some()
    {
        " typed=NodeWithSameNameExists"
    } else if error.downcast_ref::<RevisionDraftConflictError>().is_some() {
        " typed=RevisionDraftConflict"
    } else {
        ""
    };
    match ApiError::find(error) {
        Some(api) => format!(
            "http={:?} code={:?} message={:?}{typed}",
            api.http_status,
            api.code,
            redact(&api.message)
        ),
        None => {
            let text = redact(&format!("{error:#}"));
            format!("{}{typed}", text.chars().take(300).collect::<String>())
        }
    }
}

fn outcome<T>(result: &anyhow::Result<T>) -> String {
    match result {
        Ok(_) => "ok".into(),
        Err(error) => format!("ERR {}", describe_error(error)),
    }
}

/// PdMount's mapping of an SDK error (DESIGN Appendix A).
fn mapped(error: &anyhow::Error) -> String {
    let mapped = format!("{:?}", pdmount_backend::proton::classify(error));
    redact(&mapped).chars().take(120).collect()
}

fn author_state<E: std::fmt::Debug>(
    author: &PotentialObject<proton_drive_sdk::author::Author, E>,
) -> &'static str {
    match author {
        PotentialObject::Node(a) if a.email_address.is_none() => "anonymous",
        PotentialObject::Node(_) => "verified",
        PotentialObject::Degraded(_) => "UNVERIFIED",
    }
}

// ---------------------------------------------------------------------------
// The probe

struct Probe {
    plain: ProtonDriveClient,
    small: ProtonDriveClient,
    photos: ProtonPhotosClient,
    account: ProtonAccountClient,
    recorder: Arc<Recorder>,
    start: Instant,
    last_start: tokio::sync::Mutex<Instant>,
}

impl Probe {
    fn say(&self, line: impl AsRef<str>) {
        println!(
            "[+{:6.1}s] {}",
            self.start.elapsed().as_secs_f64(),
            line.as_ref()
        );
    }

    /// Waits so that calls start at most every [`PACE`].
    async fn pace(&self) {
        let mut last = self.last_start.lock().await;
        let next = *last + PACE;
        tokio::time::sleep_until(next.into()).await;
        *last = Instant::now();
    }

    /// Fails with [`RateLimited`] when a 429 was seen anywhere, the SDK's
    /// silent retries included.
    fn check<T>(&self, label: &str, result: &anyhow::Result<T>) -> anyhow::Result<()> {
        let limited = SAW_429.load(Ordering::Relaxed)
            || result
                .as_ref()
                .err()
                .and_then(ApiError::find)
                .is_some_and(|api| api.is_rate_limited() || api.code == Some(2028));
        if limited {
            return Err(anyhow::Error::new(RateLimited).context(label.to_owned()));
        }
        Ok(())
    }

    /// Runs a paced call; a 429 ends the probe, other errors are returned.
    async fn call<T>(
        &self,
        label: &str,
        call: impl std::future::Future<Output = anyhow::Result<T>>,
    ) -> anyhow::Result<anyhow::Result<T>> {
        self.pace().await;
        let result = call.await;
        self.check(label, &result)?;
        Ok(result)
    }

    /// Like `call`, but any error ends the probe.
    async fn must<T>(
        &self,
        label: &str,
        call: impl std::future::Future<Output = anyhow::Result<T>>,
    ) -> anyhow::Result<T> {
        self.call(label, call)
            .await?
            .with_context(|| label.to_owned())
    }

    async fn upload(
        &self,
        client: &ProtonDriveClient,
        parent: &NodeUid,
        name: &str,
        data: &[u8],
    ) -> anyhow::Result<anyhow::Result<RevisionUid>> {
        self.call("upload", async {
            let uploader = client
                .get_file_uploader(
                    parent.clone(),
                    name.to_owned(),
                    "text/plain".into(),
                    data.len() as i64,
                    Some(SystemTime::now()),
                    None,
                    None,
                    false,
                )
                .await?;
            let result = uploader
                .upload_from_stream_with_revision(
                    Box::new(std::io::Cursor::new(data.to_vec())),
                    Vec::new(),
                    Box::new(|_, _| {}),
                )
                .await?;
            Ok(result.revision_uid)
        })
        .await
    }

    async fn upload_revision(
        &self,
        client: &ProtonDriveClient,
        base: &RevisionUid,
        data: &[u8],
    ) -> anyhow::Result<anyhow::Result<RevisionUid>> {
        self.call("upload revision", async {
            let uploader = client
                .get_file_revision_uploader(
                    base.clone(),
                    data.len() as i64,
                    Some(SystemTime::now()),
                    None,
                    None,
                )
                .await?;
            let result = uploader
                .upload_from_stream_with_revision(
                    Box::new(std::io::Cursor::new(data.to_vec())),
                    Vec::new(),
                    Box::new(|_, _| {}),
                )
                .await?;
            Ok(result.revision_uid)
        })
        .await
    }

    /// The revision's content, read with the plain client.
    async fn read_back(&self, revision: &RevisionUid) -> anyhow::Result<anyhow::Result<Vec<u8>>> {
        self.call("read back", async {
            let map = self.plain.get_block_map(revision.clone()).await?;
            let mut data = Vec::new();
            for entry in &map.blocks {
                data.extend(self.plain.download_block(&map, entry.index).await?);
            }
            Ok(data)
        })
        .await
    }

    /// One node, fetched fresh.
    async fn fresh(&self, uid: &NodeUid) -> anyhow::Result<PotentialObject<Node, DegradedNode>> {
        let mut nodes = self
            .must(
                "get_nodes_uncached",
                self.plain
                    .get_nodes_uncached(uid.volume_id.clone(), vec![uid.link_id.clone()]),
            )
            .await?;
        nodes.pop().context("node not returned")
    }

    /// Name, author and content author of a node, as verified or not.
    async fn signatures(&self, uid: &NodeUid) -> anyhow::Result<String> {
        Ok(match self.fresh(uid).await? {
            PotentialObject::Node(node) => {
                let base = node.base();
                let content = match &node {
                    Node::File(f) | Node::Photo(f) => f
                        .active_revision
                        .content_author
                        .as_ref()
                        .map_or("none", author_state),
                    _ => "-",
                };
                format!(
                    "name_author={} author={} content_author={content}",
                    author_state(&base.name_author),
                    author_state(&base.author)
                )
            }
            PotentialObject::Degraded(_) => "DEGRADED".into(),
        })
    }

    async fn child(&self, parent: &NodeUid, name: &str) -> anyhow::Result<Option<Node>> {
        let children = self
            .must(
                "list_children",
                self.plain
                    .list_children(parent.volume_id.clone(), Some(parent.link_id.clone())),
            )
            .await?;
        Ok(children.into_iter().find_map(|child| match child {
            PotentialObject::Node(node)
                if node.base().name == name && node.base().trash_time.is_none() =>
            {
                Some(node)
            }
            _ => None,
        }))
    }

    async fn folder(&self, parent: &NodeUid, name: &str) -> anyhow::Result<NodeUid> {
        if let Some(node) = self.child(parent, name).await? {
            return Ok(node.uid().clone());
        }
        let folder = self
            .must(
                "create_folder",
                self.plain
                    .create_folder(parent.clone(), name.to_owned(), Some(SystemTime::now())),
            )
            .await?;
        Ok(folder.base.uid)
    }
}

fn active_revision(node: &PotentialObject<Node, DegradedNode>) -> Option<RevisionUid> {
    match node {
        PotentialObject::Node(Node::File(f) | Node::Photo(f)) => {
            Some(f.active_revision.uid.clone())
        }
        _ => None,
    }
}

#[test]
#[ignore = "real account (M6.0.4); run by hand"]
fn probe_parity() -> anyhow::Result<()> {
    // The SDK needs the daemon's worker stack size (main.rs).
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(crate::WORKER_STACK_SIZE)
        .build()?
        .block_on(async { tokio::spawn(run()).await? })
}

fn want(id: &str) -> bool {
    std::env::var("PROBE_ONLY").map_or(true, |only| only.split(',').any(|s| s.trim() == id))
}

async fn run() -> anyhow::Result<()> {
    use tracing_subscriber::Layer;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/probe-parity");
    std::fs::create_dir_all(&dir)?;
    let stamp = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)?
        .as_secs();
    let log = std::fs::File::create(dir.join(format!("{stamp}.log")))?;
    let _ = tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(std::sync::Mutex::new(log))
                .with_filter(tracing_subscriber::EnvFilter::new("warn,pgp=error")),
        )
        .with(WarnLayer.with_filter(tracing_subscriber::filter::LevelFilter::WARN))
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
    let paths = ProfilePaths::for_current_user(profile.parse()?)?;
    let logged_in = match session::resume(&paths).await? {
        Resumed::Active(logged_in) => logged_in,
        Resumed::NotLoggedIn => bail!("profile {profile} is not logged in"),
        Resumed::NeedsLogin { error, .. } => return Err(error.context("needs login")),
        Resumed::Offline { error, .. } => return Err(error.context("offline")),
    };
    let mut saver = TokenSaver::of(&logged_in).await;

    // No entity cache: every node is decoded, and so checked, afresh.
    let recorder = Arc::new(Recorder::default());
    let mut session = logged_in.session.clone();
    session.client_config.entity_cache_repository = Arc::new(InMemoryCacheRepository::new());
    session.client_config.telemetry = recorder.clone();
    let limits = session::TransferLimits::of(&pdmount_core::Config::default());
    let plain = limits.apply(ProtonDriveClient::new(&session, Some(CLIENT_UID.into()))?);
    let mut small_session = session.clone();
    small_session.client_config.feature_flag_provider = Arc::new(SmallUploadOn);
    let small = limits.apply(ProtonDriveClient::new(
        &small_session,
        Some(CLIENT_UID.into()),
    )?);
    let photos = ProtonPhotosClient::new(&session, Some(CLIENT_UID.into()))?;
    let account = ProtonAccountClient::new(&session);
    let p = Probe {
        plain,
        small,
        photos,
        account,
        recorder,
        start: Instant::now(),
        last_start: tokio::sync::Mutex::new(Instant::now()),
    };
    p.say(format!("probe_parity: profile {profile}"));

    let result = checks(&p).await;
    if let Err(error) = &result {
        p.say(format!("probe stopped: {}", describe_error(error)));
    }
    p.say(format!(
        "COUNTS: {} HTTP/2 requests; 429 seen: {}",
        REQUESTS.load(Ordering::Relaxed),
        SAW_429.load(Ordering::Relaxed)
    ));
    p.say("SDK warnings by kind:");
    for (key, count) in WARNINGS.lock().iter() {
        p.say(format!("  {count:5}  {}", redact(key)));
    }
    p.say("Integrity telemetry by field (one per node):");
    for (key, count) in p.recorder.integrity.lock().iter() {
        p.say(format!("  {count:5}  {key}"));
    }
    saver
        .save(&paths, TokenSaver::snapshot(&logged_in.session).await?)
        .await?;
    result
}

async fn checks(p: &Probe) -> anyhow::Result<()> {
    let root = p.must("My Files", p.plain.get_my_files_folder()).await?;
    let my_files = root.base.uid.clone();
    let test_root = p
        .child(&my_files, "pdmount-test")
        .await?
        .context("My Files/pdmount-test does not exist")?
        .uid()
        .clone();
    let parity = p.folder(&test_root, "parity").await?;
    // Leftovers of an interrupted run.
    for child in p
        .must(
            "list parity",
            p.plain
                .list_children(parity.volume_id.clone(), Some(parity.link_id.clone())),
        )
        .await?
    {
        if let PotentialObject::Node(node) = child
            && node.base().name.starts_with("run-")
        {
            let removed = p
                .call("remove leftover", remove(&p.plain, node.uid().clone()))
                .await?;
            p.say(format!(
                "cleanup: a leftover run folder: {}",
                outcome(&removed)
            ));
        }
    }
    let stamp = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)?
        .as_secs();
    let run = p
        .must(
            "create run folder",
            p.plain
                .create_folder(parity.clone(), format!("run-{stamp}"), None),
        )
        .await?
        .base
        .uid;
    p.say("run folder: pdmount-test/parity/run-<time>");

    let result = in_run(p, &my_files, &test_root, &run).await;
    let removed = p.call("remove run folder", remove(&p.plain, run)).await;
    match removed {
        Ok(removed) => p.say(format!(
            "cleanup: run folder trashed and deleted: {}",
            outcome(&removed)
        )),
        Err(error) => p.say(format!("cleanup: {}", describe_error(&error))),
    }
    result
}

/// Trashes a node and deletes it from the trash.
async fn remove(client: &ProtonDriveClient, uid: NodeUid) -> anyhow::Result<()> {
    for (_, result) in client.trash_nodes(vec![uid.clone()]).await? {
        result?;
    }
    for (_, result) in client.delete_nodes_from_trash(vec![uid]).await? {
        result?;
    }
    Ok(())
}

async fn in_run(
    p: &Probe,
    my_files: &NodeUid,
    test_root: &NodeUid,
    run: &NodeUid,
) -> anyhow::Result<()> {
    // The default address signs; its primary key is needed for P22's request.
    let address = p
        .must(
            "default address",
            p.account.get_current_user_default_address(),
        )
        .await?;

    if want("P28") {
        p28(p, run).await?;
    }
    let p22_revision = if want("P22") || want("P2") || want("P23") {
        let revision = p
            .upload(&p.plain, run, "p22.txt", b"P22 probe content\n")
            .await?
            .context("P22 upload")?;
        Some(revision)
    } else {
        None
    };
    if want("P22")
        && let Some(revision) = &p22_revision
    {
        p22(p, &address, revision).await?;
    }
    let p2 = match (&p22_revision, want("P2")) {
        (Some(revision), true) => Some(p2_start(p, revision).await?),
        _ => None,
    };
    if want("P23")
        && let Some(revision) = &p22_revision
    {
        p23(p, run, revision).await?;
    }
    if want("P21") {
        p21(p, test_root, run).await?;
    }
    let shares = if want("P24") || want("P27") {
        p24(p).await?
    } else {
        Vec::new()
    };
    if want("P27") {
        p27(p, my_files, &shares).await?;
    }
    if let Some(start) = p2 {
        p2_end(p, start).await?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// P28

async fn p28(p: &Probe, run: &NodeUid) -> anyhow::Result<()> {
    p.say("== P28: address keys");
    let addresses = p
        .must("addresses", p.account.get_current_user_addresses())
        .await?;
    let default = p
        .must(
            "default address",
            p.account.get_current_user_default_address(),
        )
        .await?;
    for (n, address) in addresses.iter().enumerate() {
        let keys = p
            .must(
                "address keys",
                p.account.get_address_private_keys(&address.address_id),
            )
            .await?;
        let versions: Vec<u8> = keys.iter().map(AccessKeyInfo::version).collect();
        let primary = p
            .call(
                "primary key",
                p.account
                    .get_address_primary_private_key(&address.address_id),
            )
            .await?;
        p.say(format!(
            "  address {n}{}: status {:?}, {} active key(s) listed, {} unlocked, versions {versions:?}, \
             primary index {}, primary key {}",
            if address.address_id == default.address_id {
                " (default)"
            } else {
                ""
            },
            address.status,
            address.keys.len(),
            keys.len(),
            address.primary_key_index,
            match &primary {
                Ok(key) => format!(
                    "v{} -> uploads sign with {}",
                    key.version(),
                    if key.version() < 6 {
                        "the primary key"
                    } else {
                        "the first v4 key (v6 guard)"
                    }
                ),
                Err(error) => format!("ERR {}", describe_error(error)),
            }
        ));
    }

    p.say("== P28: a folder, an upload, renames, a move (signed with the primary key)");
    let folder = p
        .call(
            "create folder",
            p.plain
                .create_folder(run.clone(), "p28-dir".into(), Some(SystemTime::now())),
        )
        .await?;
    p.say(format!("  create_folder: {}", outcome(&folder)));
    let folder = folder?.base.uid;
    let file = p
        .upload(&p.plain, &folder, "p28.txt", b"P28 probe\n")
        .await?;
    p.say(format!("  upload: {}", outcome(&file)));
    let file = file?;
    let file_uid = file.node_uid.clone();
    let renamed = p
        .call(
            "rename file",
            p.plain
                .rename_node(file_uid.clone(), "p28-renamed.txt".into(), None),
        )
        .await?;
    p.say(format!("  rename file: {}", outcome(&renamed)));
    let moved = p
        .call(
            "move file",
            p.plain
                .move_node(file_uid.clone(), run.clone(), Some("p28-moved.txt".into())),
        )
        .await?;
    p.say(format!("  move and rename file: {}", outcome(&moved)));
    let folder_renamed = p
        .call(
            "rename folder",
            p.plain
                .rename_node(folder.clone(), "p28-dir-renamed".into(), None),
        )
        .await?;
    p.say(format!("  rename folder: {}", outcome(&folder_renamed)));
    let revision = p
        .upload_revision(&p.plain, &file, b"P28 probe, second revision\n")
        .await?;
    p.say(format!("  new revision: {}", outcome(&revision)));
    p.say(format!(
        "  read back: folder {}; file {}",
        p.signatures(&folder).await?,
        p.signatures(&file_uid).await?
    ));
    if let Ok(revision) = &revision {
        let data = p.read_back(revision).await?;
        p.say(format!(
            "  content of the new revision: {}",
            match &data {
                Ok(data) => format!("equal={}", data == b"P28 probe, second revision\n"),
                Err(error) => format!("ERR {}", describe_error(error)),
            }
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// P22

async fn p22(
    p: &Probe,
    address: &proton_drive_sdk::proton_sdk_rs2::protobuf::Address,
    revision: &RevisionUid,
) -> anyhow::Result<()> {
    p.say("== P22: the commit of an active revision sent again");
    let node = &revision.node_uid;
    // The same request as the commit's, without its XAttr: the manifest
    // signature over the listed block hashes, by the primary key.
    let map = p
        .must("block map", p.plain.get_block_map(revision.clone()))
        .await?;
    let manifest: Vec<u8> = map
        .blocks
        .iter()
        .flat_map(|b| b.hash_digest.iter().copied())
        .collect();
    let key = p
        .must(
            "primary key",
            p.account
                .get_address_primary_private_key(&address.address_id),
        )
        .await?;
    let request = RevisionUpdateRequest {
        manifest_signature: PgpPrivateKey(key).sign_detached_armored(&manifest)?,
        signature_email_address: address.email_address.clone(),
        checksum_verified: false,
        extended_attributes: None,
        photos_attributes: None,
    };
    let files = p.plain.api().files();
    let answer = p
        .call("commit again", async {
            files
                .update_revision(
                    node.volume_id.clone(),
                    node.link_id.clone(),
                    revision.revision_id.clone(),
                    request,
                )
                .await
                .and_then(|response| response.to_result())
        })
        .await?;
    p.say(format!("  second commit: {}", outcome(&answer)));
    let state = p
        .call("revision state", async {
            let response = files
                .get_revision(
                    node.volume_id.clone(),
                    node.link_id.clone(),
                    revision.revision_id.clone(),
                    None,
                    None,
                    true,
                )
                .await?;
            response.base.to_result()?;
            Ok(response.revision.revision.state)
        })
        .await?;
    p.say(format!(
        "  revision state afterwards: {}",
        match &state {
            Ok(state) => format!("{state:?}"),
            Err(error) => format!("ERR {}", describe_error(error)),
        }
    ));
    // `seal_revision` (P22): 2511, then Active or Superseded, is a success.
    let code = answer
        .as_ref()
        .err()
        .and_then(ApiError::find)
        .and_then(|api| api.code);
    let seal = match (&answer, code, &state) {
        (Ok(()), _, _) => "the server accepted the repeated commit (no 2511)".to_owned(),
        (Err(_), Some(2511), Ok(RevisionState::Active | RevisionState::Superseded)) => {
            "2511 then a sealed revision: the fork's seal path reports success".to_owned()
        }
        (Err(_), Some(2511), _) => "2511 and no sealed revision: the seal path fails".to_owned(),
        (Err(_), code, _) => format!("code {code:?}: the seal path returns the error"),
    };
    p.say(format!("  verdict: {seal}"));
    if let Err(error) = &answer {
        p.say(format!(
            "  PdMount maps the bare answer to {}",
            mapped(error)
        ));
    }
    let data = p.read_back(revision).await?;
    p.say(format!(
        "  content afterwards: {}; {}",
        match &data {
            Ok(data) => format!("equal={}", data == b"P22 probe content\n"),
            Err(error) => format!("ERR {}", describe_error(error)),
        },
        p.signatures(node).await?
    ));
    Ok(())
}

// ---------------------------------------------------------------------------
// P2

struct P2Start {
    at: Instant,
    map: proton_drive_sdk::node::file::block_map::BlockMap,
}

async fn blob_status(
    p: &Probe,
    map: &proton_drive_sdk::node::file::block_map::BlockMap,
) -> anyhow::Result<String> {
    let entry = map.blocks.first().context("no block")?;
    let storage = p.plain.api().storage();
    let result = p
        .call("blob GET", async {
            let response = storage
                .get_blob_stream(&entry.bare_url, &entry.token)
                .await?;
            let status = response.status().as_u16();
            let _ = response.bytes().await;
            Ok(status)
        })
        .await?;
    Ok(match result {
        Ok(status) => format!("HTTP {status}"),
        Err(error) => match error
            .chain()
            .find_map(|c| c.downcast_ref::<proton_drive_sdk::error::HttpTransferError>())
        {
            Some(transfer) => format!("HTTP {} (error)", transfer.status),
            None => format!("ERR {}", describe_error(&error)),
        },
    })
}

async fn p2_start(p: &Probe, revision: &RevisionUid) -> anyhow::Result<P2Start> {
    p.say("== P2: a block URL now and later");
    let map = p
        .must("block map", p.plain.get_block_map(revision.clone()))
        .await?;
    let entry = map.blocks.first().context("no block")?;
    // What the URL and token say about a lifetime, without their values.
    let query: Vec<String> = reqwest::Url::parse(&entry.bare_url)
        .map(|url| url.query_pairs().map(|(k, _)| k.into_owned()).collect())
        .unwrap_or_default();
    let parts: Vec<&str> = entry.token.split('.').collect();
    let claims = (parts.len() == 3)
        .then(|| {
            base64url(parts[1])
                .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        })
        .flatten();
    let lifetime = claims.as_ref().map(|claims| {
        let keys: Vec<String> = claims
            .as_object()
            .map(|o| o.keys().cloned().collect())
            .unwrap_or_default();
        let now = chrono::Utc::now().timestamp();
        let exp = claims.get("exp").and_then(serde_json::Value::as_i64);
        format!(
            "claims {keys:?}, expires in {:?} s",
            exp.map(|exp| exp - now)
        )
    });
    p.say(format!(
        "  URL query keys {query:?}; token: {} dot-separated part(s), {} characters; {}",
        parts.len(),
        entry.token.len(),
        lifetime.unwrap_or_else(|| "not a JWT".into())
    ));
    p.say(format!("  GET at once: {}", blob_status(p, &map).await?));
    Ok(P2Start {
        at: Instant::now(),
        map,
    })
}

/// Decodes unpadded base64url (a JWT part).
fn base64url(text: &str) -> Option<Vec<u8>> {
    let mut bits = 0u32;
    let mut count = 0;
    let mut out = Vec::new();
    for c in text.trim_end_matches('=').bytes() {
        let value = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            _ => return None,
        };
        bits = (bits << 6) | u32::from(value);
        count += 6;
        if count >= 8 {
            count -= 8;
            out.push((bits >> count) as u8);
        }
    }
    Some(out)
}

async fn p2_end(p: &Probe, start: P2Start) -> anyhow::Result<()> {
    let wait = std::env::var("PROBE_P2_WAIT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(600);
    let due = start.at + Duration::from_secs(wait);
    if Instant::now() < due {
        p.say(format!(
            "== P2: waiting {:.0} s more (no requests)",
            (due - Instant::now()).as_secs_f64()
        ));
        tokio::time::sleep_until(due.into()).await;
    }
    let status = blob_status(p, &start.map).await?;
    p.say(format!(
        "== P2: the same URL after {:.0} s: {status}",
        start.at.elapsed().as_secs_f64()
    ));
    if status.starts_with("HTTP 404") {
        let read = p
            .call("download after refresh", async {
                p.plain.download_block(&start.map, 1).await
            })
            .await?;
        p.say(format!(
            "  download_block with the old map (refresh path): {}",
            outcome(&read)
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// P23

async fn p23(p: &Probe, run: &NodeUid, revision: &RevisionUid) -> anyhow::Result<()> {
    p.say("== P23: drafts announcing more than the remaining quota (no content sent)");
    let (used, max) = p.must("storage", p.account.get_user_storage_info()).await?;
    let over = (max - used).max(0) + (256 << 20);
    p.say(format!(
        "  remaining {:.2} GiB; announcing {:.2} GiB",
        (max - used) as f64 / f64::from(1u32 << 30),
        over as f64 / f64::from(1u32 << 30)
    ));
    let deletes_before = warnings_matching("failed to delete upload draft");
    let new_file = p
        .call("draft over quota", async {
            let uploader = p
                .plain
                .get_file_uploader(
                    run.clone(),
                    "p23-over-quota.bin".into(),
                    "application/octet-stream".into(),
                    over,
                    Some(SystemTime::now()),
                    None,
                    None,
                    false,
                )
                .await?;
            uploader
                .upload_from_stream_with_revision(
                    Box::new(NoContent),
                    Vec::new(),
                    Box::new(|_, _| {}),
                )
                .await
        })
        .await?;
    report_p23(p, "new file", &new_file);
    let new_revision = p
        .call("revision draft over quota", async {
            let uploader = p
                .plain
                .get_file_revision_uploader(
                    revision.clone(),
                    over,
                    Some(SystemTime::now()),
                    None,
                    None,
                )
                .await?;
            uploader
                .upload_from_stream_with_revision(
                    Box::new(NoContent),
                    Vec::new(),
                    Box::new(|_, _| {}),
                )
                .await
        })
        .await?;
    report_p23(p, "revision", &new_revision);
    p.say(format!(
        "  draft deletions that failed: {}",
        warnings_matching("failed to delete upload draft") - deletes_before
    ));
    // A draft left behind would show as a revision draft or a file in the run folder.
    let revisions = p
        .must(
            "revisions",
            p.plain.iterate_revisions(revision.node_uid.clone()),
        )
        .await?;
    p.say(format!(
        "  revisions of the file afterwards: {:?}",
        revisions.iter().map(|r| r.state).collect::<Vec<_>>()
    ));
    let left = p.child(run, "p23-over-quota.bin").await?;
    p.say(format!(
        "  new file left in the run folder: {}",
        left.is_some()
    ));
    Ok(())
}

fn report_p23<T>(p: &Probe, what: &str, result: &anyhow::Result<T>) {
    match result {
        Ok(_) => p.say(format!("  {what}: ok (UNEXPECTED: no content was sent)")),
        Err(error) => {
            let at_draft = error
                .chain()
                .all(|c| !c.to_string().contains("no content is sent"));
            p.say(format!(
                "  {what}: {} -> {}; PdMount maps it to {}",
                describe_error(error),
                if at_draft {
                    "refused before any content was read (at the draft)"
                } else {
                    "the draft was accepted (the source's error)"
                },
                mapped(error)
            ));
        }
    }
}

// ---------------------------------------------------------------------------
// P21

async fn p21(p: &Probe, test_root: &NodeUid, run: &NodeUid) -> anyhow::Result<()> {
    p.say("== P21: small-file endpoints, DriveSmallFileUpload on in this client only");
    let fallbacks = || FALLBACKS.lock().len();

    let report = |what: &str, result: &anyhow::Result<RevisionUid>, before: usize| {
        let route = p.recorder.last_route();
        let fell_back = FALLBACKS.lock()[before..]
            .iter()
            .map(|text| redact(text))
            .collect::<Vec<_>>();
        p.say(format!(
            "  {what}: {}; route {route}; fallback warnings {fell_back:?}",
            outcome(result)
        ));
        if let Err(error) = result {
            p.say(format!("    PdMount maps it to {}", mapped(error)));
        }
    };
    let check = |what: &'static str, revision: RevisionUid, expected: &'static [u8]| async move {
        let data = p.read_back(&revision).await?;
        let node = p.fresh(&revision.node_uid).await?;
        let size = match &node {
            PotentialObject::Node(Node::File(f)) => format!("{:?}", f.active_revision.claimed_size),
            _ => "-".into(),
        };
        p.say(format!(
            "    {what} read back: {}; claimed size {size}; active is this revision: {}; {}",
            match &data {
                Ok(data) => format!("equal={}", data == expected),
                Err(error) => format!("ERR {}", describe_error(error)),
            },
            active_revision(&node).as_ref() == Some(&revision),
            p.signatures(&revision.node_uid).await?
        ));
        anyhow::Ok(())
    };

    let before = fallbacks();
    let first: &[u8] = b"P21 small file\n";
    let new_file = p.upload(&p.small, run, "p21-new.txt", first).await?;
    report("new small file", &new_file, before);
    if let Ok(revision) = &new_file {
        check("new small file", revision.clone(), first).await?;
    }

    let mut current = new_file.as_ref().ok().cloned();
    if let Some(base) = &new_file.as_ref().ok().cloned() {
        let before = fallbacks();
        let second: &[u8] = b"P21 small revision\n";
        let revision = p.upload_revision(&p.small, base, second).await?;
        report("small revision", &revision, before);
        if let Ok(revision) = &revision {
            check("small revision", revision.clone(), second).await?;
            current = Some(revision.clone());
        }
    }

    let before = fallbacks();
    let empty = p.upload(&p.small, run, "p21-empty.txt", b"").await?;
    report("empty file", &empty, before);
    if let Ok(revision) = &empty {
        check("empty file", revision.clone(), b"").await?;
    }

    let before = fallbacks();
    let conflict = p
        .upload(&p.small, run, "p21-new.txt", b"P21 name conflict\n")
        .await?;
    report("name conflict", &conflict, before);

    if let (Ok(stale), Some(current)) = (&new_file, &current)
        && stale != current
    {
        let before = fallbacks();
        let revision = p
            .upload_revision(&p.small, stale, b"P21 on a stale revision\n")
            .await?;
        report("stale CurrentRevisionID", &revision, before);
    }

    // A file for the user to open in the web client.
    let text = format!(
        "Uploaded by PdMount's parity probe (M6.0.4) through the small-file endpoint (P21) on {}.\n",
        chrono::Utc::now().format("%Y-%m-%d")
    );
    let content: &'static [u8] = Box::leak(text.into_bytes().into_boxed_slice());
    let before = fallbacks();
    let kept = match p.child(test_root, "p21-small.txt").await? {
        Some(Node::File(f)) => {
            p.upload_revision(&p.small, &f.active_revision.uid, content)
                .await?
        }
        Some(_) => bail!("pdmount-test/p21-small.txt is not a file"),
        None => {
            p.upload(&p.small, test_root, "p21-small.txt", content)
                .await?
        }
    };
    report("pdmount-test/p21-small.txt", &kept, before);
    if let Ok(revision) = &kept {
        check("p21-small.txt", revision.clone(), content).await?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// P24

/// The shared roots: (volume, root link, root is a folder).
async fn p24(p: &Probe) -> anyhow::Result<Vec<(VolumeId, LinkId)>> {
    p.say("== P24: memberships of the nodes shared with the account");
    let shares = p.plain.api().shares();
    let mut roots = Vec::new();
    let mut anchor = None;
    loop {
        let page = p
            .must("shared with me", shares.get_shared_with_me(anchor.clone()))
            .await?;
        for link in &page.links {
            roots.push((
                link.volume_id.clone(),
                link.link_id.clone(),
                format!("{:?}", link.share_target_type),
            ));
        }
        if !page.more || page.anchor_id.is_none() {
            break;
        }
        anchor = page.anchor_id.clone();
    }
    let warn_before = warnings_matching("Failed to verify the membership");
    let mut counts: BTreeMap<String, u64> = BTreeMap::new();
    let mut folders = Vec::new();
    for (volume, link, target) in &roots {
        let uid = NodeUid::new(volume.clone(), link.clone());
        let node = p.call("share root", p.plain.get_node_uncached(uid)).await?;
        let key = match &node {
            Ok(PotentialObject::Node(node)) => {
                if let Node::Folder(_) = node {
                    folders.push((volume.clone(), link.clone()));
                }
                match &node.base().membership {
                    None => format!("{target}: no membership"),
                    Some(m) => {
                        let accepted = match &m.accepted_by {
                            PotentialObject::Node(_) => "verified".to_owned(),
                            PotentialObject::Degraded(e) => {
                                format!("UNVERIFIED ({})", redact(&e.message))
                            }
                        };
                        format!(
                            "{target}: role {:?}, inviter {}, acceptance {accepted}",
                            m.role,
                            author_state(&m.shared_by)
                        )
                    }
                }
            }
            Ok(PotentialObject::Degraded(_)) => format!("{target}: DEGRADED root"),
            Err(error) => format!("{target}: ERR {}", describe_error(error)),
        };
        *counts.entry(key).or_default() += 1;
    }
    p.say(format!("  {} shared root(s)", roots.len()));
    for (key, count) in &counts {
        p.say(format!("  {count:3}  {key}"));
    }
    p.say(format!(
        "  membership WARN lines: {}",
        warnings_matching("Failed to verify the membership") - warn_before
    ));
    Ok(folders)
}

// ---------------------------------------------------------------------------
// P27 and P4

#[derive(Default)]
struct TreeStats {
    kinds: BTreeMap<String, u64>,
    unverified: BTreeMap<String, u64>,
    degraded: BTreeMap<String, u64>,
    anonymous: u64,
    no_claimed_size: u64,
    no_block_sizes: u64,
    pre_2025: u64,
    pre_2025_several_blocks: u64,
    listing_errors: u64,
}

impl TreeStats {
    fn add(&mut self, node: &PotentialObject<Node, DegradedNode>) {
        match node {
            PotentialObject::Node(node) => {
                *self.kinds.entry(format!("{:?}", node.ty())).or_default() += 1;
                let base = node.base();
                self.author("name_author", &base.name_author);
                self.author("author", &base.author);
                if matches!(&base.author, PotentialObject::Node(a) if a.email_address.is_none()) {
                    self.anonymous += 1;
                }
                if let Node::File(f) | Node::Photo(f) = node {
                    let revision = &f.active_revision;
                    if let Some(author) = &revision.content_author {
                        self.author("content_author", author);
                    }
                    if revision.claimed_size.is_none() {
                        self.no_claimed_size += 1;
                    }
                    if revision.claimed_block_sizes.is_none() {
                        self.no_block_sizes += 1;
                    }
                    if revision.creation_time.timestamp() < SORTED_SINCE {
                        self.pre_2025 += 1;
                        if revision
                            .claimed_block_sizes
                            .as_ref()
                            .is_some_and(|sizes| sizes.len() > 1)
                        {
                            self.pre_2025_several_blocks += 1;
                        }
                    }
                }
            }
            PotentialObject::Degraded(node) => {
                let (kind, base) = match node {
                    DegradedNode::Folder(n) => ("Folder", &n.base),
                    DegradedNode::Album(n) => ("Album", &n.base),
                    DegradedNode::File(n) => ("File", &n.base),
                    DegradedNode::Photo(n) => ("Photo", &n.base),
                };
                *self.kinds.entry(format!("degraded {kind}")).or_default() += 1;
                for error in &base.errors {
                    let text = error.to_string();
                    let head = text.split(':').next().unwrap_or_default().to_owned();
                    *self.degraded.entry(head).or_default() += 1;
                }
                if let PotentialObject::Degraded(_) = &base.name {
                    *self.degraded.entry("name".into()).or_default() += 1;
                }
                self.author("name_author", &base.name_author);
                self.author("author", &base.author);
            }
        }
    }

    fn author<E: std::fmt::Debug>(
        &mut self,
        field: &str,
        author: &PotentialObject<proton_drive_sdk::author::Author, E>,
    ) {
        if let PotentialObject::Degraded(error) = author {
            let text = redact(&format!("{error:?}"));
            *self
                .unverified
                .entry(format!(
                    "{field}: {}",
                    text.chars().take(160).collect::<String>()
                ))
                .or_default() += 1;
        }
    }

    fn report(&self, p: &Probe, tree: &str, xattr_warnings: u64) {
        p.say(format!(
            "  {tree}: nodes {:?}; listing errors {}",
            self.kinds, self.listing_errors
        ));
        p.say(format!(
            "    P27 unverified: {} ({:?}); anonymous authors {}",
            self.unverified.values().sum::<u64>(),
            self.unverified,
            self.anonymous
        ));
        p.say(format!("    degraded fields: {:?}", self.degraded));
        p.say(format!(
            "    P4: XAttr warnings {xattr_warnings}; files without a claimed size {}, without block sizes {}; \
             revisions before 2025 {} ({} with more than one block)",
            self.no_claimed_size, self.no_block_sizes, self.pre_2025, self.pre_2025_several_blocks
        ));
    }
}

fn xattr_warnings() -> u64 {
    warnings_matching("revision extended attributes")
}

async fn walk(
    p: &Probe,
    client: &ProtonDriveClient,
    volume: &VolumeId,
    start: LinkId,
    stats: &mut TreeStats,
) -> anyhow::Result<()> {
    let mut queue = VecDeque::from([start]);
    let mut listed = 0u64;
    while let Some(folder) = queue.pop_front() {
        listed += 1;
        if listed.is_multiple_of(50) {
            p.say(format!(
                "    {listed} folders listed, {} queued; nodes {:?}",
                queue.len(),
                stats.kinds
            ));
        }
        let children = p
            .call(
                "list_children",
                client.list_children(volume.clone(), Some(folder)),
            )
            .await?;
        let children = match children {
            Ok(children) => children,
            Err(error) => {
                stats.listing_errors += 1;
                p.say(format!("    listing error: {}", describe_error(&error)));
                continue;
            }
        };
        for child in &children {
            stats.add(child);
            if let PotentialObject::Node(Node::Folder(f)) = child {
                queue.push_back(f.base.uid.link_id.clone());
            }
        }
    }
    Ok(())
}

async fn p27(p: &Probe, my_files: &NodeUid, shares: &[(VolumeId, LinkId)]) -> anyhow::Result<()> {
    p.say("== P27 and P4: every node decoded afresh");
    let before = xattr_warnings();
    let mut stats = TreeStats::default();
    walk(
        p,
        &p.plain,
        &my_files.volume_id,
        my_files.link_id.clone(),
        &mut stats,
    )
    .await?;
    stats.report(p, "My Files", xattr_warnings() - before);

    let before = xattr_warnings();
    let mut stats = TreeStats::default();
    for (volume, root) in shares {
        walk(p, &p.plain, volume, root.clone(), &mut stats).await?;
    }
    stats.report(
        p,
        &format!("shares ({} folder roots, below the roots)", shares.len()),
        xattr_warnings() - before,
    );

    let before = xattr_warnings();
    let mut stats = TreeStats::default();
    let timeline = p.call("timeline", p.photos.iterate_timeline()).await?;
    match timeline {
        Ok(items) => {
            p.say(format!("  Photos timeline: {} item(s)", items.len()));
            let mut by_volume: BTreeMap<String, (VolumeId, Vec<LinkId>)> = BTreeMap::new();
            for item in items {
                by_volume
                    .entry(item.uid.volume_id.raw().to_owned())
                    .or_insert_with(|| (item.uid.volume_id.clone(), Vec::new()))
                    .1
                    .push(item.uid.link_id);
            }
            for (_, (volume, links)) in by_volume {
                for batch in links.chunks(BATCH) {
                    let nodes = p
                        .call(
                            "photos batch",
                            p.photos
                                .drive()
                                .get_nodes_uncached(volume.clone(), batch.to_vec()),
                        )
                        .await?;
                    match nodes {
                        Ok(nodes) => nodes.iter().for_each(|node| stats.add(node)),
                        Err(error) => {
                            stats.listing_errors += 1;
                            p.say(format!("    batch error: {}", describe_error(&error)));
                        }
                    }
                }
            }
        }
        Err(error) => p.say(format!("  Photos timeline: ERR {}", describe_error(&error))),
    }
    // Albums, which the timeline does not list.
    if let Ok(albums) = p.call("albums", p.photos.iterate_albums()).await? {
        p.say(format!("  albums: {}", albums.len()));
        let uids: Vec<NodeUid> = albums.into_iter().map(|a| a.uid).collect();
        for uid in uids {
            if let Ok(node) = p.call("album", p.photos.get_node(uid)).await? {
                stats.add(&node);
            }
        }
    }
    stats.report(p, "Photos", xattr_warnings() - before);
    Ok(())
}
