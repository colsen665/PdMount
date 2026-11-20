//! Stopping the foreground daemon: signals, waiting for a stop request and the shutdown sequence.

use super::*;

/// SIGTERM, SIGINT and SIGHUP, listened to from before the mount is ready
/// until the daemon exits: once a handler is set, a signal no one receives
/// is lost.
pub(super) struct StopSignals {
    pub(super) terminate: tokio::signal::unix::Signal,
    pub(super) interrupt: tokio::signal::unix::Signal,
    pub(super) hangup: tokio::signal::unix::Signal,
}

impl StopSignals {
    pub(super) fn new() -> std::io::Result<Self> {
        Ok(Self {
            terminate: signal(SignalKind::terminate())?,
            interrupt: signal(SignalKind::interrupt())?,
            hangup: signal(SignalKind::hangup())?,
        })
    }

    /// The next stop signal's name.
    pub(super) async fn recv(&mut self) -> &'static str {
        tokio::select! {
            _ = self.terminate.recv() => "SIGTERM",
            _ = self.interrupt.recv() => "SIGINT",
            _ = self.hangup.recv() => "SIGHUP",
        }
    }
}

/// During the stop: the first signal ends its waits for the session and
/// for running steps, so the store is committed at once (D092); the further
/// roots' drains and waits end too ([`crate::roots::Roots::hurry`]).
pub(super) async fn hurry_on_signal(
    mut signals: StopSignals,
    engine: Arc<Engine>,
    hurried: Arc<std::sync::OnceLock<Instant>>,
    roots: Option<Arc<crate::roots::Roots>>,
) {
    let signal = signals.recv().await;
    if hurried.set(Instant::now()).is_ok() {
        tracing::info!(signal, "stop signal during the stop: ending its waits");
    }
    engine.end_shutdown_wait();
    if let Some(roots) = roots {
        roots.hurry();
    }
}

/// Serves until a signal and the flush it starts, an `unmount` request or
/// an external unmount; returns which. Saves the tokens meanwhile, and
/// handles logind's `PrepareForShutdown` (D091).
pub(super) async fn wait_for_stop(
    mountpoint: &Path,
    mounted: &pdmount_fuse::Mounted,
    late: &Late,
    mut unmount_rx: tokio::sync::mpsc::Receiver<()>,
    flusher: &mut crate::flush::Flusher,
    signals: &mut StopSignals,
) -> anyhow::Result<&'static str> {
    // Rotations are stored as they happen; the timer is a fallback.
    let mut token_timer = tokio::time::interval(TOKEN_SAVE_INTERVAL);
    let mut table_unreadable = false;
    Ok(loop {
        tokio::select! {
            // The first signal flushes (D075); the loop ends with the flush,
            // or at a second signal.
            signal = signals.recv() => if let Some(reason) = flusher.stop(signal) { break reason },
            event = flusher.next() => if let Some(reason) = flusher.handle(event) { break reason },
            // `fusermount3 -u` succeeded in the handler.
            _ = unmount_rx.recv() => break "unmount request",
            _ = token_timer.tick() => {
                if let Some(auth) = late.auth() {
                    auth.spawn_save();
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(500)) => {
                // A copy of the mount in another mount namespace (e.g. a
                // Flatpak sandbox started while mounted) keeps the session
                // alive after an external unmount, so the mount table decides.
                // A table that cannot be read (EMFILE, say) proves nothing.
                if mounted.is_finished() {
                    break "unmounted";
                }
                match mounts::find(mountpoint) {
                    Ok(Some(_)) => table_unreadable = false,
                    Ok(None) => break "unmounted",
                    Err(error) => {
                        if !table_unreadable {
                            tracing::warn!(%error, "cannot read the mount table; checking again");
                        }
                        table_unreadable = true;
                    }
                }
            }
        }
    })
}

/// Unmounts, waits a bounded time for the FUSE session, waits until
/// `deadline` for running steps (D062), commits the store and saves the
/// tokens; the tokens are saved even when the commit fails, whose error is
/// returned. Once `hurried` is set (a second stop signal), the session gets
/// [`HURRIED_SESSION_END`] at most and running steps are not waited for.
pub(super) async fn shutdown(
    engine: &Engine,
    mounted: pdmount_fuse::Mounted,
    auth: Option<&Arc<Auth>>,
    mountpoint: &Path,
    deadline: Instant,
    hurried: &std::sync::OnceLock<Instant>,
) -> pdmount_core::errors::Result<()> {
    engine.stop_starting();
    if matches!(mounts::find(mountpoint), Ok(Some(_))) {
        // Busy mounts are detached; the kernel finishes them when unused.
        if fusermount(mountpoint, false).is_err() {
            let _ = fusermount(mountpoint, true);
        }
    }
    // The session threads end once the kernel drops the connection. Copies
    // of the mount in other namespaces keep it alive; exiting closes
    // /dev/fuse, which ends it for them too. Never wait on them unbounded.
    let session_end = Instant::now() + SESSION_END_TIMEOUT;
    let session_end = || {
        hurried
            .get()
            .map_or(session_end, |&at| session_end.min(at + HURRIED_SESSION_END))
    };
    while !mounted.is_finished() && Instant::now() < session_end() {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    if !mounted.is_finished() {
        tracing::warn!(
            "the FUSE session is still referenced (another mount namespace holds a copy of the mount); exiting anyway"
        );
    }
    // Dropping the session does not join its threads; process exit ends them.
    drop(mounted);
    let stopped = engine.shutdown_by(deadline).await;
    if let Err(error) = &stopped {
        tracing::error!(%error, "could not commit the store at shutdown");
    }
    // One more keyring attempt even if it failed earlier: lost rotated
    // tokens force a new login.
    crate::credentials::retry_keyring();
    if let Some(auth) = auth {
        auth.shutdown();
        auth.save_tokens().await;
    }
    stopped
}
