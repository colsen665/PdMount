//! Control socket (DESIGN.md §14.3): JSON lines over a Unix socket, one
//! request and one response per connection. The socket is mode 0600 and the
//! peer's uid is checked.

use anyhow::Context;
use anyhow::bail;
use pdmount_core::ProfilePaths;
use pdmount_core::config::AccessMode;
use pdmount_core::engine::queue::{QueueEntry, QueueState, QueueTarget};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::future::Future;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

/// Pause after a failed `accept` (out of descriptors, say).
const ACCEPT_RETRY: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    Ping,
    Status,
    /// Counters, gauges and latencies; `reset` clears the latency
    /// histograms after reading them.
    Stats {
        #[serde(default)]
        reset: bool,
    },
    /// Start a web login in the daemon and answer with its link (D042).
    Login,
    /// Upload everything pending, then answer (`unmount --wait`).
    Drain {
        timeout_secs: u64,
    },
    Unmount,
    /// Access rules (DESIGN.md §11.5); paths are relative to the mount root.
    AccessSet {
        path: String,
        mode: pdmount_core::config::AccessMode,
    },
    AccessUnset {
        path: String,
    },
    AccessList {
        path: Option<String>,
    },
    AccessEffective {
        path: String,
    },
    /// Conflicts (DESIGN.md §10.8). `path` is relative to the mount root;
    /// `target` is a conflict id (all digits) or such a path.
    ConflictsList {
        path: Option<String>,
        all: bool,
    },
    ConflictsShow {
        target: String,
    },
    ConflictsResolve {
        target: String,
        choice: pdmount_core::conflicts::Resolution,
    },
    /// Pins (DESIGN.md §5.3); paths are relative to the mount root.
    PinSet {
        path: String,
    },
    PinUnset {
        path: String,
    },
    PinList,
    /// The journal (DESIGN.md §8.2, D061).
    QueueList {
        target: QueueTarget,
    },
    QueueRetry {
        target: QueueTarget,
    },
    QueuePark {
        target: QueueTarget,
    },
    /// Without `yes` it only says what it would discard.
    QueueDrop {
        target: QueueTarget,
        yes: bool,
    },
    /// Remote barrier (§5.6); `path` is relative to the mount root.
    Sync {
        path: Option<String>,
        timeout_secs: u64,
    },
    /// The block cache (DESIGN.md §14.2).
    CacheStats,
    CacheGc {
        target: Option<u64>,
    },
    /// `path` is relative to the mount root; `None`: the whole cache.
    CacheWipe {
        path: Option<String>,
    },
}

// A reply is `{"ok": true, ...}` with the fields of the request's reply type
// below, or `{"ok": false, "error": "..."}`. The daemon builds replies from
// these types and the CLI reads them back with the same types.

/// `Ping`.
#[derive(Debug, Serialize, Deserialize)]
pub struct Pong {
    pub pid: u32,
}

/// `Status`.
#[derive(Debug, Serialize, Deserialize)]
pub struct StatusReply {
    pub pid: u32,
    pub mountpoint: PathBuf,
    pub queue_pending: u64,
    pub queue_parked: u64,
    /// How long the oldest pending entry has been dirty.
    pub queue_oldest_secs: Option<u64>,
    /// How long ago the event stream was last read to its end.
    pub events_last_poll_secs: Option<u64>,
    pub events_refreshing: bool,
    /// `mount -o ro`.
    pub read_only: bool,
    /// Conflicts waiting for the user.
    pub conflicts: u64,
    pub connectivity: pdmount_core::health::Connectivity,
    /// Unix seconds when `connectivity` began.
    pub connectivity_since: u64,
    /// A login the daemon runs (D042), until it ends.
    pub sign_in_url: Option<String>,
    pub sign_in_code: Option<String>,
    /// Why the last daemon login failed.
    pub sign_in_error: Option<String>,
    pub username: String,
    /// Where the session is stored.
    pub credentials: String,
    pub quota_used: Option<u64>,
    pub quota_max: Option<u64>,
    pub inodes: u64,
    pub cache_bytes: u64,
    pub cache_blocks: u64,
    /// `None` without pins.
    pub pinned: Option<PinnedReply>,
}

/// All pins together, in `StatusReply`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct PinnedReply {
    pub paths: u64,
    pub fetched_bytes: u64,
    pub total_bytes: u64,
}

/// `Login`.
#[derive(Debug, Serialize, Deserialize)]
pub struct LoginReply {
    pub url: String,
    pub code: String,
}

/// `Drain`.
#[derive(Debug, Serialize, Deserialize)]
pub struct DrainReply {
    pub pending: u64,
    pub parked: u64,
    /// `(path, reason)` of each parked change.
    pub parked_items: Vec<(String, String)>,
}

/// `AccessSet`.
#[derive(Debug, Serialize, Deserialize)]
pub struct AccessSetReply {
    /// Pending changes in the subtree; they still upload.
    pub pending: u64,
}

/// `AccessList`.
#[derive(Debug, Serialize, Deserialize)]
pub struct AccessListReply {
    pub default: AccessMode,
    pub rules: Vec<RuleReply>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RuleReply {
    pub mode: AccessMode,
    /// `None`: orphaned, its entry is gone.
    pub path: Option<String>,
    pub set_as: String,
    pub source: String,
}

/// `AccessEffective`.
#[derive(Debug, Serialize, Deserialize)]
pub struct AccessEffectiveReply {
    pub mode: AccessMode,
    /// The deciding rule's path; `None`: the default.
    pub rule: Option<String>,
}

/// `ConflictsList`.
#[derive(Debug, Serialize, Deserialize)]
pub struct ConflictsReply {
    pub conflicts: Vec<ConflictReply>,
}

/// `ConflictsShow` and `ConflictsResolve`.
#[derive(Debug, Serialize, Deserialize)]
pub struct ConflictOneReply {
    pub conflict: ConflictReply,
}

/// A conflict as the control socket carries it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictReply {
    pub id: i64,
    pub kind: String,
    pub state: String,
    pub path: Option<String>,
    pub other_path: Option<String>,
    pub orig_name: String,
    pub conflict_name: Option<String>,
    pub local_rev: Option<String>,
    pub remote_rev: Option<String>,
    pub local_device: String,
    /// Unix seconds.
    pub created_at: i64,
    pub detail: Option<String>,
}

impl From<&pdmount_core::conflicts::ConflictView> for ConflictReply {
    fn from(view: &pdmount_core::conflicts::ConflictView) -> Self {
        Self {
            id: view.id,
            kind: view.kind.as_str().to_owned(),
            state: view.state.as_str().to_owned(),
            path: view.path.clone(),
            other_path: view.other_path.clone(),
            orig_name: view.orig_name.clone(),
            conflict_name: view.conflict_name.clone(),
            local_rev: view.local_rev.as_ref().map(|rev| rev.as_str().to_owned()),
            remote_rev: view.remote_rev.as_ref().map(|rev| rev.as_str().to_owned()),
            local_device: view.local_device.clone(),
            created_at: view.created_at,
            detail: view.detail.clone(),
        }
    }
}

/// `Stats`.
#[derive(Debug, Serialize, Deserialize)]
pub struct StatsReply {
    /// By name, as `Metrics` counts them.
    pub counters: std::collections::BTreeMap<String, u64>,
    pub pool: Vec<PoolClassReply>,
    /// Current values, by name (`Engine::gauges`).
    pub gauges: std::collections::BTreeMap<String, u64>,
    /// Operations recorded since the last reset, in `Op` order.
    pub latency: Vec<LatencyReply>,
}

/// One latency histogram, in µs (DESIGN.md §5.8).
#[derive(Debug, Serialize, Deserialize)]
pub struct LatencyReply {
    pub name: String,
    pub count: u64,
    pub p50_us: u64,
    pub p90_us: u64,
    pub p99_us: u64,
    pub max_us: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PoolClassReply {
    /// The slot size, or `scratch`.
    pub class: String,
    pub slots: usize,
    pub in_use: usize,
}

/// `PinList`.
#[derive(Debug, Serialize, Deserialize)]
pub struct PinListReply {
    pub pins: Vec<PinReply>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PinReply {
    /// `None`: not reachable right now.
    pub path: Option<String>,
    pub set_as: String,
    pub files: u64,
    pub fetched_bytes: u64,
    pub total_bytes: u64,
    /// `fetching`, `complete`, `waiting-offline` or `waiting-space`.
    pub state: String,
}

/// `QueueList`.
#[derive(Debug, Serialize, Deserialize)]
pub struct QueueListReply {
    pub entries: Vec<QueueEntryReply>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct QueueEntryReply {
    pub id: u64,
    pub path: String,
    pub kind: String,
    /// `waiting`, `running`, `backoff` or `parked`.
    pub state: String,
    /// Unix seconds when a back-off ends.
    pub until: Option<i64>,
    pub reason: Option<String>,
    pub attempts: u32,
    pub age_secs: u64,
    pub last_error: Option<String>,
    pub base_rev: Option<String>,
    pub dependencies: Vec<String>,
}

impl From<QueueEntry> for QueueEntryReply {
    fn from(entry: QueueEntry) -> Self {
        let (state, until, reason) = match entry.state {
            QueueState::Waiting => ("waiting", None, None),
            QueueState::Running => ("running", None, None),
            QueueState::Backoff { until } => {
                let until = until
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |since| since.as_secs() as i64);
                ("backoff", Some(until), None)
            }
            QueueState::Parked { reason } => ("parked", None, Some(reason)),
        };
        Self {
            id: entry.id,
            path: entry.path,
            kind: entry.kind.as_str().to_owned(),
            state: state.to_owned(),
            until,
            reason,
            attempts: entry.attempts,
            age_secs: entry.age.as_secs(),
            last_error: entry.last_error,
            base_rev: entry.base_rev,
            dependencies: entry.dependencies,
        }
    }
}

/// `QueueRetry` and `QueuePark`.
#[derive(Debug, Serialize, Deserialize)]
pub struct QueueActedReply {
    /// Id and path.
    pub items: Vec<(u64, String)>,
}

/// `QueueDrop`.
#[derive(Debug, Serialize, Deserialize)]
pub struct QueueDropReply {
    pub items: Vec<QueueDropItem>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct QueueDropItem {
    pub id: u64,
    pub path: String,
    pub effect: String,
    pub refused: Option<String>,
}

/// `Sync`.
#[derive(Debug, Serialize, Deserialize)]
pub struct SyncReply {
    pub pending: u64,
    /// Path and reason.
    pub parked: Vec<(String, String)>,
}

/// `CacheStats`.
#[derive(Debug, Serialize, Deserialize)]
pub struct CacheStatsReply {
    /// Against `max_bytes`; pinned blocks are apart.
    pub bytes: u64,
    pub max_bytes: u64,
    pub blocks: u64,
    pub pinned_bytes: u64,
    pub protected_revisions: u64,
    pub pool: Vec<PoolClassReply>,
}

/// `CacheGc` and `CacheWipe`.
#[derive(Debug, Serialize, Deserialize)]
pub struct CacheFreedReply {
    pub blocks: u64,
    pub bytes: u64,
    /// Against the cap, afterwards.
    pub left_bytes: u64,
}

/// `Unmount`, and requests that answer only whether they worked.
#[derive(Debug, Serialize, Deserialize)]
pub struct Done {}

/// A successful reply.
pub fn ok(body: impl Serialize) -> Value {
    let mut value = serde_json::to_value(body).expect("replies serialize");
    if let Value::Object(fields) = &mut value {
        fields.insert("ok".into(), Value::Bool(true));
    }
    value
}

pub fn failed(error: impl std::fmt::Display) -> Value {
    serde_json::json!({ "ok": false, "error": error.to_string() })
}

/// [`ok`] or [`failed`].
pub fn reply<T: Serialize, E: std::fmt::Display>(result: Result<T, E>) -> Value {
    result.map_or_else(failed, ok)
}

pub type Handler =
    Arc<dyn Fn(Request) -> Pin<Box<dyn Future<Output = Value> + Send>> + Send + Sync>;

/// Sends one request and reads its reply as `T`; a refused request is an
/// error with the daemon's message. `Ok(None)` means no daemon is listening.
pub async fn call_ok<T: DeserializeOwned>(
    socket: &Path,
    request: Request,
    timeout: Duration,
) -> anyhow::Result<Option<T>> {
    let Some(reply) = call(socket, request, timeout).await? else {
        return Ok(None);
    };
    if reply["ok"] != true {
        bail!("{}", reply["error"].as_str().unwrap_or("unknown error"));
    }
    Ok(Some(
        serde_json::from_value(reply).context("invalid daemon response")?,
    ))
}

/// [`call_ok`] for commands that need the daemon: `what` says what goes
/// through it ("access rules are changed").
pub async fn ask<T: DeserializeOwned>(
    paths: &ProfilePaths,
    request: Request,
    timeout: Duration,
    what: &str,
) -> anyhow::Result<T> {
    call_ok(&paths.control_socket(), request, timeout)
        .await?
        .with_context(|| {
            format!(
                "profile {} is not mounted; {what} through the running daemon",
                paths.profile
            )
        })
}

/// Sends one request. `Ok(None)` means no daemon is listening.
pub async fn call(
    socket: &Path,
    request: Request,
    timeout: Duration,
) -> anyhow::Result<Option<Value>> {
    let mut stream = match UnixStream::connect(socket).await {
        Ok(stream) => stream,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            return Ok(None);
        }
        Err(error) => {
            return Err(error).with_context(|| format!("cannot connect to {}", socket.display()));
        }
    };
    let exchange = async {
        let mut line = serde_json::to_string(&request)?;
        line.push('\n');
        stream.write_all(line.as_bytes()).await?;
        let mut reader = BufReader::new(stream);
        let mut response = String::new();
        reader.read_line(&mut response).await?;
        anyhow::Ok(serde_json::from_str::<Value>(&response).context("invalid daemon response")?)
    };
    let response = tokio::time::timeout(timeout, exchange)
        .await
        .map_err(|_| anyhow::anyhow!("the daemon did not answer within {timeout:?}"))?;
    match response {
        Ok(response) => Ok(Some(response)),
        // A daemon that closes the connection without answering is exiting.
        Err(error) if is_hangup(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

fn is_hangup(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.downcast_ref::<std::io::Error>().is_some_and(|io| {
            matches!(
                io.kind(),
                std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::UnexpectedEof
            )
        }) || cause
            .downcast_ref::<serde_json::Error>()
            .is_some_and(serde_json::Error::is_eof)
    })
}

/// Creates the socket's directory (mode 0700). `$XDG_RUNTIME_DIR` is a tmpfs,
/// so it is gone after every reboot.
pub fn create_dir(socket: &Path) -> anyhow::Result<()> {
    if let Some(dir) = socket.parent() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .with_context(|| format!("cannot create {}", dir.display()))?;
    }
    Ok(())
}

/// Binds the socket and serves requests until the task is aborted.
pub fn serve(socket: &Path, handler: Handler) -> anyhow::Result<tokio::task::JoinHandle<()>> {
    create_dir(socket)?;
    // The caller checked that no daemon answers, so a leftover file is stale.
    let _ = std::fs::remove_file(socket);
    let listener =
        UnixListener::bind(socket).with_context(|| format!("cannot bind {}", socket.display()))?;
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))?;
    let uid = nix::unistd::getuid().as_raw();
    Ok(tokio::spawn(async move {
        loop {
            let stream = match listener.accept().await {
                Ok((stream, _)) => stream,
                // EMFILE and the like persist: retrying at once would spin.
                Err(error) => {
                    tracing::warn!(%error, "control socket: accept failed");
                    tokio::time::sleep(ACCEPT_RETRY).await;
                    continue;
                }
            };
            let handler = Arc::clone(&handler);
            tokio::spawn(async move {
                if let Err(error) = serve_one(stream, uid, handler).await {
                    tracing::debug!(%error, "control request failed");
                }
            });
        }
    }))
}

async fn serve_one(stream: UnixStream, uid: u32, handler: Handler) -> anyhow::Result<()> {
    let peer = stream.peer_cred()?;
    if peer.uid() != uid {
        anyhow::bail!("refused a control connection from uid {}", peer.uid());
    }
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).await?;
    let response = match serde_json::from_str::<Request>(&line) {
        Ok(request) => handler(request).await,
        Err(error) => failed(format!("invalid request: {error}")),
    };
    let mut out = serde_json::to_string(&response)?;
    out.push('\n');
    reader.into_inner().write_all(out.as_bytes()).await?;
    Ok(())
}

/// Removes the socket file if it is ours to remove.
pub fn remove(socket: &PathBuf) {
    let _ = std::fs::remove_file(socket);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn requests_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("sub/test.sock");
        let handler: Handler = Arc::new(|request| {
            Box::pin(
                async move { serde_json::json!({ "ok": true, "echo": format!("{request:?}") }) },
            )
        });
        let server = serve(&socket, handler).expect("serve");
        let mode = std::fs::metadata(&socket)
            .expect("stat")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        let response = call(
            &socket,
            Request::Stats { reset: true },
            Duration::from_secs(5),
        )
        .await
        .expect("call")
        .expect("daemon");
        assert_eq!(response["echo"], "Stats { reset: true }");
        server.abort();
    }

    #[tokio::test]
    async fn typed_replies_round_trip_and_refusals_are_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("test.sock");
        let handler: Handler = Arc::new(|request| {
            Box::pin(async move {
                match request {
                    Request::Ping => ok(Pong { pid: 7 }),
                    _ => failed("busy"),
                }
            })
        });
        let server = serve(&socket, handler).expect("serve");
        let pong: Pong = call_ok(&socket, Request::Ping, Duration::from_secs(5))
            .await
            .expect("call")
            .expect("daemon");
        assert_eq!(pong.pid, 7);
        let refused = call_ok::<Done>(&socket, Request::Unmount, Duration::from_secs(5)).await;
        assert_eq!(refused.expect_err("refused").to_string(), "busy");
        // Another reply type is an error, not zeros.
        let wrong = call_ok::<DrainReply>(&socket, Request::Ping, Duration::from_secs(5)).await;
        assert!(wrong.is_err());
        server.abort();
    }

    #[test]
    fn replies_keep_their_wire_fields() {
        let reply = ok(AccessEffectiveReply {
            mode: AccessMode::Ro,
            rule: None,
        });
        assert_eq!(
            reply,
            serde_json::json!({ "ok": true, "mode": "ro", "rule": null })
        );
    }

    #[tokio::test]
    async fn no_daemon_is_not_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let response = call(
            &dir.path().join("none.sock"),
            Request::Ping,
            Duration::from_secs(1),
        )
        .await
        .expect("call");
        assert!(response.is_none());
    }

    #[test]
    fn requests_serialize_as_tagged_objects() {
        assert_eq!(
            serde_json::to_string(&Request::Unmount).expect("json"),
            r#"{"cmd":"unmount"}"#
        );
        assert_eq!(
            serde_json::to_string(&Request::ConflictsResolve {
                target: "3".into(),
                choice: pdmount_core::conflicts::Resolution::KeepLocal,
            })
            .expect("json"),
            r#"{"cmd":"conflicts_resolve","target":"3","choice":"keep_local"}"#
        );
        let parsed: Request =
            serde_json::from_str(r#"{"cmd":"conflicts_list","path":null,"all":true}"#)
                .expect("parse");
        assert_eq!(
            parsed,
            Request::ConflictsList {
                path: None,
                all: true
            }
        );
    }
}
