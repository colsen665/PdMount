//! Pacing of speculative API work (DESIGN.md §5.3, D027).
//!
//! Prefetch and revalidation are guesses; they must never delay what a user
//! is waiting for. Proton rate-limits bursts (HTTP 429), and the SDK retries
//! those with back-off longer than the 20 s metadata deadline. So background
//! work takes turns: one at a time, at most one start per `interval`, and
//! only while no foreground request (a listing or read a FUSE request waits
//! on) is in flight.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::{Mutex, MutexGuard, Notify};
use tokio::time::Instant;

pub struct Gate {
    /// Earliest start of the next background turn; the guard is the turn.
    next_start: Mutex<Instant>,
    foreground: AtomicUsize,
    quiet: Notify,
    /// Spacing of turns, in microseconds.
    interval: AtomicU64,
}

/// Marks a foreground request in flight.
pub struct Foreground<'a>(&'a Gate);

impl Drop for Foreground<'_> {
    fn drop(&mut self) {
        if self.0.foreground.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.0.quiet.notify_waiters();
        }
    }
}

/// A background turn; the next one starts after this is dropped.
pub struct Turn<'a> {
    _next_start: MutexGuard<'a, Instant>,
}

impl Gate {
    pub fn new(interval: Duration) -> Self {
        Self {
            next_start: Mutex::new(Instant::now()),
            foreground: AtomicUsize::new(0),
            quiet: Notify::new(),
            interval: AtomicU64::new(micros(interval)),
        }
    }

    /// Spaces turns `interval` apart from now on (tests that use the fake
    /// backend and need no pacing).
    #[cfg(test)]
    pub(crate) fn set_interval(&self, interval: Duration) {
        self.interval.store(micros(interval), Ordering::Relaxed);
    }

    pub fn foreground(&self) -> Foreground<'_> {
        self.foreground.fetch_add(1, Ordering::AcqRel);
        Foreground(self)
    }

    fn foreground_in_flight(&self) -> usize {
        self.foreground.load(Ordering::Acquire)
    }

    /// Waits for a background turn.
    pub async fn background(&self) -> Turn<'_> {
        let mut next_start = self.next_start.lock().await;
        tokio::time::sleep_until(*next_start).await;
        self.wait_quiet().await;
        *next_start = Instant::now() + Duration::from_micros(self.interval.load(Ordering::Relaxed));
        Turn {
            _next_start: next_start,
        }
    }

    async fn wait_quiet(&self) {
        loop {
            let notified = self.quiet.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.foreground_in_flight() == 0 {
                return;
            }
            notified.await;
        }
    }
}

fn micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test(start_paused = true)]
    async fn turns_are_serial_and_spaced() {
        let gate = Arc::new(Gate::new(Duration::from_millis(500)));
        let start = Instant::now();
        let mut starts = Vec::new();
        for _ in 0..3 {
            let _turn = gate.background().await;
            starts.push(start.elapsed());
        }
        assert_eq!(starts[0], Duration::ZERO);
        assert!(starts[1] >= Duration::from_millis(500));
        assert!(starts[2] >= Duration::from_millis(1000));
    }

    #[tokio::test(start_paused = true)]
    async fn background_waits_for_foreground_to_finish() {
        let gate = Arc::new(Gate::new(Duration::ZERO));
        let foreground = gate.foreground();
        let waiter = {
            let gate = Arc::clone(&gate);
            tokio::spawn(async move {
                let _turn = gate.background().await;
                Instant::now()
            })
        };
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert!(
            !waiter.is_finished(),
            "background ran while foreground was in flight"
        );
        let released = Instant::now();
        drop(foreground);
        let started = waiter.await.expect("join");
        assert!(started >= released);
    }
}
