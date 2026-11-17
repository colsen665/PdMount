//! logind's shutdown delay lock (DESIGN.md §14.4, D091).
//!
//! While mounted, the daemon holds a `delay` inhibitor lock for `shutdown`
//! (`org.freedesktop.login1.Manager.Inhibit`; the lock is the returned file
//! descriptor) and watches `PrepareForShutdown` on the system bus. logind
//! then holds a shutdown it starts until the lock is released, for at most
//! its `InhibitDelayMaxSec`. Without a system bus or logind, or with the
//! lock refused, this logs once and ends: SIGTERM alone starts the flush.
//!
//! Only the system bus is used, through `zbus` (review in
//! `docs/security/threat-model.md`, "Dependencies"). Any local user may send
//! a signal to this connection, so a `PrepareForShutdown` counts only when
//! it comes from the current owner of `org.freedesktop.login1`.

use futures::StreamExt;
use std::os::fd::OwnedFd;
use std::time::Duration;
use tokio::sync::mpsc;
use zbus::message::Type;
use zbus::{Connection, MatchRule, MessageStream};

const LOGIND: &str = "org.freedesktop.login1";
const LOGIND_PATH: &str = "/org/freedesktop/login1";
const MANAGER: &str = "org.freedesktop.login1.Manager";
/// Connecting to the system bus and each call to it.
const BUS_TIMEOUT: Duration = Duration::from_secs(10);

enum Command {
    Release,
    Take,
}

/// The lock and the watch, run by a background task. Dropping it releases
/// the lock.
pub struct Inhibitor {
    events: mpsc::UnboundedReceiver<bool>,
    commands: mpsc::UnboundedSender<Command>,
    task: tokio::task::JoinHandle<()>,
}

impl Inhibitor {
    /// Connects, starts watching and takes the lock, in the background.
    pub fn start() -> Self {
        let (events_tx, events) = mpsc::unbounded_channel();
        let (commands, commands_rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(run(events_tx, commands_rx));
        Self {
            events,
            commands,
            task,
        }
    }

    /// The next `PrepareForShutdown`: `true` when a shutdown starts, `false`
    /// when it was cancelled. `None` once there is no logind to watch.
    pub async fn next(&mut self) -> Option<bool> {
        self.events.recv().await
    }

    /// Releases the lock, if one is held: logind may go on.
    pub fn release(&self) {
        let _ = self.commands.send(Command::Release);
    }

    /// Takes a new lock unless one is held (after a cancelled shutdown).
    pub fn take(&self) {
        let _ = self.commands.send(Command::Take);
    }
}

impl Drop for Inhibitor {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn run(events: mpsc::UnboundedSender<bool>, mut commands: mpsc::UnboundedReceiver<Command>) {
    let fallback = "the flush starts at SIGTERM only";
    let connection = match tokio::time::timeout(BUS_TIMEOUT, Connection::system()).await {
        Ok(Ok(connection)) => connection,
        Ok(Err(error)) => {
            tracing::info!(%error, "no system bus; {fallback}");
            return;
        }
        Err(_) => {
            tracing::info!("the system bus did not answer; {fallback}");
            return;
        }
    };
    let mut owner = match name_owner(&connection).await {
        Ok(owner) => owner,
        Err(error) => {
            tracing::info!(%error, "no logind on the system bus; {fallback}");
            return;
        }
    };
    // Subscribed before the lock is taken, so no signal is missed.
    let mut signals = match prepare_signals(&connection).await {
        Ok(stream) => stream,
        Err(error) => {
            tracing::info!(%error, "cannot watch logind's PrepareForShutdown; {fallback}");
            return;
        }
    };
    let mut lock = match inhibit(&connection).await {
        Ok(lock) => {
            tracing::info!("holding a logind delay lock for shutdown");
            Some(lock)
        }
        Err(error) => {
            tracing::info!(%error, "logind refused a shutdown delay lock; {fallback}");
            return;
        }
    };
    loop {
        tokio::select! {
            message = signals.next() => {
                let message = match message {
                    Some(Ok(message)) => message,
                    Some(Err(error)) => {
                        tracing::debug!(%error, "unreadable message on the system bus");
                        continue;
                    }
                    None => {
                        tracing::warn!("lost the system bus; {fallback}");
                        return;
                    }
                };
                let sender = message.header().sender().map(|name| name.as_str().to_owned());
                if sender.as_deref() != Some(owner.as_str()) {
                    // logind may have been restarted; anyone else is ignored.
                    match name_owner(&connection).await {
                        Ok(current) if sender.as_deref() == Some(current.as_str()) => owner = current,
                        _ => {
                            tracing::debug!(?sender, "ignoring a PrepareForShutdown not sent by logind");
                            continue;
                        }
                    }
                }
                match message.body().deserialize::<bool>() {
                    Ok(active) => {
                        tracing::info!(active, "logind: PrepareForShutdown");
                        if events.send(active).is_err() {
                            return;
                        }
                    }
                    Err(error) => tracing::debug!(%error, "malformed PrepareForShutdown"),
                }
            }
            command = commands.recv() => match command {
                Some(Command::Release) => {
                    if lock.take().is_some() {
                        tracing::info!("released the logind delay lock");
                    }
                }
                Some(Command::Take) => {
                    if lock.is_none() {
                        match inhibit(&connection).await {
                            Ok(new) => {
                                tracing::info!("holding a logind delay lock for shutdown again");
                                lock = Some(new);
                            }
                            Err(error) => tracing::warn!(%error, "logind refused a new shutdown delay lock"),
                        }
                    }
                }
                None => return,
            },
        }
    }
}

/// The unique name that owns `org.freedesktop.login1`.
async fn name_owner(connection: &Connection) -> anyhow::Result<String> {
    let reply = tokio::time::timeout(
        BUS_TIMEOUT,
        connection.call_method(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            Some("org.freedesktop.DBus"),
            "GetNameOwner",
            &LOGIND,
        ),
    )
    .await??;
    Ok(reply.body().deserialize::<String>()?)
}

async fn prepare_signals(connection: &Connection) -> zbus::Result<MessageStream> {
    let rule = MatchRule::builder()
        .msg_type(Type::Signal)
        .sender(LOGIND)?
        .path(LOGIND_PATH)?
        .interface(MANAGER)?
        .member("PrepareForShutdown")?
        .build();
    MessageStream::for_match_rule(rule, connection, None).await
}

/// Takes a `delay` lock for `shutdown`; the lock lasts while the returned
/// descriptor is open. zbus duplicates it with close-on-exec, so children
/// such as `notify-send` do not keep it.
async fn inhibit(connection: &Connection) -> anyhow::Result<OwnedFd> {
    let reply = tokio::time::timeout(
        BUS_TIMEOUT,
        connection.call_method(
            Some(LOGIND),
            LOGIND_PATH,
            Some(MANAGER),
            "Inhibit",
            &(
                "shutdown",
                "PdMount",
                "Uploading changes before shutdown",
                "delay",
            ),
        ),
    )
    .await??;
    let lock: zbus::zvariant::OwnedFd = reply.body().deserialize()?;
    Ok(lock.into())
}
