//! SQLite store (DESIGN.md §4.3, §13.2).
//!
//! One writer thread owns the write connection. Callers send closures that
//! run inside a transaction; everything queued at the moment the writer wakes
//! up is committed together (group commit). Each closure runs in its own
//! savepoint, so a failing write does not undo the others in its batch.
//! Reads use a separate connection (WAL allows both).
//!
//! The writer never waits for the disk: it commits with `synchronous=NORMAL`
//! and leaves automatic checkpoints off. A sync thread acknowledges durable
//! writes after it has fsync'd the WAL (what `synchronous=FULL` does at
//! commit), and runs checkpoints. So a fsync never delays the commits that
//! FUSE replies wait for.
//!
//! Every write gets a sequence number in queue order, so "committed up to
//! `n`" covers every write queued before `n`. [`Store::after`] runs a
//! callback once a sequence number is committed; the FUSE layer sends its
//! replies that way (§5.6).

use rusqlite::{Connection, OptionalExtension, Transaction};
use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tokio::sync::oneshot;

/// Migrations in order; the schema version is the number applied
/// (`PRAGMA user_version`).
const MIGRATIONS: &[&str] = &[
    include_str!("0001.sql"),
    include_str!("0002.sql"),
    include_str!("0003.sql"),
];
/// The sync thread checkpoints the WAL once no commit came for
/// [`CHECKPOINT_IDLE`], or after [`CHECKPOINT_WRITES`] writes under steady
/// load. On btrfs a checkpoint (database fsync) slows every file creation
/// in the file system while it runs, hundreds of ms after a burst.
const CHECKPOINT_IDLE: Duration = Duration::from_secs(1);
const CHECKPOINT_WRITES: u64 = if cfg!(test) { 500 } else { 20_000 };
/// Frames left in the WAL after a passive checkpoint above which the
/// writer empties it with a `TRUNCATE` checkpoint between two batches:
/// under steady writes a passive one never catches up with the writer, so
/// the WAL never starts over and grows without bound (14 GB in a few hours
/// of fsx). 64 MiB at 4 KiB pages.
const WAL_LIMIT_FRAMES: i64 = if cfg!(test) { 256 } else { 16_384 };

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error(
        "the state database {path} has schema version {found}, newer than this pdmount supports ({supported}); \
         upgrade pdmount or run `pdmount reset --keep-login`"
    )]
    Downgrade {
        path: PathBuf,
        found: usize,
        supported: usize,
    },
    #[error("the store writer has stopped")]
    Closed,
    #[error("commit failed: {0}")]
    Commit(String),
    #[error("cannot sync the state database: {0}")]
    Sync(String),
    #[error("cannot create {0}: {1}")]
    Io(PathBuf, std::io::Error),
}

pub type Result<T> = std::result::Result<T, StoreError>;

type WriteFn = Box<dyn FnOnce(&Connection) -> rusqlite::Result<()> + Send>;
type DoneFn = Box<dyn FnOnce(Result<()>) + Send>;

struct WriteRequest {
    seq: u64,
    write: WriteFn,
    durable: bool,
    done: Option<DoneFn>,
}

thread_local! {
    /// The last sequence number queued from this thread (any store).
    static QUEUED_HERE: Cell<u64> = const { Cell::new(0) };
}

pub struct Store {
    path: PathBuf,
    /// The last sequence number queued; held while sending, so the channel
    /// carries writes in sequence order.
    writer: parking_lot::Mutex<(u64, Option<mpsc::Sender<WriteRequest>>)>,
    /// The highest sequence number whose batch has committed (or failed).
    committed: Arc<AtomicU64>,
    thread: Option<JoinHandle<()>>,
    syncer: Option<JoinHandle<()>>,
    reader: parking_lot::Mutex<Connection>,
    /// Set by tests: the writer commits nothing more, as after a kill -9.
    #[cfg_attr(not(test), allow(dead_code))]
    crashed: Arc<AtomicBool>,
}

impl Store {
    /// Opens (or creates) the database and applies pending migrations.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| StoreError::Io(parent.to_owned(), e))?;
        }
        let mut conn = Connection::open(path)?;
        // Short contention (a `reset` check, a reader) waits instead of failing.
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "wal_autocheckpoint", 0)?;
        conn.pragma_update(None, "foreign_keys", "OFF")?;
        migrate(&mut conn, path)?;

        let reader = Connection::open(path)?;
        reader.busy_timeout(std::time::Duration::from_secs(5))?;
        let checkpointer = Connection::open(path)?;
        checkpointer.busy_timeout(std::time::Duration::from_secs(5))?;
        let (sender, receiver) = mpsc::channel();
        let (sync_sender, sync_receiver) = mpsc::channel();
        let crashed = Arc::new(AtomicBool::new(false));
        let committed = Arc::new(AtomicU64::new(0));
        let truncate = Arc::new(AtomicBool::new(false));
        let (writer_crashed, writer_committed, writer_truncate) = (
            Arc::clone(&crashed),
            Arc::clone(&committed),
            Arc::clone(&truncate),
        );
        let thread = std::thread::Builder::new()
            .name("pdmount-store".into())
            .spawn(move || {
                writer_loop(
                    conn,
                    receiver,
                    &sync_sender,
                    &writer_crashed,
                    &writer_committed,
                    &writer_truncate,
                );
            })
            .map_err(|e| StoreError::Io(path.to_owned(), e))?;
        let syncer = Syncer {
            db: path.to_owned(),
            wal: None,
            conn: checkpointer,
            committed: Arc::clone(&committed),
            truncate,
        };
        let syncer = std::thread::Builder::new()
            .name("pdmount-store-sync".into())
            .spawn(move || syncer.run(&sync_receiver))
            .map_err(|e| StoreError::Io(path.to_owned(), e))?;
        Ok(Self {
            path: path.to_owned(),
            writer: parking_lot::Mutex::new((0, Some(sender))),
            committed,
            thread: Some(thread),
            syncer: Some(syncer),
            reader: parking_lot::Mutex::new(reader),
            crashed,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// From now on nothing queued is committed, as if the process died
    /// (tests). Durable writes resolve with [`StoreError::Closed`].
    #[cfg(test)]
    pub(crate) fn crash(&self) {
        self.crashed.store(true, Ordering::SeqCst);
    }

    /// Queues a write; it is committed with the next batch. Errors are logged.
    pub fn write(&self, write: impl FnOnce(&Connection) -> rusqlite::Result<()> + Send + 'static) {
        self.send(Box::new(write), false, None);
    }

    /// Queues a write and resolves once it is committed and on disk (the
    /// WAL fsync'd, as with `synchronous=FULL`).
    pub async fn write_durable(
        &self,
        write: impl FnOnce(&Connection) -> rusqlite::Result<()> + Send + 'static,
    ) -> Result<()> {
        let (done, wait) = oneshot::channel();
        let done = Box::new(move |result| drop(done.send(result)));
        self.send(Box::new(write), true, Some(done));
        wait.await.map_err(|_| StoreError::Closed)?
    }

    /// The sequence number of the last write queued.
    pub fn queued(&self) -> u64 {
        self.writer.lock().0
    }

    /// The sequence number of the last write queued from this thread.
    pub fn queued_here() -> u64 {
        QUEUED_HERE.with(Cell::get)
    }

    /// Runs `then` once every write up to `seq` is committed with
    /// `synchronous=NORMAL`: it then survives a crash of the process, not a
    /// power loss. Runs it at once if they are; otherwise on the writer
    /// thread after the commit, so `then` must be short.
    pub fn after(&self, seq: u64, then: impl FnOnce(Result<()>) + Send + 'static) {
        if self.committed.load(Ordering::Acquire) >= seq {
            then(Ok(()));
        } else {
            self.send(Box::new(|_| Ok(())), false, Some(Box::new(then)));
        }
    }

    /// Resolves once everything queued before it is durably committed.
    pub async fn flush(&self) -> Result<()> {
        self.write_durable(|_| Ok(())).await
    }

    /// Runs a read on the read connection. Sees every committed write.
    pub fn read<T>(&self, read: impl FnOnce(&Connection) -> rusqlite::Result<T>) -> Result<T> {
        Ok(read(&self.reader.lock())?)
    }

    pub fn meta_get(&self, key: &str) -> Result<Option<String>> {
        self.read(|conn| meta_get(conn, key))
    }

    fn send(&self, write: WriteFn, durable: bool, done: Option<DoneFn>) {
        let mut writer = self.writer.lock();
        writer.0 += 1;
        let request = WriteRequest {
            seq: writer.0,
            write,
            durable,
            done,
        };
        QUEUED_HERE.with(|queued| queued.set(request.seq));
        let sent = match &writer.1 {
            Some(sender) => sender.send(request).map_err(|e| e.0),
            None => Err(request),
        };
        drop(writer);
        if let Err(request) = sent {
            tracing::error!("store write dropped: the writer has stopped");
            if let Some(done) = request.done {
                done(Err(StoreError::Closed));
            }
        }
    }
}

impl Drop for Store {
    /// Closing the channel lets the writer drain its queue and exit; the
    /// sync thread follows once the writer has dropped its sender.
    fn drop(&mut self) {
        self.writer.lock().1.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        if let Some(syncer) = self.syncer.take() {
            let _ = syncer.join();
        }
    }
}

/// Reads a `meta` value stored as text.
pub fn meta_get(conn: &Connection, key: &str) -> rusqlite::Result<Option<String>> {
    conn.query_row("SELECT value FROM meta WHERE key = ?1", [key], |row| {
        row.get(0)
    })
    .optional()
}

pub fn meta_set(conn: &Connection, key: &str, value: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO meta(key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        (key, value),
    )?;
    Ok(())
}

fn migrate(conn: &mut Connection, path: &Path) -> Result<()> {
    let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let version = usize::try_from(version).unwrap_or(usize::MAX);
    if version > MIGRATIONS.len() {
        return Err(StoreError::Downgrade {
            path: path.to_owned(),
            found: version,
            supported: MIGRATIONS.len(),
        });
    }
    if version == MIGRATIONS.len() {
        return Ok(());
    }
    let tx = conn.transaction()?;
    for sql in &MIGRATIONS[version..] {
        tx.execute_batch(sql)?;
    }
    tx.pragma_update(None, "user_version", MIGRATIONS.len() as i64)?;
    tx.commit()?;
    tracing::info!(
        from = version,
        to = MIGRATIONS.len(),
        "state database migrated"
    );
    Ok(())
}

fn writer_loop(
    mut conn: Connection,
    receiver: mpsc::Receiver<WriteRequest>,
    syncer: &mpsc::Sender<Vec<DoneFn>>,
    crashed: &AtomicBool,
    committed: &AtomicU64,
    truncate: &AtomicBool,
) {
    while let Ok(first) = receiver.recv() {
        // Here and not on the sync thread: a `TRUNCATE` checkpoint on another
        // connection made writes in the writer's transactions fail with
        // SQLITE_BUSY, and those writes were lost. Between two batches
        // nothing is lost if it fails; it waits for readers (busy timeout).
        if truncate.swap(false, Ordering::AcqRel)
            && let Err(error) = checkpoint(&conn, "TRUNCATE")
        {
            tracing::warn!(%error, "store checkpoint failed");
        }
        let mut batch = vec![first];
        while let Ok(next) = receiver.try_recv() {
            batch.push(next);
        }
        if crashed.load(Ordering::SeqCst) {
            for request in batch {
                if let Some(done) = request.done {
                    done(Err(StoreError::Closed));
                }
            }
            continue;
        }
        let last = batch.last().map_or(0, |request| request.seq);
        let mut outcomes = Vec::with_capacity(batch.len());
        let result = commit_batch(&mut conn, batch, &mut outcomes);
        // A failed batch counts too: waiting for it would never end.
        committed.store(last, Ordering::Release);
        let mut durable = Vec::new();
        for (done, is_durable, outcome) in outcomes {
            let outcome = match (&result, outcome) {
                (Err(error), _) => Err(StoreError::Commit(error.to_string())),
                (Ok(()), outcome) => outcome,
            };
            match done {
                Some(done) if is_durable && outcome.is_ok() => durable.push(done),
                Some(done) => done(outcome),
                None => {
                    if let Err(error) = outcome {
                        tracing::error!(%error, "store write failed");
                    }
                }
            }
        }
        if !durable.is_empty()
            && let Err(mpsc::SendError(durable)) = syncer.send(durable)
        {
            for done in durable {
                done(Err(StoreError::Closed));
            }
        }
    }
}

/// Fsyncs the WAL for durable writes and checkpoints it, off the writer.
struct Syncer {
    db: PathBuf,
    /// The WAL, opened at the first sync. SQLite keeps the file (and resets
    /// it in place) while connections are open.
    wal: Option<std::fs::File>,
    conn: Connection,
    committed: Arc<AtomicU64>,
    /// Asks the writer for a `TRUNCATE` checkpoint ([`WAL_LIMIT_FRAMES`]).
    truncate: Arc<AtomicBool>,
}

impl Syncer {
    fn run(mut self, receiver: &mpsc::Receiver<Vec<DoneFn>>) {
        let mut checkpointed = 0;
        let (mut seen, mut quiet_since) = (0, Instant::now());
        loop {
            let mut waiting = Vec::new();
            match receiver.recv_timeout(CHECKPOINT_IDLE) {
                Ok(batch) => waiting.extend(batch),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
            while let Ok(batch) = receiver.try_recv() {
                waiting.extend(batch);
            }
            if !waiting.is_empty() {
                let result = self.sync().map_err(|e| e.to_string());
                if let Err(error) = &result {
                    tracing::error!(%error, "store sync failed");
                }
                for done in waiting {
                    done(result.clone().map_err(StoreError::Sync));
                }
            }
            let committed = self.committed.load(Ordering::Acquire);
            if committed != seen {
                (seen, quiet_since) = (committed, Instant::now());
            }
            let idle = quiet_since.elapsed() >= CHECKPOINT_IDLE;
            if committed > checkpointed && (idle || committed >= checkpointed + CHECKPOINT_WRITES) {
                // PASSIVE never waits for the writer or readers; what it
                // cannot copy now goes next time.
                match checkpoint(&self.conn, "PASSIVE") {
                    Ok(frames) if frames >= WAL_LIMIT_FRAMES => {
                        self.truncate.store(true, Ordering::Release);
                    }
                    Ok(_) => {}
                    Err(error) => tracing::warn!(%error, "store checkpoint failed"),
                }
                checkpointed = committed;
            }
        }
    }

    /// Puts every committed transaction on disk: frames still in the WAL
    /// by its fsync; frames already checkpointed were synced into the
    /// database by the checkpoint.
    fn sync(&mut self) -> std::io::Result<()> {
        if self.wal.is_none() {
            let mut name = self.db.clone().into_os_string();
            name.push("-wal");
            match std::fs::File::open(&name) {
                Ok(wal) => self.wal = Some(wal),
                // No WAL yet: everything is in the database file.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return std::fs::File::open(&self.db)?.sync_data();
                }
                Err(e) => return Err(e),
            }
        }
        self.wal.as_ref().expect("opened").sync_data()
    }
}

/// Runs a checkpoint; returns the frames in the WAL before it.
fn checkpoint(conn: &Connection, mode: &str) -> rusqlite::Result<i64> {
    conn.query_row(&format!("PRAGMA wal_checkpoint({mode})"), [], |row| {
        row.get(1)
    })
}

type Outcome = (Option<DoneFn>, bool, Result<()>);

fn commit_batch(
    conn: &mut Connection,
    batch: Vec<WriteRequest>,
    outcomes: &mut Vec<Outcome>,
) -> rusqlite::Result<()> {
    let result = (|| {
        let mut tx = conn.transaction()?;
        for request in batch {
            let outcome = run_in_savepoint(&mut tx, request.write);
            outcomes.push((
                request.done,
                request.durable,
                outcome.map_err(StoreError::from),
            ));
        }
        tx.commit()
    })();
    if let Err(error) = &result {
        tracing::error!(%error, "store commit failed");
    }
    result
}

fn run_in_savepoint(tx: &mut Transaction<'_>, write: WriteFn) -> rusqlite::Result<()> {
    let savepoint = tx.savepoint()?;
    write(&savepoint)?;
    savepoint.commit()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn durable_writes_are_visible_to_reads() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::open(&dir.path().join("state.db")).expect("open");
        store
            .write_durable(|tx| meta_set(tx, "k", "v"))
            .await
            .expect("write");
        assert_eq!(store.meta_get("k").expect("read").as_deref(), Some("v"));
    }

    #[tokio::test]
    async fn a_failing_write_does_not_undo_its_batch() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::open(&dir.path().join("state.db")).expect("open");
        store.write(|tx| meta_set(tx, "a", "1"));
        store.write(|tx| tx.execute("INSERT INTO nope VALUES (1)", []).map(drop));
        store.write(|tx| meta_set(tx, "b", "2"));
        store.flush().await.expect("flush");
        assert_eq!(store.meta_get("a").expect("read").as_deref(), Some("1"));
        assert_eq!(store.meta_get("b").expect("read").as_deref(), Some("2"));
    }

    #[tokio::test]
    async fn after_waits_for_the_commit_of_earlier_writes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.db");
        let store = Store::open(&path).expect("open");
        let (sender, receiver) = std::sync::mpsc::channel();
        store.write(|tx| meta_set(tx, "k", "v"));
        assert_eq!(Store::queued_here(), store.queued());
        store.after(store.queued(), move |result| {
            // Another connection sees the row only once it is committed.
            let conn = Connection::open(&path).expect("open");
            let _ = sender.send((result.is_ok(), meta_get(&conn, "k").expect("read")));
        });
        let (ok, value) = receiver.recv().expect("callback ran");
        assert!(ok);
        assert_eq!(value.as_deref(), Some("v"));

        // Nothing waiting: the callback runs at once.
        let ran = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&ran);
        store.after(store.queued(), move |_| flag.store(true, Ordering::SeqCst));
        assert!(ran.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn after_reports_a_crash() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::open(&dir.path().join("state.db")).expect("open");
        store.crash();
        store.write(|tx| meta_set(tx, "k", "v"));
        let (sender, receiver) = std::sync::mpsc::channel();
        store.after(store.queued(), move |result| {
            let _ = sender.send(matches!(result, Err(StoreError::Closed)));
        });
        assert!(receiver.recv().expect("callback ran"));
    }

    #[tokio::test]
    async fn queued_writes_survive_drop_and_reopen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.db");
        {
            let store = Store::open(&path).expect("open");
            store.write(|tx| meta_set(tx, "k", "v"));
        }
        let store = Store::open(&path).expect("reopen");
        assert_eq!(store.meta_get("k").expect("read").as_deref(), Some("v"));
    }

    /// Under steady writes a passive checkpoint never catches up with the
    /// writer, so the WAL never starts over; it grew to 14 GB under fsx.
    #[tokio::test]
    async fn steady_writes_keep_the_wal_bounded() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.db");
        let wal = dir.path().join("state.db-wal");
        let store = Arc::new(Store::open(&path).expect("open"));
        let value = "x".repeat(2000);
        let mut largest = 0;
        let stop = Arc::new(AtomicBool::new(false));
        let reader = {
            let (store, stop) = (Arc::clone(&store), Arc::clone(&stop));
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let _ = store.read(|conn| {
                        conn.query_row("SELECT COUNT(*) FROM meta", [], |row| row.get::<_, i64>(0))
                    });
                }
            })
        };
        for round in 0..100 {
            for i in 0..50 {
                let (key, value) = (format!("k{}", round * 50 + i), value.clone());
                // A read first, as many writes do: a transaction that
                // has read cannot wait for a checkpoint to let it write.
                store.write(move |tx| {
                    meta_get(tx, &key)?;
                    meta_set(tx, &key, &value)
                });
            }
            store.flush().await.expect("flush");
            if round % 40 == 39 {
                // Idle: the sync thread checkpoints.
                tokio::time::sleep(CHECKPOINT_IDLE + Duration::from_millis(200)).await;
            }
            largest = largest.max(std::fs::metadata(&wal).map_or(0, |m| m.len()));
        }
        stop.store(true, Ordering::Relaxed);
        reader.join().expect("reader");
        assert!(largest < 4 << 20, "the WAL grew to {largest} bytes");
        // No write was lost to the checkpoints.
        let rows: i64 = store
            .read(|conn| {
                conn.query_row("SELECT COUNT(*) FROM meta WHERE key LIKE 'k%'", [], |row| {
                    row.get(0)
                })
            })
            .expect("count");
        assert_eq!(rows, 5_000);
    }

    #[test]
    fn migration_0003_applies_to_a_version_2_database() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.db");
        let conn = Connection::open(&path).expect("open");
        for sql in &MIGRATIONS[..2] {
            conn.execute_batch(sql).expect("migration");
        }
        conn.pragma_update(None, "user_version", 2)
            .expect("version");
        conn.execute(
            "INSERT INTO conflicts(id, kind, ino, orig_name, local_device, created_at, state)
             VALUES (7, 'edit_edit', 5, 'a', 'dev', 1, 'open')",
            [],
        )
        .expect("old row");
        drop(conn);
        let store = Store::open(&path).expect("migrate");
        let (version, detail, other, indexes): (i64, Option<String>, Option<i64>, i64) = store
            .read(|conn| {
                let version = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
                let (detail, other) = conn.query_row(
                    "SELECT detail, other_ino FROM conflicts WHERE id = 7",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?;
                let indexes = conn.query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE name = 'conflicts_state'",
                    [],
                    |row| row.get(0),
                )?;
                Ok((version, detail, other, indexes))
            })
            .expect("read");
        assert_eq!((version, detail, other, indexes), (3, None, None, 1));
    }

    #[test]
    fn a_newer_schema_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.db");
        let conn = Connection::open(&path).expect("open");
        conn.pragma_update(None, "user_version", 99)
            .expect("set version");
        drop(conn);
        assert!(matches!(
            Store::open(&path),
            Err(StoreError::Downgrade { found: 99, .. })
        ));
    }
}
