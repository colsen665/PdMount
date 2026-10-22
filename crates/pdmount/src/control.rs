//! Control socket (DESIGN.md §14.3): JSON lines over a Unix socket, one
//! request and one response per connection. The socket is mode 0600 and the
//! peer's uid is checked.

use anyhow::Context;
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    Ping,
    Status,
    Stats,
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
}

pub type Handler =
    Arc<dyn Fn(Request) -> Pin<Box<dyn Future<Output = Value> + Send>> + Send + Sync>;

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

/// Binds the socket and serves requests until the task is aborted.
pub fn serve(socket: &Path, handler: Handler) -> anyhow::Result<tokio::task::JoinHandle<()>> {
    if let Some(dir) = socket.parent() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .with_context(|| format!("cannot create {}", dir.display()))?;
    }
    // The caller checked that no daemon answers, so a leftover file is stale.
    let _ = std::fs::remove_file(socket);
    let listener =
        UnixListener::bind(socket).with_context(|| format!("cannot bind {}", socket.display()))?;
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))?;
    let uid = nix::unistd::getuid().as_raw();
    Ok(tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
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
        Err(error) => {
            serde_json::json!({ "ok": false, "error": format!("invalid request: {error}") })
        }
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
        let response = call(&socket, Request::Stats, Duration::from_secs(5))
            .await
            .expect("call")
            .expect("daemon");
        assert_eq!(response["echo"], "Stats");
        server.abort();
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
    }
}
