//! Stopping an engine: the shutdown sequence and its deadline.

use super::*;

impl Engine {
    /// Stops background work and commits the store. Pending journal work
    /// stays in the store and continues at the next mount.
    /// No journal step starts from now on (D062); `shutdown` waits for those
    /// running.
    pub fn stop_starting(&self) {
        self.journal.stop_starting();
    }

    pub async fn shutdown(&self) -> Result<()> {
        self.shutdown_by(std::time::Instant::now() + self.config.timeouts.shutdown_grace.0)
            .await
    }

    /// Ends the wait of [`Self::shutdown_by`] for running steps now, or at
    /// once if it has not started: a second stop signal (D092).
    pub fn end_shutdown_wait(&self) {
        self.shutdown_wait_ended.store(true, Ordering::SeqCst);
    }

    /// [`Self::shutdown`] with the wait for running steps ending at
    /// `deadline`: the end of the grace that a shutdown flush started
    /// (D075), or earlier at [`Self::end_shutdown_wait`]. A step still
    /// running then is cut off and retried at the next mount.
    pub async fn shutdown_by(&self, deadline: std::time::Instant) -> Result<()> {
        self.journal.stop_starting();
        while !self.journal.idle() {
            if self.shutdown_wait_ended.load(Ordering::SeqCst) {
                tracing::warn!(
                    "steps still run at a second stop signal; the next mount retries them"
                );
                break;
            }
            if std::time::Instant::now() >= deadline {
                tracing::warn!(
                    "steps still run after timeouts.shutdown_grace; the next mount retries them"
                );
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        // Before the abort, so no refresh task starts after it (D120).
        self.stop_refresh();
        for task in self.background.lock().drain(..) {
            task.abort();
        }
        self.cache.flush_access();
        let rows: Vec<Ino> = self.tree.read().inos().collect();
        self.persist(&rows);
        self.store.flush().await?;
        remove_committed_staging(
            Arc::clone(&self.store),
            self.staging.clone(),
            Arc::clone(&self.staged),
            Arc::clone(&self.staging_removals),
        )
        .await;
        Ok(())
    }
}
