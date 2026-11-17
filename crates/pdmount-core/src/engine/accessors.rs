//! Accessors of the engine's parts, the conflict registry and the commit hook.

use super::*;

impl Engine {
    /// Connectivity of the drive (§9.6).
    pub fn health(&self) -> &Arc<Health> {
        &self.health
    }

    /// The session's transfer limits, for the further trees (D033, D110).
    pub fn limits(&self) -> &Arc<crate::limits::SessionLimits> {
        &self.limits
    }

    pub fn metrics(&self) -> &Arc<Metrics> {
        &self.metrics
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn device(&self) -> &DeviceIdentity {
        &self.device
    }

    /// The time source for conflict names and records.
    pub fn clock(&self) -> &Clock {
        &self.clock
    }

    pub fn runtime(&self) -> &tokio::runtime::Handle {
        &self.runtime
    }

    /// Receives a notice for every conflict registered from now on.
    pub fn subscribe_conflicts(&self) -> tokio::sync::broadcast::Receiver<ConflictNotice> {
        self.notices.subscribe()
    }

    /// Registers a conflict: numbers it, queues its row, logs it and tells
    /// the subscribers. A crash may lose the record, never the conflict copy.
    pub fn record_conflict(&self, note: ConflictNote) -> i64 {
        let path = self.tree.read().path(note.ino);
        let at = self.clock()()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs() as i64);
        let conflict = self.conflicts.lock().insert(note, &self.device.name, at);
        let row = conflict.clone();
        self.store.write(move |conn| store_conflict(conn, &row));
        tracing::warn!(
            id = conflict.id,
            kind = conflict.kind.as_str(),
            path = path.as_deref().unwrap_or(""),
            conflict_name = conflict.conflict_name.as_deref().unwrap_or(""),
            "conflict"
        );
        // No subscriber is not an error.
        let _ = self.notices.send(ConflictNotice {
            id: conflict.id,
            kind: conflict.kind,
            path,
        });
        conflict.id
    }

    /// Records the other copy of a conflict once it shows.
    pub(crate) fn set_conflict_other(&self, id: i64, other: Ino) {
        let row = self.conflicts.lock().set_other_ino(id, other).cloned();
        if let Some(row) = row {
            self.store.write(move |conn| store_conflict(conn, &row));
        }
    }

    /// Changes a conflict's state; `false` for an unknown id.
    pub fn set_conflict_state(&self, id: i64, state: ConflictState) -> bool {
        if self.conflicts.lock().set_state(id, state).is_none() {
            return false;
        }
        self.store.write(move |conn| store_state(conn, id, state));
        true
    }

    /// Every registered conflict, oldest first.
    pub fn conflicts(&self) -> Vec<Conflict> {
        self.conflicts.lock().all().cloned().collect()
    }

    #[cfg(test)]
    pub fn conflict_count(&self) -> usize {
        self.conflicts.lock().count_active()
    }

    /// Conflicts that wait for the user (state `open`).
    pub fn conflict_open_count(&self) -> usize {
        self.conflicts.lock().count_open()
    }

    /// Runs `reply` once the store rows the current request queued are
    /// committed, so a crash of the daemon loses nothing the kernel was told
    /// (§5.6). On the hot path that is what this thread queued; elsewhere,
    /// everything queued so far.
    pub fn after_commit(&self, reply: impl FnOnce() + Send + 'static) {
        self.after_commit_kept(move |_| reply());
    }

    /// [`Self::after_commit`] for the reply of a change: it gets EIO
    /// instead when the engine was retired before the commit ended, since
    /// a tree that left its mount (a gone share, D114) may have exported or
    /// deleted its state without the change.
    pub fn after_commit_kept(&self, reply: impl FnOnce(Result<()>) + Send + 'static) {
        let seq = if crate::metrics::is_hot() {
            Store::queued_here()
        } else {
            self.store.queued()
        };
        let retired = Arc::clone(&self.retired);
        self.store.after(seq, move |result| {
            if let Err(error) = result {
                tracing::warn!(%error, "replying although the store did not commit");
            }
            if retired.load(Ordering::SeqCst) {
                reply(Err(CoreError::Errno(nix::libc::EIO)));
            } else {
                reply(Ok(()));
            }
        });
    }

    /// The tree leaves its mount for good (a gone share, or a mount whose
    /// session outlives its stop): its write gate closes, and a change
    /// whose commit ends from now on is answered EIO, not acknowledged
    /// (D114). Work already queued stays queued.
    pub fn retire(&self) {
        self.retired.store(true, Ordering::SeqCst);
        self.set_write_gate(false);
    }

    /// Whether `handle` is an open file handle that may write: its close
    /// is the reply of a change ([`Self::after_commit_kept`]).
    pub fn handle_writes(&self, handle: u64) -> bool {
        self.files
            .lock()
            .get(&handle)
            .is_some_and(|file| file.writable)
    }

    /// Whether [`Self::retire`] was called: the pollers skip the tree.
    pub fn is_retired(&self) -> bool {
        self.retired.load(Ordering::SeqCst)
    }

    /// Waits until no event pass holds the tree's cursor: a poller's pass
    /// that started before [`Self::retire`] has ended, and later ones skip
    /// the tree ([`crate::poller`]).
    pub async fn wait_for_event_pass(&self) {
        drop(self.event_cursor.lock().await);
    }

    pub fn inode_count(&self) -> usize {
        self.tree.read().len()
    }

    /// Quota as last fetched.
    pub fn storage(&self) -> Option<StorageInfo> {
        *self.storage.lock()
    }

    /// The next value of the remote clock, for a change the executor just
    /// made remotely.
    pub(crate) fn stamp(&self) -> u64 {
        self.remote_clock.fetch_add(1, Ordering::SeqCst) + 1
    }
}
