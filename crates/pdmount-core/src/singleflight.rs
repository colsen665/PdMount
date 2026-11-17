//! In-flight de-duplication (DESIGN.md §6.4).
//!
//! Concurrent requests for the same key share one spawned task. The task runs
//! to completion even when every waiter gives up (deadline expiry), so the
//! result still lands in the cache.

use crate::errors::{CoreError, Result};
use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use std::collections::HashMap;
use std::future::Future;
use std::hash::Hash;
use std::sync::Arc;

type Flight<V> = Shared<BoxFuture<'static, Result<V>>>;

pub struct SingleFlight<K, V> {
    flights: Arc<parking_lot::Mutex<HashMap<K, Flight<V>>>>,
}

impl<K, V> Default for SingleFlight<K, V> {
    fn default() -> Self {
        Self {
            flights: Arc::default(),
        }
    }
}

impl<K, V> SingleFlight<K, V>
where
    K: Eq + Hash + Clone + Send + 'static,
    V: Clone + Send + Sync + 'static,
{
    /// Joins the flight for `key`, starting it with `start` if none runs.
    /// Must be called inside a tokio runtime.
    pub fn run<F>(&self, key: K, start: impl FnOnce() -> F) -> Flight<V>
    where
        F: Future<Output = Result<V>> + Send + 'static,
    {
        let mut flights = self.flights.lock();
        if let Some(flight) = flights.get(&key) {
            return flight.clone();
        }
        let work = start();
        let registry = Arc::clone(&self.flights);
        let done_key = key.clone();
        // The registry lock is held until the flight is inserted, so the
        // removal below cannot run before the insertion.
        let task = tokio::spawn(async move {
            let result = work.await;
            registry.lock().remove(&done_key);
            result
        });
        let flight = async move {
            task.await
                .unwrap_or_else(|error| Err(CoreError::Internal(format!("task failed: {error}"))))
        }
        .boxed()
        .shared();
        flights.insert(key, flight.clone());
        flight
    }

    /// Whether a flight for `key` runs.
    pub fn running(&self, key: &K) -> bool {
        self.flights.lock().contains_key(key)
    }

    pub fn in_flight(&self) -> usize {
        self.flights.lock().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[tokio::test]
    async fn concurrent_requests_share_one_execution() {
        let flights = SingleFlight::<u32, u32>::default();
        let runs = Arc::new(AtomicUsize::new(0));
        let start = |runs: Arc<AtomicUsize>| {
            move || async move {
                runs.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(20)).await;
                Ok(7)
            }
        };
        let a = flights.run(1, start(runs.clone()));
        let b = flights.run(1, start(runs.clone()));
        assert_eq!(a.await.expect("a"), 7);
        assert_eq!(b.await.expect("b"), 7);
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        assert_eq!(flights.in_flight(), 0);
    }

    #[tokio::test]
    async fn abandoned_flights_still_complete() {
        let flights = SingleFlight::<u32, u32>::default();
        let done = Arc::new(AtomicUsize::new(0));
        let marker = done.clone();
        let flight = flights.run(1, move || async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            marker.fetch_add(1, Ordering::SeqCst);
            Ok(1)
        });
        let timed_out = tokio::time::timeout(Duration::from_millis(1), flight).await;
        assert!(timed_out.is_err());
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(done.load(Ordering::SeqCst), 1);
    }
}
