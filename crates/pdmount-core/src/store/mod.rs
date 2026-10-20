//! SQLite store (DESIGN.md §4.3, §13.2).
//!
//! One writer thread owns the write connection. Callers send closures that
//! run inside a transaction; everything queued at the moment the writer wakes
//! up is committed together (group commit). Each closure runs in its own
//! savepoint, so a failing write does not undo the others in its batch.
//! Durable writes switch the batch to `synchronous=FULL` and are acknowledged
//! after the commit. Reads use a separate connection (WAL allows both).

use rusqlite::{Connection, OptionalExtension, Transaction};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread::JoinHandle;
use tokio::sync::oneshot;

/// Migrations in order; the schema version is the number applied
/// (`PRAGMA user_version`).
const MIGRATIONS: &[&str] = &[include_str!("0001.sql")];

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
    #[error("cannot create {0}: {1}")]
    Io(PathBuf, std::io::Error),
}

pub type Result<T> = std::result::Result<T, StoreError>;

type WriteFn = Box<dyn FnOnce(&Connection) -> rusqlite::Result<()> + Send>;

struct WriteRequest {
    write: WriteFn,
    durable: bool,
    done: Option<oneshot::Sender<Result<()>>>,
}

pub struct Store {
    path: PathBuf,
    writer: Option<mpsc::Sender<WriteRequest>>,
    thread: Option<JoinHandle<()>>,
    reader: parking_lot::Mutex<Connection>,
}

impl Store {
    /// Opens (or creates) the database and applies pending migrations.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| StoreError::Io(parent.to_owned(), e))?;
        }
        let mut conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "OFF")?;
        migrate(&mut conn, path)?;

        let reader = Connection::open(path)?;
        let (sender, receiver) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("pdmount-store".into())
            .spawn(move || writer_loop(conn, receiver))
            .map_err(|e| StoreError::Io(path.to_owned(), e))?;
        Ok(Self {
            path: path.to_owned(),
            writer: Some(sender),
            thread: Some(thread),
            reader: parking_lot::Mutex::new(reader),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Queues a write; it is committed with the next batch. Errors are logged.
    pub fn write(&self, write: impl FnOnce(&Connection) -> rusqlite::Result<()> + Send + 'static) {
        self.send(Box::new(write), false, None);
    }

    /// Queues a write and resolves once it is committed with
    /// `synchronous=FULL`.
    pub async fn write_durable(
        &self,
        write: impl FnOnce(&Connection) -> rusqlite::Result<()> + Send + 'static,
    ) -> Result<()> {
        let (done, wait) = oneshot::channel();
        self.send(Box::new(write), true, Some(done));
        wait.await.map_err(|_| StoreError::Closed)?
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

    fn send(&self, write: WriteFn, durable: bool, done: Option<oneshot::Sender<Result<()>>>) {
        let request = WriteRequest {
            write,
            durable,
            done,
        };
        let Some(writer) = &self.writer else { return };
        if let Err(mpsc::SendError(request)) = writer.send(request) {
            tracing::error!("store write dropped: the writer has stopped");
            if let Some(done) = request.done {
                let _ = done.send(Err(StoreError::Closed));
            }
        }
    }
}

impl Drop for Store {
    /// Closing the channel lets the writer drain its queue and exit.
    fn drop(&mut self) {
        self.writer.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
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

fn writer_loop(mut conn: Connection, receiver: mpsc::Receiver<WriteRequest>) {
    while let Ok(first) = receiver.recv() {
        let mut batch = vec![first];
        while let Ok(next) = receiver.try_recv() {
            batch.push(next);
        }
        let durable = batch.iter().any(|request| request.durable);
        let mut outcomes = Vec::with_capacity(batch.len());
        let committed = commit_batch(&mut conn, batch, durable, &mut outcomes);
        for (done, outcome) in outcomes {
            let result = match (&committed, outcome) {
                (Err(error), _) => Err(StoreError::Commit(error.to_string())),
                (Ok(()), result) => result,
            };
            if let Some(done) = done {
                let _ = done.send(result);
            } else if let Err(error) = result {
                tracing::error!(%error, "store write failed");
            }
        }
    }
}

type Outcome = (Option<oneshot::Sender<Result<()>>>, Result<()>);

fn commit_batch(
    conn: &mut Connection,
    batch: Vec<WriteRequest>,
    durable: bool,
    outcomes: &mut Vec<Outcome>,
) -> rusqlite::Result<()> {
    if durable {
        conn.pragma_update(None, "synchronous", "FULL")?;
    }
    let result = (|| {
        let mut tx = conn.transaction()?;
        for request in batch {
            let outcome = run_in_savepoint(&mut tx, request.write);
            outcomes.push((request.done, outcome.map_err(StoreError::from)));
        }
        tx.commit()
    })();
    if durable {
        conn.pragma_update(None, "synchronous", "NORMAL")?;
    }
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
