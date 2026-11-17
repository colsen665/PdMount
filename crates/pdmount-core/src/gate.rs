//! Pacing of speculative API work (DESIGN.md §5.3, D027).
//!
//! Prefetch and revalidation are guesses; they must never delay what a user
//! is waiting for. Proton rate-limits bursts (HTTP 429), and the SDK retries
//! those with back-off longer than the 20 s metadata deadline. So background
//! work takes turns: one at a time, at most one start per `interval`, and
//! only while no foreground request (a listing or read a FUSE request waits
//! on) is in flight. A daemon has one gate for all its trees
//! ([`crate::limits::SessionLimits`]): rate limits are per session.
//!
//! The queue of turns is held only for the spacing and the turn itself: a
//! waiter waits for the foreground to end before it queues, and again,
//! without its place, if a request came meanwhile (D120). So work that may
//! not wait for good ([`Gate::background_patient`], the event sync) gets its
//! turn under a steady stream of user requests, while the others wait.

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

    /// Waits for a background turn: no foreground request in flight, the
    /// turn before it ended and the spacing passed. It waits for the
    /// foreground without holding the queue, so it never holds up a turn
    /// that waits less ([`Gate::background_patient`]).
    pub async fn background(&self) -> Turn<'_> {
        loop {
            self.wait_quiet().await;
            let next_start = self.next_start.lock().await;
            tokio::time::sleep_until(*next_start).await;
            if self.foreground_in_flight() == 0 {
                return self.start(next_start);
            }
            // A request came meanwhile: wait again, out of the queue.
        }
    }

    /// [`Gate::background`] for background work that must not wait for good
    /// under a steady stream of user requests (the event sync, D120): it
    /// waits for foreground requests to end at most `patience`, then takes
    /// its place in the queue and the spacing like any turn. The pacing
    /// stays: one turn at a time, `interval` apart.
    pub async fn background_patient(&self, patience: Duration) -> Turn<'_> {
        let _ = tokio::time::timeout(patience, self.wait_quiet()).await;
        let next_start = self.next_start.lock().await;
        tokio::time::sleep_until(*next_start).await;
        self.start(next_start)
    }

    /// [`Gate::background`] for work a program waits on (a scan read,
    /// D111): after `patience` it stops waiting for foreground requests to
    /// end and takes the turn, or, still waiting for an earlier turn, goes
    /// ahead without one (`None`). So the gate can slow it, never fail it.
    pub async fn background_within(&self, patience: Duration) -> Option<Turn<'_>> {
        let until = Instant::now() + patience;
        loop {
            let _ = tokio::time::timeout_at(until, self.wait_quiet()).await;
            let next_start = tokio::time::timeout_at(until, self.next_start.lock())
                .await
                .ok()?;
            tokio::time::sleep_until((*next_start).min(until)).await;
            if self.foreground_in_flight() == 0 || Instant::now() >= until {
                return Some(self.start(next_start));
            }
            // A request came during the spacing: wait again, out of the
            // queue, within the patience left.
        }
    }

    /// The turn starts now; the next one `interval` later.
    fn start<'a>(&self, mut next_start: MutexGuard<'a, Instant>) -> Turn<'a> {
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
    async fn a_patient_turn_waits_for_foreground_no_longer_than_its_patience() {
        let gate = Arc::new(Gate::new(Duration::from_millis(100)));
        let foreground = gate.foreground();
        let start = Instant::now();
        let turn = gate.background_within(Duration::from_secs(2)).await;
        assert!(turn.is_some(), "the turn is taken after the patience");
        assert_eq!(start.elapsed(), Duration::from_secs(2));
        // A second caller waits for the first turn, but gives up without one.
        let second = {
            let gate = Arc::clone(&gate);
            tokio::spawn(async move {
                gate.background_within(Duration::from_secs(1))
                    .await
                    .is_some()
            })
        };
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(
            !second.await.expect("join"),
            "no turn while the first is held"
        );
        drop(turn);
        drop(foreground);
        // Quiet and spaced: the next turn comes at once.
        let quick = Instant::now();
        assert!(
            gate.background_within(Duration::from_secs(1))
                .await
                .is_some()
        );
        assert_eq!(quick.elapsed(), Duration::ZERO);
    }

    /// A turn held back by user requests waits out of the queue: a patient
    /// turn asked for later goes after its patience, still spaced (D120).
    #[tokio::test(start_paused = true)]
    async fn a_turn_waiting_for_users_does_not_hold_up_a_patient_one() {
        let gate = Arc::new(Gate::new(Duration::from_millis(500)));
        let foreground = gate.foreground();
        let start = Instant::now();
        let speculative = {
            let gate = Arc::clone(&gate);
            tokio::spawn(async move {
                let _turn = gate.background().await;
                Instant::now()
            })
        };
        tokio::time::sleep(Duration::from_secs(1)).await;
        let first = {
            let _turn = gate.background_patient(Duration::from_secs(2)).await;
            start.elapsed()
        };
        assert_eq!(first, Duration::from_secs(3), "after its patience");
        let second = {
            let _turn = gate.background_patient(Duration::from_secs(2)).await;
            start.elapsed()
        };
        assert_eq!(second, Duration::from_secs(5), "patience again");
        // Without a user request, a patient turn still keeps the spacing.
        drop(foreground);
        let third = {
            let _turn = gate.background_patient(Duration::from_secs(2)).await;
            start.elapsed()
        };
        assert!(third >= second + Duration::from_millis(500), "{third:?}");
        let speculative = speculative.await.expect("join");
        assert!(speculative - start >= second, "it waited for the user");
    }

    /// A user's request that comes while a turn of `background_within`
    /// waits for the spacing holds it back, out of the queue, until the
    /// request ends (within the patience).
    #[tokio::test(start_paused = true)]
    async fn a_request_during_the_spacing_holds_back_a_turn_within_patience() {
        let gate = Arc::new(Gate::new(Duration::from_millis(500)));
        let start = Instant::now();
        drop(gate.background().await);
        let user = {
            let gate = Arc::clone(&gate);
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(200)).await;
                let _foreground = gate.foreground();
                tokio::time::sleep(Duration::from_millis(1800)).await;
            })
        };
        let turn = gate.background_within(Duration::from_secs(10)).await;
        assert!(turn.is_some());
        assert_eq!(start.elapsed(), Duration::from_secs(2), "after the request");
        user.await.expect("join");
    }

    #[tokio::test(start_paused = true)]
    async fn patient_turns_are_serial_and_spaced() {
        let gate = Arc::new(Gate::new(Duration::from_millis(500)));
        let _foreground = gate.foreground();
        let start = Instant::now();
        let turns: Vec<_> = (0..3)
            .map(|_| {
                let gate = Arc::clone(&gate);
                tokio::spawn(async move {
                    let _turn = gate.background_patient(Duration::from_secs(1)).await;
                    let started = start.elapsed();
                    tokio::time::sleep(Duration::from_millis(700)).await;
                    started
                })
            })
            .collect();
        let mut starts = Vec::new();
        for turn in turns {
            starts.push(turn.await.expect("join"));
        }
        starts.sort();
        // One at a time: each starts when the one before ended.
        assert_eq!(
            starts,
            [
                Duration::from_secs(1),
                Duration::from_millis(1700),
                Duration::from_millis(2400)
            ]
        );
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
