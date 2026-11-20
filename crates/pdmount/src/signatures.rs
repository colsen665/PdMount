//! Nodes whose signatures do not verify (DESIGN.md §6.3, §14.2, D132
//! amended): `pdmount signatures list|accept|check`, their side in the
//! daemon across My Files and the further trees, and the notification.

use crate::control::{
    self, Request, SignatureAcceptReply, SignatureReply, SignaturesCheckReply, SignaturesReply,
};
use pdmount_core::ProfilePaths;
use pdmount_core::config::SignatureMode;
use pdmount_core::engine::{CheckProgress, Engine};
use pdmount_core::errors::CoreError;
use pdmount_core::photos::PhotosTree;
use pdmount_core::signatures::{CheckCounts, FoundView};
use serde_json::Value;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// At most one notification per tree in this time (D132); what is found in
/// between is counted and announced together.
pub const NOTICE_GAP: Duration = Duration::from_secs(600);

/// `list` and `accept` wait for the store's commit.
const SIGNATURES_TIMEOUT: Duration = Duration::from_secs(60);
/// How often `check` asks for its progress.
const CHECK_POLL: Duration = Duration::from_secs(1);
/// How often `check` prints its progress.
const CHECK_PRINT: Duration = Duration::from_secs(10);

/// A tree that keeps found nodes.
#[derive(Clone)]
pub enum Tree {
    Engine(Arc<Engine>),
    Photos(Arc<PhotosTree>),
}

impl Tree {
    fn mode(&self) -> SignatureMode {
        match self {
            Self::Engine(engine) => engine.signature_mode(),
            Self::Photos(photos) => {
                pdmount_core::readonly::ReadOnlyTree::config(photos.as_ref()).signatures
            }
        }
    }

    async fn list(&self) -> Result<Vec<FoundView>, CoreError> {
        match self {
            Self::Engine(engine) => engine.signatures_list().await,
            Self::Photos(photos) => photos.signatures_list().await,
        }
    }

    async fn accept(&self, id: &str) -> Result<Option<FoundView>, CoreError> {
        match self {
            Self::Engine(engine) => engine.signature_accept(id).await,
            Self::Photos(photos) => photos.signature_accept(id).await,
        }
    }

    async fn check(&self, path: Option<&str>, progress: &CheckProgress) -> Result<(), CoreError> {
        match self {
            Self::Engine(engine) => engine.signatures_check(path, progress).await.map(drop),
            Self::Photos(photos) => photos.signatures_check(progress).await.map(drop),
        }
    }

    fn found(&self) -> u64 {
        match self {
            Self::Engine(engine) => engine.signature_counts().0,
            Self::Photos(photos) => photos.signature_counts().0,
        }
    }
}

/// Every tree of the daemon, My Files first, each with the name `list`
/// gives its root.
fn trees(engine: &Arc<Engine>, roots: Option<&crate::roots::Roots>) -> Vec<(String, Tree)> {
    let mut trees = vec![("My Files".to_owned(), Tree::Engine(Arc::clone(engine)))];
    if let Some(roots) = roots {
        trees.extend(roots.signature_trees());
    }
    trees
}

async fn found_nodes(trees: &[(String, Tree)]) -> Result<Vec<SignatureReply>, CoreError> {
    let mut nodes = Vec::new();
    for (root, tree) in trees {
        let mode = tree.mode().as_str().to_owned();
        nodes.extend(tree.list().await?.into_iter().map(|node| SignatureReply {
            root: root.clone(),
            mode: mode.clone(),
            node,
        }));
    }
    Ok(nodes)
}

/// The daemon's answer to `SignaturesList`.
pub async fn list_reply(engine: &Arc<Engine>, roots: Option<&crate::roots::Roots>) -> Value {
    control::reply(
        found_nodes(&trees(engine, roots))
            .await
            .map(|nodes| SignaturesReply { nodes }),
    )
}

/// The daemon's answer to `SignaturesAccept`: the one node of all trees
/// with the id.
pub async fn accept_reply(
    engine: &Arc<Engine>,
    roots: Option<&crate::roots::Roots>,
    id: &str,
) -> Value {
    let trees = trees(engine, roots);
    let nodes = match found_nodes(&trees).await {
        Ok(nodes) => nodes,
        Err(error) => return control::failed(error),
    };
    let named: Vec<&SignatureReply> = nodes.iter().filter(|node| node.node.id == id).collect();
    let root = match named.as_slice() {
        [] => {
            return control::failed(format!(
                "no node with unverified signatures has the id {id}; see `pdmount signatures list`"
            ));
        }
        [node] => node.root.clone(),
        _ => {
            return control::failed(format!(
                "the id {id} names {} nodes; nothing was accepted",
                named.len()
            ));
        }
    };
    let Some((_, tree)) = trees.iter().find(|(name, _)| *name == root) else {
        return control::failed("the node's tree is gone");
    };
    match tree.accept(id).await {
        Ok(Some(node)) => control::ok(SignatureAcceptReply {
            node: SignatureReply {
                root,
                mode: tree.mode().as_str().to_owned(),
                node,
            },
        }),
        Ok(None) => control::failed(format!("the node {id} verifies now or is gone")),
        Err(error) => control::failed(error),
    }
}

/// The running or last `signatures check` of the daemon.
#[derive(Default)]
pub struct Checks {
    state: parking_lot::Mutex<CheckState>,
}

#[derive(Default)]
struct CheckState {
    running: bool,
    trees: u64,
    trees_done: u64,
    progress: Arc<CheckProgress>,
    found: u64,
    error: Option<String>,
}

impl Checks {
    fn reply(&self, started: bool) -> SignaturesCheckReply {
        let state = self.state.lock();
        SignaturesCheckReply {
            running: state.running,
            started,
            trees_done: state.trees_done,
            trees: state.trees,
            counts: state.progress.counts(),
            found: state.found,
            error: state.error.clone(),
        }
    }
}

/// The daemon's answer to `SignaturesCheck`: with `start`, a check of
/// `path` in My Files, or of every tree, begins unless one runs; the
/// reply is its state either way.
pub fn check_reply(
    engine: &Arc<Engine>,
    roots: Option<&crate::roots::Roots>,
    checks: &Arc<Checks>,
    path: Option<String>,
    start: bool,
) -> Value {
    let started = start && {
        let mut state = checks.state.lock();
        if state.running {
            false
        } else {
            let trees = match &path {
                Some(_) => vec![("My Files".to_owned(), Tree::Engine(Arc::clone(engine)))],
                None => trees(engine, roots),
            };
            *state = CheckState {
                running: true,
                trees: trees.len() as u64,
                ..CheckState::default()
            };
            let progress = Arc::clone(&state.progress);
            let checks = Arc::clone(checks);
            tokio::spawn(async move {
                let mut error = None;
                for (root, tree) in &trees {
                    if let Err(failed) = tree.check(path.as_deref(), &progress).await {
                        tracing::warn!(root, error = %failed, "signatures check of a tree failed");
                        error = Some(format!("{root}: {failed}"));
                    }
                    checks.state.lock().trees_done += 1;
                }
                let found = trees.iter().map(|(_, tree)| tree.found()).sum();
                let mut state = checks.state.lock();
                state.running = false;
                state.found = found;
                state.error = error;
            });
            true
        }
    };
    control::ok(checks.reply(started))
}

async fn ask<T: serde::de::DeserializeOwned>(
    paths: &ProfilePaths,
    request: Request,
) -> anyhow::Result<T> {
    control::ask(
        paths,
        request,
        SIGNATURES_TIMEOUT,
        "signatures are listed and checked",
    )
    .await
}

/// `pdmount signatures list [--json]`.
pub async fn list(paths: &ProfilePaths, json: bool) -> anyhow::Result<ExitCode> {
    let reply: SignaturesReply = ask(paths, Request::SignaturesList).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&reply.nodes)?);
    } else {
        println!("{}", format_list(&reply.nodes));
    }
    Ok(ExitCode::SUCCESS)
}

/// `pdmount signatures accept <id>`.
pub async fn accept(paths: &ProfilePaths, id: String) -> anyhow::Result<ExitCode> {
    let reply: SignatureAcceptReply = ask(paths, Request::SignaturesAccept { id }).await?;
    let node = &reply.node;
    println!(
        "accepted {} in {}: {}",
        node.node.id,
        crate::status::printable(&node.root),
        shown_path(&node.node)
    );
    println!(
        "It is handled as verified on this device while its signatures stay as they are; a change asks again."
    );
    Ok(ExitCode::SUCCESS)
}

/// `pdmount signatures check [<path>]`: starts the check in the daemon and
/// follows it, printing its progress now and then and a summary.
pub async fn check(paths: &ProfilePaths, path: Option<String>) -> anyhow::Result<ExitCode> {
    let first: SignaturesCheckReply = ask(
        paths,
        Request::SignaturesCheck {
            path: path.clone(),
            start: true,
        },
    )
    .await?;
    if first.started {
        eprintln!(
            "Checking {}: every folder is listed from the server, one at a time as a refresh does.",
            path.as_deref().map_or_else(
                || "every tree".to_owned(),
                |path| format!("{path} in My Files")
            )
        );
    } else {
        eprintln!("A check runs already; following it.");
    }
    let mut reply = first;
    let mut printed = Instant::now();
    while reply.running {
        tokio::time::sleep(CHECK_POLL).await;
        reply = ask(
            paths,
            Request::SignaturesCheck {
                path: None,
                start: false,
            },
        )
        .await?;
        if reply.running && printed.elapsed() >= CHECK_PRINT {
            eprintln!("  {}", progress_line(&reply));
            printed = Instant::now();
        }
    }
    println!("{}", summary(&reply));
    if let Some(error) = &reply.error {
        eprintln!(
            "The check did not finish: {}",
            crate::status::printable(error)
        );
        return Ok(ExitCode::FAILURE);
    }
    Ok(if reply.counts.failed > 0 {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

fn progress_line(reply: &SignaturesCheckReply) -> String {
    let counts = &reply.counts;
    format!(
        "{} folders and {} nodes checked, {} unverified (tree {} of {})",
        counts.folders,
        counts.nodes,
        counts.unverified,
        (reply.trees_done + 1).min(reply.trees),
        reply.trees
    )
}

fn summary(reply: &SignaturesCheckReply) -> String {
    let CheckCounts {
        folders,
        nodes,
        unverified,
        failed,
    } = reply.counts;
    let trees = match reply.trees_done {
        1 => "1 tree".to_owned(),
        n => format!("{n} trees"),
    };
    let mut out = format!(
        "Checked {folders} folders and {nodes} nodes in {trees}: {unverified} with unverified signatures."
    );
    if failed > 0 {
        out.push_str(&format!(
            "\n{failed} folders could not be listed; run the check again later."
        ));
    }
    if reply.found > 0 {
        out.push_str(&format!(
            "\n{} nodes are listed by `pdmount signatures list`.",
            reply.found
        ));
    }
    out
}

/// A found node's path, its name marked when the name's own signature
/// failed; control characters made visible (D070).
fn shown_path(node: &FoundView) -> String {
    let path = node
        .path
        .as_deref()
        .map_or_else(|| "(not placed)".to_owned(), crate::status::printable);
    if node.parts.iter().any(|part| part == "name") {
        format!("{path} [name unverified]")
    } else {
        path
    }
}

/// `2026-10-22` for unix seconds.
fn day(seconds: i64) -> String {
    chrono::DateTime::from_timestamp(seconds, 0).map_or_else(
        || "?".to_owned(),
        |time| time.format("%Y-%m-%d").to_string(),
    )
}

fn format_list(nodes: &[SignatureReply]) -> String {
    if nodes.is_empty() {
        return "no nodes with unverified signatures".to_owned();
    }
    let mut out = String::new();
    for node in nodes {
        let state = if node.node.accepted {
            "accepted"
        } else if node.node.refused {
            "refused"
        } else {
            "warned"
        };
        out.push_str(&format!(
            "{}  {state:<8}  {}\n  root: {}\n  parts: {}\n  claimed signer: {}\n  first seen: {}\n",
            node.node.id,
            shown_path(&node.node),
            crate::status::printable(&node.root),
            node.node.parts.join(", "),
            crate::status::printable(&node.node.signer),
            day(node.node.first_seen),
        ));
    }
    out.push_str(
        "\nA refused node opens after `pdmount signatures accept <id>`, on this device, while its signatures stay as they are.",
    );
    out
}

/// The notification for `count` newly found nodes: the count and the
/// command, never names (they are unverified remote text, D132). The
/// summary is plain text; the body may be read as markup.
pub fn notification(tree: Option<&str>, count: u32) -> (String, String) {
    let place = tree.map_or_else(String::new, |name| {
        format!(" in {}", crate::notify::clean_name(name))
    });
    let summary = format!("PdMount: unverified signatures{place}");
    let body = if count == 1 {
        "1 newly found file or folder whose signatures do not verify; see `pdmount signatures list`"
            .to_owned()
    } else {
        format!(
            "{count} newly found files or folders whose signatures do not verify; see `pdmount signatures list`"
        )
    };
    (summary, body)
}

/// Announces a tree's newly found nodes, at most every [`NOTICE_GAP`];
/// until the tree is gone.
pub async fn announce(tree: Option<String>, mut notices: tokio::sync::broadcast::Receiver<u64>) {
    use tokio::sync::broadcast::error::RecvError;
    let mut desktop = crate::notify::Coalescer::with_gap(NOTICE_GAP);
    loop {
        let due = desktop.deadline().map(tokio::time::Instant::from_std);
        let held = async {
            match due {
                Some(due) => tokio::time::sleep_until(due).await,
                None => std::future::pending().await,
            }
        };
        let count = tokio::select! {
            () = held => desktop.flush(Instant::now()),
            notice = notices.recv() => match notice {
                Ok(count) => desktop.add_n(Instant::now(), u32::try_from(count).unwrap_or(u32::MAX)),
                Err(RecvError::Lagged(missed)) => {
                    desktop.add_n(Instant::now(), u32::try_from(missed).unwrap_or(u32::MAX))
                }
                Err(RecvError::Closed) => return,
            },
        };
        if let Some(count) = count {
            let (summary, body) = notification(tree.as_deref(), count);
            crate::notify::desktop(&summary, &body);
        }
    }
}

/// The `status` line of a tree's found nodes; `None` without any.
pub fn status_line(found: u64, refused: u64) -> Option<String> {
    (found > 0).then(|| {
        let refused = if refused > 0 {
            format!(", {refused} refused")
        } else {
            String::new()
        };
        format!("{found} unverified{refused} (pdmount signatures list)")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_notification_gives_the_count_and_the_command_never_names() {
        let (summary, body) = notification(None, 1);
        assert_eq!(summary, "PdMount: unverified signatures");
        assert_eq!(
            body,
            "1 newly found file or folder whose signatures do not verify; see `pdmount signatures list`"
        );
        let (summary, body) = notification(Some("Team\u{1b}[31m"), 3);
        assert!(!summary.contains('\u{1b}'), "{summary}");
        assert!(summary.starts_with("PdMount: unverified signatures in Team"));
        assert!(body.starts_with("3 newly found files or folders"), "{body}");
        // The share's name is cleaned by the rule of every notification
        // (`notify::clean_name`, M6.5 V-2).
        let (summary, _) = notification(Some("Q3\u{202e}txt.exe\u{2028}x"), 2);
        assert_eq!(
            summary,
            "PdMount: unverified signatures in Q3\u{fffd}txt.exe\u{fffd}x"
        );
    }

    /// At most one notification per tree every 10 minutes; what comes in
    /// between is announced together when the gap is over.
    #[test]
    fn notifications_come_at_most_every_ten_minutes() {
        let mut notices = crate::notify::Coalescer::with_gap(NOTICE_GAP);
        let start = Instant::now();
        assert_eq!(notices.add_n(start, 2), Some(2));
        assert_eq!(notices.add_n(start + Duration::from_secs(60), 1), None);
        assert_eq!(notices.add_n(start + Duration::from_secs(599), 4), None);
        assert_eq!(notices.deadline(), Some(start + NOTICE_GAP));
        assert_eq!(notices.flush(start + NOTICE_GAP), Some(5));
        assert_eq!(notices.flush(start + NOTICE_GAP * 2), None);
    }

    #[test]
    fn a_found_node_shows_its_state_and_marks_an_unverified_name() {
        let node = |parts: &[&str], accepted: bool, refused: bool| SignatureReply {
            root: "My Files".into(),
            mode: "refuse".into(),
            node: FoundView {
                id: "0a1b2c3d".into(),
                link: "l".into(),
                path: Some("/docs/a\u{7}.txt".into()),
                parts: parts.iter().map(|p| (*p).to_owned()).collect(),
                signer: "x@example.test".into(),
                first_seen: 1_792_663_200,
                accepted,
                refused,
            },
        };
        let text = format_list(&[node(&["name", "key"], false, true)]);
        assert!(
            text.starts_with("0a1b2c3d  refused   /docs/a?.txt [name unverified]\n"),
            "{text}"
        );
        assert!(text.contains("  parts: name, key\n"), "{text}");
        assert!(
            text.contains("  claimed signer: x@example.test\n"),
            "{text}"
        );
        assert!(text.contains("  first seen: 2026-10-22\n"), "{text}");
        let text = format_list(&[node(&["content key"], true, false)]);
        assert!(
            text.starts_with("0a1b2c3d  accepted  /docs/a?.txt\n"),
            "{text}"
        );
        assert_eq!(format_list(&[]), "no nodes with unverified signatures");
        assert_eq!(status_line(0, 0), None);
        assert_eq!(
            status_line(2, 1).as_deref(),
            Some("2 unverified, 1 refused (pdmount signatures list)")
        );
    }
}
