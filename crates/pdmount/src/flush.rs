//! The flush at a stop (DESIGN.md §14.4, D075, D091).
//!
//! SIGTERM, SIGINT or SIGHUP, or logind's `PrepareForShutdown(true)`, starts
//! a flush: `syncfs` on the mount, then a barrier over the queue as it is,
//! uploaded without the debounce until it is reached, the connection is
//! lost, or `timeouts.shutdown_grace` has passed. The grace counts from the
//! first of the two triggers. A signal during the flush that
//! `PrepareForShutdown` started continues it, and the daemon stops when it
//! ends; a second signal ends the wait at once, and the stop that follows
//! does not wait for running steps (D092). After a flush that
//! `PrepareForShutdown` started the daemon keeps running until the signal
//! that stops it; `PrepareForShutdown(false)` (a cancelled shutdown) forgets
//! the shutdown and takes a new lock. With `sync.pending_marker`, a flush
//! still running 10 s before the end of the grace leaves the pending marker
//! for other devices (D076, `pdmount_core::engine::marker`), and trashes it
//! again when the barrier is reached with nothing left (D096).
//!
//! logind sends `PrepareForShutdown(false)` when a delayed shutdown does not
//! happen (cancelled, or the job failed); without it, `since` would stay
//! set after the flush and a later stop would find its grace used up.

use crate::logind::Inhibitor;
use pdmount_core::engine::Engine;
use pdmount_core::engine::marker::{MARKER_WINDOW, MarkerOutcome};
use pdmount_core::health::Connectivity;
use pdmount_core::journal::{FlushBarrier, FlushEnd, FlushOutcome};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// `syncfs` gets this long at most: dirty pages of shared writable mmaps
/// reach the daemon before the barrier (D083: `write()` already has).
const SYNCFS_TIMEOUT: Duration = Duration::from_secs(5);

/// What started a flush.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Trigger {
    PrepareForShutdown,
    Signal,
}

/// What [`Flusher::next`] saw.
pub enum Event {
    Prepare(bool),
    /// The flush task ended.
    Finished(Option<FlushOutcome>),
    /// logind is not watched (any more).
    NoLogind,
}

pub struct Flusher {
    engine: Arc<Engine>,
    mountpoint: PathBuf,
    grace: Duration,
    inhibitor: Option<Inhibitor>,
    /// When the shutdown started: the first `PrepareForShutdown(true)` or
    /// stop signal.
    since: Option<Instant>,
    task: Option<tokio::task::JoinHandle<Option<FlushOutcome>>>,
    /// The signal that stops the daemon once the flush ends.
    stopping: Option<&'static str>,
    /// When a second signal came: the stop then waits for nothing (D092).
    hurried: Option<Instant>,
    /// Whether the flush that ran last started after the grace of an
    /// earlier flush was used up (`PrepareForShutdown`, then a signal):
    /// the log says so instead of "the grace ran out" a second time.
    spent: bool,
}

impl Flusher {
    pub fn new(engine: Arc<Engine>, mountpoint: PathBuf, inhibitor: Option<Inhibitor>) -> Self {
        let grace = engine.config().timeouts.shutdown_grace.0;
        Self {
            engine,
            mountpoint,
            grace,
            inhibitor,
            since: None,
            task: None,
            stopping: None,
            hurried: None,
            spent: false,
        }
    }

    /// The next event; pending forever when there is nothing to watch.
    pub async fn next(&mut self) -> Event {
        let prepare = async {
            match self.inhibitor.as_mut() {
                Some(inhibitor) => match inhibitor.next().await {
                    Some(active) => Event::Prepare(active),
                    None => Event::NoLogind,
                },
                None => std::future::pending().await,
            }
        };
        let finished = async {
            match self.task.as_mut() {
                Some(task) => Event::Finished(task.await.ok().flatten()),
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            event = prepare => event,
            event = finished => event,
        }
    }

    /// Handles an event of [`Self::next`]; returns the stop signal when the
    /// daemon should stop now.
    pub fn handle(&mut self, event: Event) -> Option<&'static str> {
        match event {
            Event::Prepare(true) => {
                if self.stopping.is_none() && self.since.is_none() {
                    self.since = Some(Instant::now());
                    self.start(Trigger::PrepareForShutdown, false);
                }
                None
            }
            Event::Prepare(false) => {
                if self.stopping.is_none() {
                    if let Some(task) = self.task.take() {
                        task.abort();
                    }
                    if self.since.take().is_some() {
                        tracing::info!("the shutdown was cancelled; the mount stays");
                    }
                    if let Some(inhibitor) = &self.inhibitor {
                        inhibitor.take();
                    }
                }
                None
            }
            Event::NoLogind => {
                self.inhibitor = None;
                None
            }
            Event::Finished(outcome) => {
                self.task = None;
                if let Some(outcome) = outcome {
                    log_outcome(&outcome, self.spent);
                }
                // At the barrier, at the deadline, at once when the queue is
                // empty or the drive cannot be reached (D091).
                if let Some(inhibitor) = &self.inhibitor {
                    inhibitor.release();
                }
                self.stopping
            }
        }
    }

    /// A stop signal: starts the flush, or continues the one running.
    /// Returns the signal when the daemon should stop now, without waiting:
    /// at a second signal, which also ends the grace and releases the lock,
    /// so the store is committed before a service manager's kill (D092).
    pub fn stop(&mut self, signal: &'static str) -> Option<&'static str> {
        if self.stopping.is_some() {
            tracing::info!(signal, "second stop signal: ending the flush");
            if let Some(task) = self.task.take() {
                task.abort();
            }
            self.hurried.get_or_insert_with(Instant::now);
            if let Some(inhibitor) = &self.inhibitor {
                inhibitor.release();
            }
            return Some(signal);
        }
        tracing::info!(signal, "stopping after a flush");
        self.stopping = Some(signal);
        // A shutdown that started earlier and whose flush has ended: if its
        // grace is gone, this flush has none left (one grace for the whole
        // stop, D092).
        let spent = self
            .since
            .is_some_and(|since| since + self.grace <= Instant::now());
        self.since.get_or_insert_with(Instant::now);
        if self.task.is_some() {
            tracing::info!(
                signal,
                "continuing the flush that PrepareForShutdown started"
            );
        } else {
            self.start(Trigger::Signal, spent);
        }
        None
    }

    /// When the grace ends: counted from the start of the shutdown, or from
    /// now when there was none (`unmount`, D062); at a second signal, then.
    pub fn deadline(&self) -> Instant {
        let end = self.since.unwrap_or_else(Instant::now) + self.grace;
        self.hurried.map_or(end, |hurried| hurried.min(end))
    }

    /// Whether a second signal ended the flush: the stop waits for nothing.
    pub fn hurried(&self) -> bool {
        self.hurried.is_some()
    }

    /// Ends the flush and releases the lock. After an `unmount` request or
    /// an external unmount during a `PrepareForShutdown` flush, the lock is
    /// held until here: the stop's wait for running steps still uploads,
    /// within the same grace.
    pub fn end(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
        self.inhibitor = None;
    }

    fn start(&mut self, trigger: Trigger, spent: bool) {
        self.spent = spent;
        let deadline = self.deadline();
        let engine = Arc::clone(&self.engine);
        let mountpoint = self.mountpoint.clone();
        self.task = Some(tokio::spawn(async move {
            flush(&engine, &mountpoint, deadline, trigger, spent).await
        }));
    }
}

async fn flush(
    engine: &Engine,
    mountpoint: &Path,
    deadline: Instant,
    trigger: Trigger,
    spent: bool,
) -> Option<FlushOutcome> {
    // Also offline: pages left in the kernel are lost once the daemon is
    // gone, while staged ones wait for the next mount.
    syncfs(
        mountpoint,
        SYNCFS_TIMEOUT.min(deadline.saturating_duration_since(Instant::now())),
    )
    .await;
    let state = engine.health().state();
    if state != Connectivity::Online {
        tracing::info!(
            state = state.as_str(),
            "no flush: the drive cannot be reached"
        );
        return None;
    }
    let barrier = engine.flush_barrier();
    if barrier.is_empty() {
        tracing::info!("nothing to flush");
        return None;
    }
    let grace = deadline.saturating_duration_since(Instant::now());
    if !spent {
        tracing::info!(
            entries = barrier.len(),
            grace_secs = grace.as_secs(),
            "flushing the queue"
        );
    }
    if trigger == Trigger::PrepareForShutdown {
        // Best effort, never waited for (D091).
        crate::notify::desktop(
            &shutdown_summary(barrier.len()),
            &format!("The shutdown waits up to {} s for them.", grace.as_secs()),
        );
    }
    if engine.pending_marker_enabled() {
        Some(flush_with_marker(engine, &barrier, deadline).await)
    } else {
        Some(engine.flush_to(&barrier, deadline).await)
    }
}

/// The flush with the pending marker (D076): when the barrier is still
/// moving [`MARKER_WINDOW`] before the deadline, the marker is written
/// then, and the flush goes on until the deadline. The marker names what
/// was left when it was written; the next drain trashes it. When that is
/// the end of this flush, it is trashed here, within the grace, since the
/// drain watcher stops with the daemon (D096).
async fn flush_with_marker(
    engine: &Engine,
    barrier: &FlushBarrier,
    deadline: Instant,
) -> FlushOutcome {
    let point = deadline
        .checked_sub(MARKER_WINDOW)
        .unwrap_or_else(Instant::now)
        .max(Instant::now());
    let first = engine.flush_to(barrier, point).await;
    if first.ended != FlushEnd::Deadline {
        return first;
    }
    let marker = engine.leave_pending_marker(deadline).await;
    let rest = engine.flush_to(barrier, deadline).await;
    if matches!(marker, MarkerOutcome::Written { .. }) && rest.ended == FlushEnd::Reached {
        engine.trash_pending_marker_by(deadline).await;
    }
    FlushOutcome {
        elapsed: first.elapsed + rest.elapsed,
        ..rest
    }
}

/// "1 entry" or "N entries".
fn entries(n: usize) -> String {
    match n {
        1 => "1 entry".to_owned(),
        n => format!("{n} entries"),
    }
}

fn shutdown_summary(entries: usize) -> String {
    match entries {
        1 => "PdMount: uploading 1 change before shutdown".to_owned(),
        n => format!("PdMount: uploading {n} changes before shutdown"),
    }
}

/// The warning when the grace ended a flush; `spent` when an earlier flush
/// of the same stop had used it up.
fn deadline_line(outcome: &FlushOutcome, secs: &str, spent: bool) -> String {
    if spent {
        format!(
            "no grace left: it was used at PrepareForShutdown; stopping with {} left",
            entries(outcome.left)
        )
    } else {
        format!(
            "the grace ran out: flushed {} of {} in {secs} s",
            outcome.flushed(),
            entries(outcome.barrier)
        )
    }
}

fn log_outcome(outcome: &FlushOutcome, spent: bool) {
    let secs = format!("{:.1}", outcome.elapsed.as_secs_f64());
    match outcome.ended {
        FlushEnd::Reached if outcome.left == 0 => {
            tracing::info!("flushed {} in {secs} s", entries(outcome.flushed()));
        }
        FlushEnd::Reached => tracing::info!(
            parked = outcome.left,
            "flushed {} in {secs} s; the others are parked",
            entries(outcome.flushed())
        ),
        FlushEnd::Deadline => tracing::warn!("{}", deadline_line(outcome, &secs, spent)),
        FlushEnd::NotOnline(state) => tracing::warn!(
            state = state.as_str(),
            "the drive cannot be reached: flushed {} of {} in {secs} s",
            outcome.flushed(),
            entries(outcome.barrier)
        ),
    }
}

/// `syncfs` on the mount, on a thread of its own: the kernel writes dirty
/// pages back through this daemon's FUSE threads, and a wedged mount must
/// not hold up the stop.
async fn syncfs(mountpoint: &Path, timeout: Duration) {
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    let path = mountpoint.to_owned();
    // A plain thread: the runtime does not wait for it at exit.
    let spawned = std::thread::Builder::new()
        .name("syncfs".into())
        .spawn(move || {
            let result = std::fs::File::open(&path)
                .and_then(|dir| nix::unistd::syncfs(&dir).map_err(std::io::Error::from));
            let _ = done_tx.send(result);
        });
    if let Err(error) = spawned {
        tracing::warn!(%error, "cannot start syncfs");
        return;
    }
    match tokio::time::timeout(timeout, done_rx).await {
        Ok(Ok(Ok(()))) => {}
        Ok(Ok(Err(error))) => tracing::warn!(%error, "syncfs on the mount failed"),
        Ok(Err(_)) => {}
        Err(_) => tracing::warn!(?timeout, "syncfs on the mount did not finish; going on"),
    }
}

/// The last line of a stop: what the next mount resumes.
pub fn log_left(engine: &Engine) {
    let counts = engine.queue_counts();
    let left = counts.pending + counts.parked;
    if left > 0 {
        tracing::warn!(
            parked = counts.parked,
            "{} left for the next mount",
            entries(left)
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pdmount_backend::fake::FakeBackend;
    use pdmount_core::Config;
    use pdmount_core::device::{DeviceIdentity, Uuid};
    use pdmount_core::engine::EngineOptions;

    async fn flusher(dir: &Path) -> Flusher {
        let device = DeviceIdentity {
            id: Uuid::new_v4(),
            name: "test".into(),
        };
        let options = EngineOptions::in_dir(
            dir,
            Config::default(),
            device,
            pdmount_core::conflicts::system_clock(),
        );
        let (engine, _invalidations) = Engine::open(Arc::new(FakeBackend::new()), options)
            .await
            .expect("engine");
        Flusher::new(engine, dir.to_owned(), None)
    }

    /// After the grace ran out at `PrepareForShutdown`, a later signal
    /// starts a flush that says the grace was used, not that it ran out.
    #[tokio::test]
    async fn a_later_signal_finds_the_grace_used() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut flusher = flusher(dir.path()).await;
        assert_eq!(flusher.stop("SIGTERM"), None);
        assert!(!flusher.spent, "the first flush has its grace");
        flusher.end();

        let mut flusher = self::flusher(dir.path()).await;
        flusher.since = Some(Instant::now() - flusher.grace - Duration::from_secs(1));
        assert_eq!(flusher.stop("SIGTERM"), None);
        assert!(flusher.spent, "the grace of PrepareForShutdown is gone");
        assert!(flusher.deadline() <= Instant::now());
        flusher.end();

        // The same shutdown with time left: a normal flush.
        let mut flusher = self::flusher(dir.path()).await;
        flusher.since = Some(Instant::now());
        assert_eq!(flusher.stop("SIGTERM"), None);
        assert!(!flusher.spent);
        flusher.end();
    }

    #[test]
    fn the_deadline_line_says_when_the_grace_was_used() {
        let outcome = FlushOutcome {
            barrier: 5,
            left: 3,
            elapsed: Duration::from_secs(1),
            ended: FlushEnd::Deadline,
        };
        assert_eq!(
            deadline_line(&outcome, "1.0", false),
            "the grace ran out: flushed 2 of 5 entries in 1.0 s"
        );
        let line = deadline_line(&outcome, "0.0", true);
        assert_eq!(
            line,
            "no grace left: it was used at PrepareForShutdown; stopping with 3 entries left"
        );
        assert!(!line.contains("ran out"));
    }

    #[test]
    fn entry_counts_agree_in_number() {
        assert_eq!(entries(0), "0 entries");
        assert_eq!(entries(1), "1 entry");
        assert_eq!(entries(8), "8 entries");
    }

    /// The first signal flushes within the grace; a second one stops at
    /// once and leaves no grace for the stop's wait (D092).
    #[tokio::test]
    async fn a_second_signal_ends_the_grace() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut flusher = flusher(dir.path()).await;
        let grace = flusher.grace;
        assert!(grace >= Duration::from_secs(60));
        let before = Instant::now();
        assert_eq!(flusher.stop("SIGTERM"), None, "the first signal flushes");
        assert!(!flusher.hurried());
        assert!(flusher.deadline() >= before + grace);
        assert_eq!(
            flusher.deadline(),
            flusher.deadline(),
            "counted from the signal"
        );

        assert_eq!(
            flusher.stop("SIGINT"),
            Some("SIGINT"),
            "the second one stops"
        );
        assert!(flusher.hurried());
        assert!(flusher.task.is_none(), "the flush was ended");
        let deadline = flusher.deadline();
        assert!(deadline <= Instant::now(), "the stop waits for nothing");
        assert_eq!(flusher.deadline(), deadline);
        flusher.end();
    }

    /// Without a second signal the deadline stays at the end of the grace.
    #[tokio::test]
    async fn a_flush_that_ends_keeps_its_grace() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut flusher = flusher(dir.path()).await;
        let before = Instant::now();
        assert_eq!(flusher.stop("SIGTERM"), None);
        // An empty queue: the flush ends at once and the daemon stops.
        let event = tokio::time::timeout(Duration::from_secs(10), flusher.next())
            .await
            .expect("the flush ends");
        assert_eq!(flusher.handle(event), Some("SIGTERM"));
        assert!(!flusher.hurried());
        assert!(flusher.deadline() >= before + flusher.grace);
        flusher.end();
    }
}
