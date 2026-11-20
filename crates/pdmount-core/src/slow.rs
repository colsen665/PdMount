//! Reads a user waits on for long (DESIGN.md §6.4, D137): when a read that
//! is not a scan read (D111) has waited [`SLOW_AFTER`] for a file's blocks,
//! in any tree, the file is reported as waited for until its last such read
//! got its bytes or failed. The daemon turns the reports into a
//! notification; the engine only measures.
//!
//! The wait is the read's own: a read served from the cache, or by a
//! readahead that arrives in time, is not slow. Only the slow path is
//! watched, with one timer per read and none while nobody subscribes; the
//! hot path (reads served locally) is not touched. One [`SlowReads`] per
//! session, on the [`crate::scan::ScanGuard`] every tree shares, so a file
//! is named by its tree's seat on the guard and its inode.

use crate::vfs::Ino;
use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::broadcast;

/// How long a read waits before its file is reported.
pub const SLOW_AFTER: Duration = Duration::from_secs(5);

/// A file in a session: its tree's seat on the scan guard, and its inode.
pub type FileKey = (u64, Ino);

/// A report about a file a user waits on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SlowRead {
    /// A read of the file has waited [`SLOW_AFTER`], and none of its other
    /// reads was already reported: the wait `wait` begins.
    Began {
        wait: u64,
        file: FileKey,
        /// The file's name (not its path), as the tree shows it.
        name: String,
        size: u64,
    },
    /// The last slow read of wait `wait` got its bytes or failed.
    Ended { wait: u64 },
}

/// The session's slow reads and their subscribers.
pub struct SlowReads {
    /// [`SLOW_AFTER`] in milliseconds; shorter in tests.
    after_ms: AtomicU64,
    /// Files with slow reads now: the wait's id and how many reads.
    waiting: parking_lot::Mutex<HashMap<FileKey, (u64, usize)>>,
    next: AtomicU64,
    events: broadcast::Sender<SlowRead>,
}

impl Default for SlowReads {
    fn default() -> Self {
        Self {
            after_ms: AtomicU64::new(SLOW_AFTER.as_millis() as u64),
            waiting: parking_lot::Mutex::default(),
            next: AtomicU64::new(1),
            events: broadcast::channel(64).0,
        }
    }
}

impl SlowReads {
    /// The reports from now on. A receiver that falls behind misses the
    /// oldest.
    pub fn subscribe(&self) -> broadcast::Receiver<SlowRead> {
        self.events.subscribe()
    }

    #[cfg(test)]
    pub(crate) fn set_after(&self, after: Duration) {
        self.after_ms
            .store(after.as_millis() as u64, Ordering::Relaxed);
    }

    /// Runs `read` of file `file`; once it has waited [`SLOW_AFTER`], the
    /// file is reported (named by `describe`: its name and size) until the
    /// read ends. Without a subscriber, `read` just runs.
    pub async fn watch<T>(
        &self,
        file: FileKey,
        describe: impl FnOnce() -> Option<(String, u64)>,
        read: impl Future<Output = T>,
    ) -> T {
        if self.events.receiver_count() == 0 {
            return read.await;
        }
        let after = Duration::from_millis(self.after_ms.load(Ordering::Relaxed));
        let mut read = std::pin::pin!(read);
        match tokio::time::timeout(after, &mut read).await {
            Ok(value) => value,
            Err(_) => {
                let _waiting = describe().map(|(name, size)| self.began(file, name, size));
                read.await
            }
        }
    }

    /// One more slow read of `file`: reported if it is the first.
    fn began(&self, file: FileKey, name: String, size: u64) -> Waiting<'_> {
        let first = {
            let mut waiting = self.waiting.lock();
            let entry = waiting
                .entry(file)
                .or_insert_with(|| (self.next.fetch_add(1, Ordering::Relaxed), 0));
            entry.1 += 1;
            (entry.1 == 1).then_some(entry.0)
        };
        if let Some(wait) = first {
            tracing::debug!(wait, name, size, "a read has waited long for its blocks");
            // No subscriber is not an error.
            let _ = self.events.send(SlowRead::Began {
                wait,
                file,
                name,
                size,
            });
        }
        Waiting { reads: self, file }
    }

    fn ended(&self, file: FileKey) {
        let ended = {
            let mut waiting = self.waiting.lock();
            match waiting.get_mut(&file) {
                Some((wait, count)) => {
                    *count -= 1;
                    let wait = *wait;
                    (*count == 0).then(|| {
                        waiting.remove(&file);
                        wait
                    })
                }
                None => None,
            }
        };
        if let Some(wait) = ended {
            let _ = self.events.send(SlowRead::Ended { wait });
        }
    }
}

/// A slow read in progress: its end is counted when dropped, also when the
/// read is cancelled.
struct Waiting<'a> {
    reads: &'a SlowReads,
    file: FileKey,
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        self.reads.ended(self.file);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn reads(after_ms: u64) -> Arc<SlowReads> {
        let reads = Arc::new(SlowReads::default());
        reads.set_after(Duration::from_millis(after_ms));
        reads
    }

    fn describe() -> Option<(String, u64)> {
        Some(("a.mp4".into(), 7))
    }

    #[tokio::test]
    async fn a_fast_read_is_not_reported() {
        let reads = reads(200);
        let mut events = reads.subscribe();
        let value = reads.watch((1, 2), describe, async { 5 }).await;
        assert_eq!(value, 5);
        assert!(events.try_recv().is_err());
    }

    #[tokio::test]
    async fn a_slow_read_is_reported_once_and_ends_with_its_last_reader() {
        let reads = reads(50);
        let mut events = reads.subscribe();
        let (release, wait) = tokio::sync::watch::channel(false);
        let read = |reads: Arc<SlowReads>, mut wait: tokio::sync::watch::Receiver<bool>| async move {
            reads
                .watch((1, 2), describe, async move {
                    let _ = wait.wait_for(|go| *go).await;
                })
                .await
        };
        let first = tokio::spawn(read(Arc::clone(&reads), wait.clone()));
        let second = tokio::spawn(read(Arc::clone(&reads), wait.clone()));
        let began = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("reported")
            .expect("open");
        let SlowRead::Began {
            wait: id,
            file,
            name,
            size,
        } = began
        else {
            panic!("not a begin: {began:?}");
        };
        assert_eq!((file, name.as_str(), size), ((1, 2), "a.mp4", 7));
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            events.try_recv().is_err(),
            "the second read is not reported"
        );
        release.send_replace(true);
        first.await.expect("first");
        second.await.expect("second");
        assert_eq!(
            events.recv().await.expect("open"),
            SlowRead::Ended { wait: id }
        );
        assert!(events.try_recv().is_err(), "one end");
    }

    #[tokio::test]
    async fn a_cancelled_read_ends_its_wait() {
        let reads = reads(20);
        let mut events = reads.subscribe();
        let task = {
            let reads = Arc::clone(&reads);
            tokio::spawn(async move {
                reads
                    .watch((3, 4), describe, std::future::pending::<()>())
                    .await
            })
        };
        assert!(matches!(
            events.recv().await.expect("open"),
            SlowRead::Began { .. }
        ));
        task.abort();
        assert!(matches!(
            events.recv().await.expect("open"),
            SlowRead::Ended { .. }
        ));
        assert!(reads.waiting.lock().is_empty());
    }

    #[tokio::test]
    async fn without_a_subscriber_nothing_is_counted() {
        let reads = reads(1);
        reads
            .watch(
                (1, 2),
                describe,
                tokio::time::sleep(Duration::from_millis(20)),
            )
            .await;
        assert!(reads.waiting.lock().is_empty());
    }
}
