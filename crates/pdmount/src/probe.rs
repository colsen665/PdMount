//! Q1/Q6 probe (M3.1, docs/plans/m3.md): how the Drive API enforces
//! `CurrentRevisionID`, `OriginalHash` and draft ownership, and a few related
//! facts the conflict engine relies on. It runs against the real account and
//! writes only under `My Files/pdmount-test/q1/` (D028); each run gets its own
//! folder, which is moved to the trash at the end.
//!
//! ```sh
//! PROBE_PROFILE_A=default PROBE_PROFILE_B=second PROBE_SECRETS=force-file \
//!   cargo test -p pdmount probe -- --ignored --nocapture
//! ```
//!
//! `PROBE_PROFILE_B=same` uses profile A's session with a second client UID.
//! Both profiles must belong to the same account. Results are printed; the
//! interpretation is in docs/plans/m3.md, "M3.1 outcomes".

use crate::credentials::{self, SecretsMode};
use crate::session::{self, Resumed, TokenSaver};
use anyhow::{Context, bail};
use pdmount_backend::Backend;
use pdmount_backend::proton::ProtonBackend;
use pdmount_core::ProfilePaths;
use proton_drive_sdk::client::ProtonDriveClient;
use proton_drive_sdk::error::ApiError;
use proton_drive_sdk::links::LinkId;
use proton_drive_sdk::node::draft::{NodeWithSameNameExistsError, RevisionDraftConflictError};
use proton_drive_sdk::node::revision::RevisionUid;
use proton_drive_sdk::node::{Node, NodeUid};
use proton_drive_sdk::revision::RevisionId;
use proton_drive_sdk::utils::PotentialObject;
use proton_drive_sdk::volume::VolumeId;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};
use std::time::{Duration, SystemTime};
use tokio::io::{AsyncRead, ReadBuf};
use tokio::sync::oneshot;

/// One client: a device as the server sees it (session plus client UID).
struct Dev {
    client: ProtonDriveClient,
    backend: Arc<ProtonBackend>,
    volume: VolumeId,
}

/// The decoded fields the probe looks at.
#[derive(Debug)]
#[allow(dead_code)] // read through Debug
struct Seen {
    name: String,
    parent: Option<String>,
    active: Option<String>,
    trashed: bool,
    name_hash: String,
}

/// An upload whose source blocks on its first read, after the SDK created
/// the draft and before it uploads blocks and commits.
struct Held {
    task: tokio::task::JoinHandle<anyhow::Result<(String, String)>>,
    release: oneshot::Sender<()>,
}

impl Held {
    /// Lets the upload continue and waits for it (at most 2 minutes).
    async fn finish(self) -> anyhow::Result<(String, String)> {
        let _ = self.release.send(());
        let abort = self.task.abort_handle();
        match tokio::time::timeout(Duration::from_secs(120), self.task).await {
            Ok(result) => result?,
            Err(_) => {
                abort.abort();
                bail!("the upload did not finish within 2 minutes")
            }
        }
    }
}

struct GatedReader {
    inner: std::io::Cursor<Vec<u8>>,
    opened: Option<oneshot::Sender<()>>,
    release: Option<oneshot::Receiver<()>>,
}

impl AsyncRead for GatedReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if let Some(opened) = self.opened.take() {
            let _ = opened.send(());
        }
        if let Some(release) = self.release.as_mut() {
            match Pin::new(release).poll(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(_) => self.release = None,
            }
        }
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

fn describe<T: std::fmt::Debug>(result: &anyhow::Result<T>) -> String {
    match result {
        Ok(value) => format!("ok {value:?}"),
        Err(error) => {
            let typed = if let Some(c) = error.downcast_ref::<RevisionDraftConflictError>() {
                format!(" typed=RevisionDraftConflict{:?}", c.conflict)
            } else if let Some(c) = error.downcast_ref::<NodeWithSameNameExistsError>() {
                format!(" typed=NodeWithSameNameExists{:?}", c.conflict)
            } else {
                String::new()
            };
            match ApiError::find(error) {
                Some(api) => format!(
                    "ERR http={:?} code={:?} message={:?} details={}{typed}",
                    api.http_status,
                    api.code,
                    api.message,
                    api.details.as_ref().map_or("-".into(), |d| d.to_string())
                ),
                None => format!("ERR {error:#}{typed}"),
            }
        }
    }
}

fn hit_rate_limit<T>(result: &anyhow::Result<T>) -> bool {
    result
        .as_ref()
        .err()
        .and_then(ApiError::find)
        .is_some_and(|api| api.http_status == Some(429) || api.code == Some(2028))
}

struct Report {
    lines: Vec<String>,
}

impl Report {
    fn note(&mut self, line: String) {
        println!("{line}");
        self.lines.push(line);
    }

    /// Records an outcome; stops the probe at the first rate limit.
    fn outcome<T: std::fmt::Debug>(
        &mut self,
        label: &str,
        result: &anyhow::Result<T>,
    ) -> anyhow::Result<()> {
        self.note(format!("{label}: {}", describe(result)));
        if hit_rate_limit(result) {
            bail!("rate limited; stopping (AGENTS.md)");
        }
        Ok(())
    }
}

impl Dev {
    fn uid(&self, link: &str) -> NodeUid {
        NodeUid::from_parts(self.volume.raw(), link)
    }

    fn revision(&self, link: &str, revision: &str) -> RevisionUid {
        RevisionUid::new(self.uid(link), RevisionId::new(revision.to_owned()))
    }

    async fn mkdir(&self, parent: &str, name: &str) -> anyhow::Result<String> {
        let folder = self
            .client
            .create_folder(self.uid(parent), name.to_owned(), Some(SystemTime::now()))
            .await?;
        Ok(folder.base.uid.link_id.raw().to_owned())
    }

    /// Starts a new file; returns (link, revision).
    async fn put_new(
        &self,
        parent: &str,
        name: &str,
        data: &[u8],
    ) -> anyhow::Result<(String, String)> {
        let held = self.hold_new(parent, name, data).await?;
        held.finish().await
    }

    async fn hold_new(&self, parent: &str, name: &str, data: &[u8]) -> anyhow::Result<Held> {
        let uploader = self
            .client
            .get_file_uploader(
                self.uid(parent),
                name.to_owned(),
                "text/plain".into(),
                data.len() as i64,
                Some(SystemTime::now()),
                None,
                None,
                false,
            )
            .await?;
        Ok(Self::hold(uploader, data).await)
    }

    async fn put_revision(&self, link: &str, base: &str, data: &[u8]) -> anyhow::Result<String> {
        let held = self.hold_revision(link, base, data).await?;
        Ok(held.finish().await?.1)
    }

    async fn hold_revision(&self, link: &str, base: &str, data: &[u8]) -> anyhow::Result<Held> {
        let uploader = self
            .client
            .get_file_revision_uploader(
                self.revision(link, base),
                data.len() as i64,
                Some(SystemTime::now()),
                None,
                None,
            )
            .await?;
        Ok(Self::hold(uploader, data).await)
    }

    /// Runs the upload until its source is first read (the draft exists).
    /// If the upload fails before that, `task` holds the error.
    async fn hold(
        uploader: proton_drive_sdk::node::file::upload::FileUploader,
        data: &[u8],
    ) -> Held {
        let (opened_tx, opened_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let reader = GatedReader {
            inner: std::io::Cursor::new(data.to_vec()),
            opened: Some(opened_tx),
            release: Some(release_rx),
        };
        let task = tokio::spawn(async move {
            let result = uploader
                .upload_from_stream_with_revision(Box::new(reader), Vec::new(), Box::new(|_, _| {}))
                .await?;
            Ok((
                result.node_uid.link_id.raw().to_owned(),
                result.revision_uid.revision_id.raw().to_owned(),
            ))
        });
        let _ = tokio::time::timeout(Duration::from_secs(60), opened_rx).await;
        Held {
            task,
            release: release_tx,
        }
    }

    async fn seen(&self, link: &str) -> anyhow::Result<Option<Seen>> {
        let nodes = self
            .client
            .get_nodes_uncached_with_name_hashes(
                self.volume.clone(),
                vec![LinkId::new(link.to_owned())],
            )
            .await?;
        Ok(nodes.into_iter().find_map(|(node, hash)| match node {
            PotentialObject::Node(node) => Some(seen(node, &hash)),
            PotentialObject::Degraded(_) => None,
        }))
    }

    async fn hash(&self, link: &str) -> anyhow::Result<Vec<u8>> {
        let nodes = self
            .client
            .get_nodes_uncached_with_name_hashes(
                self.volume.clone(),
                vec![LinkId::new(link.to_owned())],
            )
            .await?;
        nodes
            .into_iter()
            .next()
            .map(|(_, hash)| hash)
            .context("node not returned")
    }

    async fn revisions(&self, link: &str) -> anyhow::Result<Vec<(String, String, Option<i64>)>> {
        Ok(self
            .client
            .iterate_revisions(self.uid(link))
            .await?
            .into_iter()
            .map(|r| {
                (
                    r.uid.revision_id.raw().to_owned(),
                    format!("{:?}", r.state),
                    r.claimed_size,
                )
            })
            .collect())
    }

    async fn trash(&self, link: &str) -> anyhow::Result<()> {
        let results = self.client.trash_nodes(vec![self.uid(link)]).await?;
        for (_, result) in results {
            result?;
        }
        Ok(())
    }

    /// (name, link) of the parent's children that are not trashed.
    async fn children(&self, parent: &str) -> anyhow::Result<Vec<(String, String)>> {
        let children = self
            .client
            .list_children(self.volume.clone(), Some(LinkId::new(parent.to_owned())))
            .await?;
        Ok(children
            .into_iter()
            .filter_map(|child| match child {
                PotentialObject::Node(node) => {
                    let link = node.uid().link_id.raw().to_owned();
                    let s = seen(node, &[]);
                    (!s.trashed).then_some((s.name, link))
                }
                PotentialObject::Degraded(_) => None,
            })
            .collect())
    }

    async fn child(&self, parent: &str, name: &str) -> anyhow::Result<Option<String>> {
        Ok(self
            .children(parent)
            .await?
            .into_iter()
            .find_map(|(n, link)| (n == name).then_some(link)))
    }
}

fn seen(node: Node, hash: &[u8]) -> Seen {
    let hex = hex::encode(hash);
    match node {
        Node::Folder(f) | Node::Album(f) => Seen {
            name: f.base.name,
            parent: f.base.parent_uid.map(|p| p.link_id.raw().to_owned()),
            active: None,
            trashed: f.base.trash_time.is_some(),
            name_hash: hex,
        },
        Node::File(f) | Node::Photo(f) => Seen {
            name: f.base.base.name,
            parent: f.base.base.parent_uid.map(|p| p.link_id.raw().to_owned()),
            active: Some(f.active_revision.uid.revision_id.raw().to_owned()),
            trashed: f.base.base.trash_time.is_some(),
            name_hash: hex,
        },
    }
}

/// Whether to run scenario `id`: all of them unless `PROBE_ONLY` lists some
/// (`PROBE_ONLY=S9,S10`).
fn want(id: &str) -> bool {
    std::env::var("PROBE_ONLY").map_or(true, |only| only.split(',').any(|s| s.trim() == id))
}

async fn signed_in(profile: &str) -> anyhow::Result<(ProfilePaths, Box<session::LoggedIn>)> {
    let paths = ProfilePaths::for_current_user(profile.parse()?)?;
    match session::resume(&paths).await? {
        Resumed::Active(logged_in) => Ok((paths, logged_in)),
        Resumed::NotLoggedIn => bail!("profile {profile} is not logged in"),
        Resumed::NeedsLogin { error, .. } => {
            Err(error.context(format!("profile {profile} needs login")))
        }
        Resumed::Offline { error, .. } => {
            Err(error.context(format!("profile {profile} is offline")))
        }
    }
}

async fn dev(
    label: &'static str,
    logged_in: &session::LoggedIn,
    client_uid: String,
) -> anyhow::Result<Dev> {
    let limits = session::TransferLimits::of(&pdmount_core::Config::default());
    let client = limits.apply(ProtonDriveClient::new(
        &logged_in.session,
        Some(client_uid),
    )?);
    let backend = ProtonBackend::new(
        client.clone(),
        logged_in.session.token_credential.clone(),
        logged_in.session.username.clone(),
    )
    .await
    .with_context(|| format!("{label}: cannot open My Files"))?;
    let root = client.get_my_files_folder().await?;
    Ok(Dev {
        client,
        backend: Arc::new(backend),
        volume: root.base.uid.volume_id,
    })
}

#[test]
#[ignore = "real account (M3.1); run by hand"]
fn q1_probe() -> anyhow::Result<()> {
    // The SDK needs the daemon's worker stack size (main.rs).
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(crate::WORKER_STACK_SIZE)
        .build()?
        .block_on(async { tokio::spawn(probe()).await? })
}

async fn probe() -> anyhow::Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(
            "warn,pgp=error,proton_drive_sdk::node::draft=info",
        ))
        .with_test_writer()
        .try_init();
    credentials::set_mode(match std::env::var("PROBE_SECRETS").as_deref() {
        Ok("force-file") => SecretsMode::ForceFile,
        Ok("allow-file") => SecretsMode::AllowFile,
        _ => SecretsMode::Keyring,
    });
    let profile_a = std::env::var("PROBE_PROFILE_A").unwrap_or_else(|_| "default".into());
    let profile_b = std::env::var("PROBE_PROFILE_B").unwrap_or_else(|_| "second".into());

    let (paths_a, session_a) = signed_in(&profile_a).await?;
    let mut saver_a = TokenSaver::new(&session_a.session).await;
    let a = dev("A", &session_a, "pdmount-q1probe-a".into()).await?;
    // A after a restart: same client UID, its own client (and transfer slot).
    let a2 = dev("A2", &session_a, "pdmount-q1probe-a".into()).await?;
    let second = if profile_b == "same" {
        None
    } else {
        Some(signed_in(&profile_b).await?)
    };
    let (b, mut saver_b) = match &second {
        None => (
            dev("B", &session_a, "pdmount-q1probe-b".into()).await?,
            None,
        ),
        Some((_, session_b)) => {
            if session_b.session.username != session_a.session.username {
                bail!("profiles {profile_a} and {profile_b} belong to different accounts");
            }
            (
                dev("B", session_b, "pdmount-q1probe-b".into()).await?,
                Some(TokenSaver::new(&session_b.session).await),
            )
        }
    };

    let mut report = Report { lines: Vec::new() };
    report.note(format!(
        "probe: A = profile {profile_a}, B = {}",
        if second.is_some() {
            format!("profile {profile_b}")
        } else {
            "profile A's session, another client UID".into()
        }
    ));
    let result = run(&a, &a2, &b, &mut report).await;
    if let Err(error) = &result {
        report.note(format!("probe stopped: {error:#}"));
    }

    saver_a
        .save(&paths_a, TokenSaver::snapshot(&session_a.session).await?)
        .await?;
    if let (Some(saver), Some((paths_b, session_b))) = (saver_b.as_mut(), &second) {
        saver
            .save(paths_b, TokenSaver::snapshot(&session_b.session).await?)
            .await?;
    }
    if let Ok(path) = std::env::var("PROBE_REPORT") {
        std::fs::write(path, report.lines.join("\n") + "\n")?;
    }
    result
}

async fn run(a: &Dev, a2: &Dev, b: &Dev, report: &mut Report) -> anyhow::Result<()> {
    let root = a.backend.account().await?.root;
    let test_root = a
        .child(root.as_str(), "pdmount-test")
        .await?
        .context("My Files/pdmount-test does not exist")?;
    let q1 = match a.child(&test_root, "q1").await? {
        Some(link) => link,
        None => a.mkdir(&test_root, "q1").await?,
    };
    let stamp = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)?
        .as_secs();
    for (name, link) in a.children(&q1).await? {
        if name.starts_with("run-") {
            report.outcome(
                &format!("cleanup: leftover {name} to the trash"),
                &a.trash(&link).await,
            )?;
        }
    }
    let run = a.mkdir(&q1, &format!("run-{stamp}")).await?;
    report.note(format!("run folder: pdmount-test/q1/run-{stamp}"));

    let result = scenarios(a, a2, b, &run, report).await;
    let trashed = a.trash(&run).await;
    report.outcome("cleanup: run folder to the trash", &trashed)?;
    result
}

async fn scenarios(
    a: &Dev,
    a2: &Dev,
    b: &Dev,
    run: &str,
    report: &mut Report,
) -> anyhow::Result<()> {
    // S1 (Q1c, Q1a): B creates a revision while A's draft on the same base is open.
    if want("S1") {
        let (f, r1) = a.put_new(run, "s1.txt", b"v1").await?;
        let held = a.hold_revision(&f, &r1, b"A2-from-a").await?;
        let other = b.put_revision(&f, &r1, b"B2-from-b").await;
        report.outcome("S1 B revision on r1 while A's draft on r1 is open", &other)?;
        let mine = held.finish().await;
        report.outcome("S1 A commit after release", &mine)?;
        report.outcome("S1 revisions (oldest first?)", &a.revisions(&f).await)?;
        report.outcome("S1 node", &a.seen(&f).await)?;
    }

    // S2 (Q1a without a second draft): B restores r1 while A's draft on r2 is open.
    if want("S2") {
        let (f, r1) = a.put_new(run, "s2.txt", b"v1").await?;
        let r2 = a.put_revision(&f, &r1, b"v2").await?;
        let held = a.hold_revision(&f, &r2, b"A3-from-a").await?;
        let restored = b.client.restore_revision(b.revision(&f, &r1)).await;
        report.outcome("S2 B restores r1 while A's draft on r2 is open", &restored)?;
        // The API answers 1002 (accepted) and applies the restore later.
        let mut landed = None;
        for _ in 0..15 {
            let active = a.seen(&f).await?.and_then(|s| s.active);
            if active.as_deref() != Some(r2.as_str()) {
                landed = active;
                break;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        report.note(format!(
            "S2 active revision within 30 s of the restore, A's draft still open: {landed:?} (r1 = {r1}, r2 = {r2})"
        ));
        let mine = held.finish().await;
        report.outcome("S2 A commit (based on r2) after release", &mine)?;
        report.outcome("S2 revisions", &a.revisions(&f).await)?;
        report.outcome("S2 node", &a.seen(&f).await)?;
    }

    // S3: the same client starts a second revision while its first draft is open.
    if want("S3") {
        let (f, r1) = a.put_new(run, "s3.txt", b"v1").await?;
        let held = a.hold_revision(&f, &r1, b"first").await?;
        let second = a2.put_revision(&f, &r1, b"second").await;
        report.outcome(
            "S3 A second revision on r1 while its own draft is open",
            &second,
        )?;
        report.outcome("S3 A first upload after release", &held.finish().await)?;
        report.outcome("S3 revisions", &a.revisions(&f).await)?;
    }

    // S4 (Q1c): A crashes with a revision draft open; then B, then A again.
    if want("S4") {
        let (f, r1) = a.put_new(run, "s4.txt", b"v1").await?;
        let held = a.hold_revision(&f, &r1, b"crashed").await?;
        held.task.abort();
        let _ = held.task.await;
        let other = b.put_revision(&f, &r1, b"B-after-crash").await;
        report.outcome("S4 B revision while A's crashed draft remains", &other)?;
        let base = a
            .seen(&f)
            .await?
            .and_then(|s| s.active)
            .unwrap_or(r1.clone());
        let again = a2.put_revision(&f, &base, b"A-after-restart").await;
        report.outcome("S4 A (same client UID) revision after the crash", &again)?;
        report.outcome("S4 revisions", &a.revisions(&f).await)?;
    }

    // S5 (Q1c): A crashes with a new-file draft open; then B, then A again.
    if want("S5") {
        let held = a.hold_new(run, "s5.txt", b"crashed").await?;
        held.task.abort();
        let _ = held.task.await;
        let other = b.put_new(run, "s5.txt", b"B").await;
        report.outcome(
            "S5 B creates the name of A's crashed new-file draft",
            &other,
        )?;
        let again = a2.put_new(run, "s5.txt", b"A").await;
        report.outcome("S5 A (same client UID) creates the name again", &again)?;
    }

    // S6 (Q1b, Q6): rename with a stale OriginalHash.
    if want("S6") {
        let (f, _) = a.put_new(run, "s6.txt", b"v1").await?;
        let stale = a.hash(&f).await?;
        let theirs = b
            .client
            .rename_node(b.uid(&f), "s6-b.txt".into(), None)
            .await;
        report.outcome("S6 B renames s6.txt -> s6-b.txt", &theirs)?;
        let mine = a
            .client
            .rename_node_with_original_hash(a.uid(&f), "s6-a.txt".into(), None, stale)
            .await;
        report.outcome("S6 A renames with the stale hash of s6.txt", &mine)?;
        report.outcome("S6 node", &a.seen(&f).await)?;
        let fresh = a.hash(&f).await?;
        let control = a
            .client
            .rename_node_with_original_hash(a.uid(&f), "s6-c.txt".into(), None, fresh)
            .await;
        report.outcome("S6 control: rename with the fresh hash", &control)?;
        report.outcome("S6 node after control", &a.seen(&f).await)?;
    }

    // S7 (Q1b, Q6): move with a stale OriginalHash.
    if want("S7") {
        let d1 = a.mkdir(run, "s7-d1").await?;
        let d2 = a.mkdir(run, "s7-d2").await?;
        let (f, _) = a.put_new(&d1, "s7.txt", b"v1").await?;
        let stale = a.hash(&f).await?;
        let theirs = b
            .client
            .rename_node(b.uid(&f), "s7-b.txt".into(), None)
            .await;
        report.outcome("S7 B renames s7.txt -> s7-b.txt", &theirs)?;
        let mine = a
            .client
            .move_node_with_original_hash(a.uid(&f), a.uid(&d2), None, stale.clone())
            .await;
        report.outcome("S7 A moves to d2 with the stale hash", &mine)?;
        let renamed = a
            .client
            .move_node_with_original_hash(a.uid(&f), a.uid(&d2), Some("s7-a.txt".into()), stale)
            .await;
        report.outcome("S7 A moves and renames with the stale hash", &renamed)?;
        report.outcome("S7 node", &a.seen(&f).await)?;
        let fresh = a.hash(&f).await?;
        let control = a
            .client
            .move_node_with_original_hash(a.uid(&f), a.uid(&d2), None, fresh)
            .await;
        report.outcome("S7 control: move with the fresh hash", &control)?;
        report.outcome("S7 node after control", &a.seen(&f).await)?;
    }

    // S8 (Q6): move a folder into its own child.
    if want("S8") {
        let p = a.mkdir(run, "s8-p").await?;
        let c = a.mkdir(&p, "c").await?;
        let moved = a.client.move_node(a.uid(&p), a.uid(&c), None).await;
        report.outcome("S8 move p into p/c", &moved)?;
    }

    // S9: children of a trashed folder.
    if want("S9") {
        let t = a.mkdir(run, "s9-t").await?;
        let (x, _) = a.put_new(&t, "x.txt", b"x").await?;
        report.outcome("S9 trash folder t", &a.trash(&t).await)?;
        report.outcome("S9 get_nodes(t) at once", &a.seen(&t).await)?;
        report.outcome("S9 get_nodes(t/x) at once", &a.seen(&x).await)?;
        let started = std::time::Instant::now();
        let mut state = None;
        while started.elapsed() < Duration::from_secs(60) {
            let now = a.seen(&t).await?;
            if now.as_ref().is_none_or(|s| s.trashed) {
                state = Some(now);
                break;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        report.note(format!(
            "S9 t trashed or gone after {:?}: {state:?}",
            started.elapsed()
        ));
        report.outcome("S9 get_nodes(t/x) then", &a.seen(&x).await)?;
        report.outcome("S9 list_children(t) then", &a.children(&t).await)?;
        let (y, _) = a.put_new(run, "s9-file.txt", b"y").await?;
        report.outcome("S9 trash a file", &a.trash(&y).await)?;
        report.outcome("S9 get_nodes(file) at once", &a.seen(&y).await)?;
    }

    // S10: download a revision of a trashed file.
    if want("S10") {
        let content = b"content of a file in the trash";
        let (y, ry) = a.put_new(run, "s10.txt", content).await?;
        report.outcome("S10 trash file", &a.trash(&y).await)?;
        let link = pdmount_backend::LinkId::new(y.clone());
        let rev = pdmount_backend::RevisionId::new(ry.clone());
        let read = async {
            let blocks = a.backend.blocks(&link, &rev).await?;
            let mut data = Vec::new();
            for block in blocks {
                data.extend_from_slice(&a.backend.fetch_block(&link, &rev, block.index).await?);
            }
            anyhow::Ok(data == content)
        }
        .await;
        report.outcome("S10 download the trashed file's revision (equal?)", &read)?;
    }

    // S11 (F14): does list_children see another client's rename at once?
    if want("S11") {
        let (z, _) = a.put_new(run, "s11.txt", b"z").await?;
        let _ = a.client.get_node(a.uid(&z)).await?; // fills A's entity cache
        b.client
            .rename_node(b.uid(&z), "s11-b.txt".into(), None)
            .await?;
        let listed = a.child(run, "s11-b.txt").await.map(|link| link.is_some());
        report.outcome("S11 A list_children shows B's new name", &listed)?;
        let cached = a.client.get_node(a.uid(&z)).await.map(|node| match node {
            PotentialObject::Node(node) => seen(node, &[]).name,
            PotentialObject::Degraded(_) => "<degraded>".into(),
        });
        report.outcome("S11 A get_node (cached) name", &cached)?;
    }
    Ok(())
}
