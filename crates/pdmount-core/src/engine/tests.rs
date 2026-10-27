use super::*;
use crate::access::RuleSource;
use crate::config::AccessMode;
use crate::conflicts::{ConflictKind, Resolution};
use crate::fake_fs::{fake_tree, read_all};
use crate::metrics::hot;
use crate::units::HumanDuration;
use bytes::Bytes;
use pdmount_backend::fake::{FakeBackend, FakeChange, FakeClient, FakePoint};
use pdmount_backend::{BackendError, FileMeta, LinkId, RevisionId, UploadSource};
use std::time::{Instant, UNIX_EPOCH};

fn meta() -> FileMeta {
    FileMeta {
        mtime: UNIX_EPOCH + Duration::from_secs(1_700_000_000),
        mode: Some(0o600),
        media_type: "application/octet-stream".into(),
        sha1: None,
    }
}

async fn seeded() -> FakeBackend {
    seed(FakeBackend::new()).await
}

async fn seed(backend: FakeBackend) -> FakeBackend {
    backend.set_block_size(4);
    let root = backend.root().await.expect("root").link_id;
    let docs = backend
        .create_folder(&root, "docs", UNIX_EPOCH)
        .await
        .expect("mkdir");
    backend
        .upload_new(
            &docs,
            "a.txt",
            UploadSource::Bytes(Bytes::from_static(b"hello world")),
            meta(),
        )
        .await
        .expect("upload");
    backend
        .upload_new(
            &root,
            "top.bin",
            UploadSource::Bytes(Bytes::from_static(b"0123456789")),
            meta(),
        )
        .await
        .expect("upload");
    backend
}

/// 2026-10-22 10:00:00 UTC.
pub(super) const CLOCK_SECS: u64 = 1_792_663_200;

/// A device whose id is made from its name, so two names are two devices.
pub(super) fn test_device(name: &str) -> DeviceIdentity {
    let mut id = [0u8; 16];
    for (slot, byte) in id.iter_mut().zip(name.bytes()) {
        *slot = byte;
    }
    DeviceIdentity {
        id: crate::device::Uuid::from_bytes(id),
        name: name.to_owned(),
    }
}

pub(super) fn fixed_clock() -> Clock {
    Arc::new(|| UNIX_EPOCH + Duration::from_secs(CLOCK_SECS))
}

fn config() -> Config {
    let mut config = Config::default();
    config.upload.debounce = HumanDuration(Duration::from_millis(20));
    config
}

async fn open_with(
    backend: &FakeBackend,
    dir: &std::path::Path,
    config: Config,
) -> (Arc<Engine>, mpsc::UnboundedReceiver<Invalidation>) {
    open_device(backend, dir, config, "dev-a").await
}

/// An engine of the device called `name`, with the fixed clock.
async fn open_device(
    backend: &FakeBackend,
    dir: &std::path::Path,
    config: Config,
    name: &str,
) -> (Arc<Engine>, mpsc::UnboundedReceiver<Invalidation>) {
    let mut options = EngineOptions::in_dir(dir, config, test_device(name), fixed_clock());
    options.cache.disk_bytes = 1 << 20;
    Engine::open(Arc::new(backend.clone()), options)
        .await
        .expect("engine")
}

async fn open(
    backend: &FakeBackend,
    dir: &std::path::Path,
) -> (Arc<Engine>, mpsc::UnboundedReceiver<Invalidation>) {
    open_with(backend, dir, config()).await
}

/// An engine with its executor running.
async fn running(backend: &FakeBackend, dir: &std::path::Path) -> Arc<Engine> {
    let (engine, _inval) = open(backend, dir).await;
    engine.start_background();
    engine
}

async fn settle(engine: &Engine) {
    let counts = engine.drain(Duration::from_secs(10)).await;
    assert_eq!(counts.pending, 0, "the queue did not drain: {counts:?}");
    assert_eq!(counts.parked, 0, "parked: {:?}", engine.parked());
}

fn found(lookup: Lookup) -> Attr {
    match lookup {
        Lookup::Found(attr) => attr,
        other => panic!("expected an entry, got {other:?}"),
    }
}

const WRITE: OpenMode = OpenMode {
    read: false,
    write: true,
    truncate: false,
};
const READ: OpenMode = OpenMode {
    read: true,
    write: false,
    truncate: false,
};

/// Writes a whole new file and closes it.
fn write_file(engine: &Engine, parent: Ino, name: &str, data: &[u8]) -> Ino {
    let (attr, handle) = engine
        .create(parent, name.as_bytes(), 0o644, true, WRITE)
        .expect("create");
    engine.write_local(handle, 0, data).expect("write");
    engine.release_file(handle);
    attr.ino
}

async fn read_file(engine: &Arc<Engine>, ino: Ino) -> Vec<u8> {
    let (handle, _) = engine.open_file(ino, READ).expect("open");
    let size = engine.getattr(ino).expect("attr").size;
    let data = engine.read(handle, 0, size as u32 + 1).await.expect("read");
    engine.release_file(handle);
    data.into_vec()
}

#[tokio::test]
async fn listing_lookup_and_read_through_the_cache() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let (engine, _inval) = open(&backend, dir.path()).await;
    assert!(matches!(
        hot(|| engine.lookup(ROOT_INO, b"docs")),
        Err(CoreError::NeedsListing(ROOT_INO))
    ));
    engine.ensure_listed(ROOT_INO).await.expect("list");
    let docs = found(hot(|| engine.lookup(ROOT_INO, b"docs")).expect("lookup"));
    assert_eq!(docs.kind, NodeKind::Folder);
    engine.ensure_listed(docs.ino).await.expect("list docs");
    let file = found(hot(|| engine.lookup(docs.ino, b"a.txt")).expect("lookup"));
    assert_eq!((file.size, file.perm), (11, 0o600));
    assert_eq!(
        hot(|| engine.lookup(docs.ino, b"nope")).expect("lookup"),
        Lookup::Negative
    );

    let (handle, keep) = engine.open_file(file.ino, READ).expect("open");
    assert!(!keep);
    assert!(
        hot(|| engine.read_local(handle, 0, 100))
            .expect("local")
            .is_none()
    );
    assert_eq!(engine.read(handle, 3, 6).await.expect("read"), b"lo wor");
    assert_eq!(
        hot(|| engine.read_local(handle, 0, 100))
            .expect("local")
            .as_deref(),
        Some(&b"hello world"[..])
    );
    assert_eq!(
        hot(|| engine.read_local(handle, 11, 5))
            .expect("eof")
            .as_deref(),
        Some(&b""[..])
    );
    engine.release_file(handle);
    let (_, keep) = engine.open_file(file.ino, READ).expect("reopen");
    assert!(keep, "same content keeps the page cache");
    assert_eq!(
        engine
            .metrics()
            .hot_path_network_calls
            .load(Ordering::Relaxed),
        0
    );
}

#[tokio::test]
async fn a_remount_serves_the_stored_tree_with_the_same_inodes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let first = {
        let (engine, _inval) = open(&backend, dir.path()).await;
        engine.ensure_listed(ROOT_INO).await.expect("list");
        let attr = found(engine.lookup(ROOT_INO, b"top.bin").expect("lookup"));
        engine.shutdown().await.expect("shutdown");
        attr
    };
    let (engine, _inval) = open(&backend, dir.path()).await;
    let calls = engine.metrics().backend_calls.load(Ordering::Relaxed);
    let again = found(hot(|| engine.lookup(ROOT_INO, b"top.bin")).expect("lookup"));
    assert_eq!(again, first);
    assert_eq!(
        engine.metrics().backend_calls.load(Ordering::Relaxed),
        calls
    );
}

#[tokio::test]
async fn revalidation_applies_remote_changes_and_invalidates() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let (engine, mut inval) = open(&backend, dir.path()).await;
    engine.ensure_listed(ROOT_INO).await.expect("list");
    let root = backend.root().await.expect("root").link_id;
    backend
        .upload_new(
            &root,
            "new.txt",
            UploadSource::Bytes(Bytes::from_static(b"n")),
            meta(),
        )
        .await
        .expect("upload");
    engine.revalidate().await.expect("revalidate");
    found(hot(|| engine.lookup(ROOT_INO, b"new.txt")).expect("lookup"));
    assert_eq!(
        inval.try_recv().expect("invalidation"),
        Invalidation::Entry {
            parent: ROOT_INO,
            name: "new.txt".into()
        }
    );
}

#[tokio::test]
async fn inode_numbers_are_never_reused_across_reopen() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let reserved = {
        let (engine, _inval) = open(&backend, dir.path()).await;
        engine.ensure_listed(ROOT_INO).await.expect("list");
        let reserved = engine.inos.lock().reserved;
        engine.shutdown().await.expect("shutdown");
        reserved
    };
    let (engine, _inval) = open(&backend, dir.path()).await;
    assert!(engine.inos.lock().next > reserved);
}

#[tokio::test]
async fn a_state_database_of_another_account_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    drop(open(&backend, dir.path()).await);
    let store = Store::open(&dir.path().join("state.db")).expect("store");
    store
        .write_durable(|conn| meta_set(conn, "root_link", "someone-else"))
        .await
        .expect("write");
    drop(store);
    let result = Engine::open(
        Arc::new(backend),
        EngineOptions::in_dir(
            dir.path(),
            Config::default(),
            test_device("dev-a"),
            fixed_clock(),
        ),
    )
    .await;
    assert!(
        matches!(result, Err(CoreError::Store(message)) if message.contains("another account"))
    );
}

#[tokio::test]
async fn prefetch_is_paced_instead_of_bursting() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = FakeBackend::new();
    let root = backend.root().await.expect("root").link_id;
    for i in 0..20 {
        backend
            .upload_new(
                &root,
                &format!("f{i}"),
                UploadSource::Bytes(Bytes::from_static(b"x")),
                meta(),
            )
            .await
            .expect("upload");
    }
    let (engine, _inval) = open(&backend, dir.path()).await;
    engine.ensure_listed(ROOT_INO).await.expect("list");
    let before = engine.metrics().backend_calls.load(Ordering::Relaxed);
    // The gate runs on tokio time: paused, the count does not depend on the
    // machine's load.
    tokio::time::pause();
    engine.opendir(ROOT_INO).expect("opendir");
    tokio::time::sleep(Duration::from_millis(1200)).await;
    // Turns start at 0, 0.5 and 1.0 s; each prefetches one file with two
    // calls (block list, block). Unpaced, all 20 files would be done.
    let calls = engine.metrics().backend_calls.load(Ordering::Relaxed) - before;
    assert_eq!(calls, 6, "backend calls in 1.2 s");
}

#[tokio::test]
async fn sequential_reads_trigger_readahead() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let (engine, _inval) = open(&backend, dir.path()).await;
    engine.ensure_listed(ROOT_INO).await.expect("list");
    let file = found(engine.lookup(ROOT_INO, b"top.bin").expect("lookup"));
    let (handle, _) = engine.open_file(file.ino, READ).expect("open");
    engine.read(handle, 0, 2).await.expect("read");
    engine.read(handle, 2, 2).await.expect("read");
    eventually("readahead", || {
        engine.metrics().prefetched_blocks.load(Ordering::Relaxed) > 0
    })
    .await;
}

// ---- writes ----

#[tokio::test]
async fn new_files_and_folders_upload_in_dependency_order() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let engine = running(&backend, dir.path()).await;
    engine.ensure_listed(ROOT_INO).await.expect("list");
    let a = engine.mkdir(ROOT_INO, b"a", 0o750).expect("mkdir a");
    let b = engine.mkdir(a.ino, b"b", 0o755).expect("mkdir b");
    let file = write_file(&engine, b.ino, "f.txt", b"nested content");
    assert_eq!(engine.getattr(file).expect("attr").size, 14);
    settle(&engine).await;
    assert_eq!(
        fake_tree(&backend)
            .await
            .get("/a/b/f.txt")
            .map(|f| f.content.as_slice()),
        Some(&b"nested content"[..])
    );
    // Clean now, and served from the seeded cache without downloading.
    let calls = engine.metrics().cache_misses.load(Ordering::Relaxed);
    assert_eq!(read_file(&engine, file).await, b"nested content");
    assert_eq!(engine.metrics().cache_misses.load(Ordering::Relaxed), calls);
    assert_eq!(
        engine
            .metrics()
            .hot_path_network_calls
            .load(Ordering::Relaxed),
        0
    );
}

#[tokio::test]
async fn temporary_files_never_reach_the_server() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let engine = running(&backend, dir.path()).await;
    engine.ensure_listed(ROOT_INO).await.expect("list");
    // An rsync temp renamed into place, and a lock file removed at once.
    write_file(&engine, ROOT_INO, ".new.txt.XyZ123", b"final");
    engine
        .rename(
            ROOT_INO,
            b".new.txt.XyZ123",
            ROOT_INO,
            b"new.txt",
            RenameMode::Replace,
        )
        .expect("rename");
    write_file(&engine, ROOT_INO, "index.lock", b"lock");
    engine.unlink(ROOT_INO, b"index.lock").expect("unlink");
    settle(&engine).await;
    let names: Vec<String> = backend
        .changes()
        .into_iter()
        .filter_map(|change| match change {
            FakeChange::CreatedFile { name, .. } | FakeChange::Renamed { name, .. } => Some(name),
            _ => None,
        })
        .collect();
    assert_eq!(names, ["a.txt", "top.bin", "new.txt"]);
}

#[tokio::test]
async fn an_atomic_save_becomes_a_revision_of_the_target() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let engine = running(&backend, dir.path()).await;
    engine.ensure_listed(ROOT_INO).await.expect("list");
    let top = found(engine.lookup(ROOT_INO, b"top.bin").expect("lookup")).ino;
    let link = engine
        .tree
        .read()
        .get(top)
        .and_then(|n| n.link().cloned())
        .expect("link");
    write_file(&engine, ROOT_INO, ".top.bin.tmp", b"replaced!");
    engine
        .rename(
            ROOT_INO,
            b".top.bin.tmp",
            ROOT_INO,
            b"top.bin",
            RenameMode::Replace,
        )
        .expect("rename over");
    settle(&engine).await;
    assert_eq!(
        backend.revision_count(&link),
        2,
        "a new revision of the same node"
    );
    assert_eq!(backend.content(&link).as_deref(), Some(&b"replaced!"[..]));
    assert!(
        !backend
            .changes()
            .iter()
            .any(|c| matches!(c, FakeChange::Trashed { .. }))
    );
}

#[tokio::test]
async fn partial_overwrites_materialize_the_base_block() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let engine = running(&backend, dir.path()).await;
    engine.ensure_listed(ROOT_INO).await.expect("list");
    let file = found(engine.lookup(ROOT_INO, b"top.bin").expect("lookup")).ino;
    let (handle, _) = engine.open_file(file, WRITE).expect("open");
    assert!(matches!(
        engine.write_local(handle, 3, b"ab"),
        Err(CoreError::NeedsMaterialize(_))
    ));
    engine
        .write(handle, 3, b"ab".to_vec())
        .await
        .expect("slow write");
    engine.write_local(handle, 10, b"++").expect("append");
    engine.release_file(handle);
    assert_eq!(read_file(&engine, file).await, b"012ab56789++");
    settle(&engine).await;
    let tree = fake_tree(&backend).await;
    assert_eq!(tree["/top.bin"].content, b"012ab56789++");
}

/// A write lands while a staged read downloads its base: the read plans
/// again instead of failing with EIO.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_staged_read_survives_a_write_while_it_fetches_the_base() {
    use crate::staging::GRID;
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = FakeBackend::new();
    let root = backend.root().await.expect("root").link_id;
    let content: Vec<u8> = (0..3 * GRID).map(|i| (i % 251) as u8).collect();
    backend
        .upload_new(
            &root,
            "big.bin",
            UploadSource::Bytes(Bytes::from(content.clone())),
            meta(),
        )
        .await
        .expect("upload");
    let engine = running(&backend, dir.path()).await;
    engine.ensure_listed(ROOT_INO).await.expect("list");
    let file = found(engine.lookup(ROOT_INO, b"big.bin").expect("lookup")).ino;
    let (writer, _) = engine.open_file(file, WRITE).expect("open");
    // A whole block written at the end: the file is staged on its base.
    let tail = vec![b'z'; GRID as usize];
    engine.write_local(writer, 2 * GRID, &tail).expect("stage");
    let (reader, _) = engine.open_file(file, READ).expect("open");

    // The read spans the first block boundary: one base piece at GRID - 10.
    backend.set_latency(Duration::from_millis(400));
    let read = {
        let engine = Arc::clone(&engine);
        tokio::spawn(async move { engine.read(reader, GRID - 10, 20).await })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    // Block 0 becomes staging data: the base piece now starts at GRID.
    let head = vec![b'a'; GRID as usize];
    engine.write_local(writer, 0, &head).expect("write");
    let data = read.await.expect("task").expect("read");
    backend.set_latency(Duration::ZERO);

    let mut expected = vec![b'a'; 10];
    expected.extend_from_slice(&content[GRID as usize..GRID as usize + 10]);
    assert_eq!(&data[..], &expected[..]);
    engine.release_file(reader);
    engine.release_file(writer);
}

/// Uploads commit while writes and truncations keep coming: none of them is
/// lost, and the size always matches the content (fsx found a write that
/// landed between an upload's commit and its unstaging).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn writes_racing_upload_commits_are_kept() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let mut config = config();
    config.upload.debounce = HumanDuration(Duration::from_millis(1));
    config.upload.max_dirty_age = HumanDuration(Duration::from_millis(1));
    let (engine, _inval) = open_with(&backend, dir.path(), config).await;
    engine.start_background();
    engine.ensure_listed(ROOT_INO).await.expect("list");
    let file = write_file(&engine, ROOT_INO, "racy.bin", b"");
    let (handle, _) = engine.open_file(file, WRITE).expect("open");
    let writer = Arc::clone(&engine);
    let expected = tokio::spawn(async move {
        let mut expected = Vec::new();
        for round in 0..4000u32 {
            if round % 500 == 499 {
                let size = expected.len() as u64 / 2;
                let change = SetAttr {
                    size: Some(size),
                    handle: Some(handle),
                    ..SetAttr::default()
                };
                writer.setattr_slow(file, change).await.expect("truncate");
                expected.truncate(size as usize);
            } else {
                let chunk = round.to_le_bytes();
                writer
                    .write(handle, expected.len() as u64, chunk.to_vec())
                    .await
                    .expect("write");
                expected.extend_from_slice(&chunk);
            }
            let size = writer.getattr(file).expect("attr").size;
            assert_eq!(size, expected.len() as u64, "round {round}");
            if round % 4 == 0 {
                // Room for the uploads to start.
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        }
        expected
    })
    .await
    .expect("writer");
    engine.release_file(handle);
    assert_eq!(read_file(&engine, file).await, expected);
    settle(&engine).await;
    assert_eq!(read_file(&engine, file).await, expected);
    assert_eq!(fake_tree(&backend).await["/racy.bin"].content, expected);
    assert!(engine.metrics().uploads.load(Ordering::Relaxed) > 1);
}

#[tokio::test]
async fn truncation_and_metadata_changes_upload() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let engine = running(&backend, dir.path()).await;
    engine.ensure_listed(ROOT_INO).await.expect("list");
    let file = found(engine.lookup(ROOT_INO, b"top.bin").expect("lookup")).ino;
    let cut = SetAttr {
        size: Some(6),
        ..SetAttr::default()
    };
    assert!(matches!(
        engine.setattr(file, &cut),
        Err(CoreError::NeedsMaterialize(_))
    ));
    engine.setattr_slow(file, cut).await.expect("truncate");
    let mtime = UNIX_EPOCH + Duration::from_secs(1_234_567_890);
    engine
        .setattr(
            file,
            &SetAttr {
                mode: Some(0o100_751),
                mtime: Some(mtime),
                ..SetAttr::default()
            },
        )
        .expect("chmod and touch");
    settle(&engine).await;
    let tree = fake_tree(&backend).await;
    let top = &tree["/top.bin"];
    assert_eq!(top.content, b"012345");
    assert_eq!((top.mode, top.mtime), (Some(0o751), Some(mtime)));

    // A metadata-only change of a clean file is a new revision too.
    engine
        .setattr(
            file,
            &SetAttr {
                mode: Some(0o644),
                ..SetAttr::default()
            },
        )
        .expect("chmod");
    settle(&engine).await;
    assert_eq!(fake_tree(&backend).await["/top.bin"].mode, Some(0o644));
}

#[tokio::test]
async fn deletions_go_to_the_trash() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let engine = running(&backend, dir.path()).await;
    engine.ensure_listed(ROOT_INO).await.expect("list");
    let docs = found(engine.lookup(ROOT_INO, b"docs").expect("lookup")).ino;
    assert!(matches!(
        engine.rmdir(ROOT_INO, b"docs"),
        Err(CoreError::NeedsListing(_))
    ));
    engine.ensure_listed(docs).await.expect("list docs");
    assert!(matches!(
        engine.rmdir(ROOT_INO, b"docs"),
        Err(CoreError::NotEmpty)
    ));
    engine.unlink(docs, b"a.txt").expect("unlink");
    engine.rmdir(ROOT_INO, b"docs").expect("rmdir");
    settle(&engine).await;
    let tree = fake_tree(&backend).await;
    assert!(!tree.contains_key("/docs") && !tree.contains_key("/docs/a.txt"));
    assert!(tree.contains_key("/top.bin"));
}

#[tokio::test]
async fn swapped_names_are_resolved_through_a_temporary_name() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let engine = running(&backend, dir.path()).await;
    engine.ensure_listed(ROOT_INO).await.expect("list");
    write_file(&engine, ROOT_INO, "b.bin", b"bee");
    settle(&engine).await;
    // Hold the executor back so both renames are pending together.
    let mv = |from: &[u8], to: &[u8]| {
        engine
            .rename(ROOT_INO, from, ROOT_INO, to, RenameMode::Replace)
            .expect("rename")
    };
    mv(b"top.bin", b"t");
    mv(b"b.bin", b"top.bin");
    mv(b"t", b"b.bin");
    settle(&engine).await;
    let tree = fake_tree(&backend).await;
    assert_eq!(tree["/top.bin"].content, b"bee");
    assert_eq!(tree["/b.bin"].content, b"0123456789");
    assert!(!tree.keys().any(|path| path.contains("pdmount-swap")));
}

#[tokio::test]
async fn unuploaded_changes_survive_a_crash() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    {
        // No executor: the change is only local when the "daemon" dies.
        let (engine, _inval) = open(&backend, dir.path()).await;
        engine.ensure_listed(ROOT_INO).await.expect("list");
        write_file(&engine, ROOT_INO, "pending.txt", b"survives");
        engine.store.flush().await.expect("commit");
    }
    let engine = running(&backend, dir.path()).await;
    let file = found(hot(|| engine.lookup(ROOT_INO, b"pending.txt")).expect("lookup")).ino;
    assert_eq!(read_file(&engine, file).await, b"survives");
    settle(&engine).await;
    assert_eq!(
        fake_tree(&backend).await["/pending.txt"].content,
        b"survives"
    );
}

/// A kill -9 loses the store writes still queued. Staging the stored rows
/// name must outlive it: after an upload and after an unlink.
#[tokio::test]
async fn staging_outlives_a_crash_before_its_rows_are_committed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let staging = |ino: Ino| dir.path().join("staging").join(ino.to_string());
    let (uploaded, unlinked) = {
        let engine = running(&backend, dir.path()).await;
        engine.ensure_listed(ROOT_INO).await.expect("list");
        let uploaded = write_file(&engine, ROOT_INO, "uploaded.txt", b"uploaded");
        engine.store.flush().await.expect("commit");
        engine.store.crash();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !fake_tree(&backend).await.contains_key("/uploaded.txt") {
            assert!(Instant::now() < deadline, "the upload did not happen");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        settle(&engine).await;
        assert!(
            staging(uploaded).exists(),
            "staging deleted after the upload"
        );
        let _ = engine.shutdown().await;
        // No executor this time: the unlinked file never uploads.
        let (engine, _inval) = open(&backend, dir.path()).await;
        engine.reserve_inos(16).await.expect("inos");
        let unlinked = write_file(&engine, ROOT_INO, "unlinked.txt", b"unlinked");
        engine.store.flush().await.expect("commit");
        engine.store.crash();
        engine.unlink(ROOT_INO, b"unlinked.txt").expect("unlink");
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            staging(unlinked).exists(),
            "staging deleted after the unlink"
        );
        (uploaded, unlinked)
    };
    let engine = running(&backend, dir.path()).await;
    assert_eq!(read_file(&engine, uploaded).await, b"uploaded");
    assert_eq!(read_file(&engine, unlinked).await, b"unlinked");
    settle(&engine).await;
    let tree = fake_tree(&backend).await;
    assert_eq!(tree["/uploaded.txt"].content, b"uploaded");
    assert_eq!(tree["/unlinked.txt"].content, b"unlinked");
    let created = backend
        .changes()
        .iter()
        .filter(|c| matches!(c, FakeChange::CreatedFile { name, .. } if name == "uploaded.txt"))
        .count();
    assert_eq!(created, 1, "the recorded upload was repeated");
}

#[tokio::test]
async fn an_upload_that_reached_the_server_before_a_crash_is_adopted() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let engine = running(&backend, dir.path()).await;
    engine.ensure_listed(ROOT_INO).await.expect("list");
    // The same content already exists remotely under the name, as if an
    // earlier upload of it succeeded but was never recorded.
    let root = backend.root().await.expect("root").link_id;
    backend
        .upload_new(
            &root,
            "again.txt",
            UploadSource::Bytes(Bytes::from_static(b"same")),
            meta(),
        )
        .await
        .expect("upload");
    write_file(&engine, ROOT_INO, "again.txt", b"same");
    settle(&engine).await;
    let created = backend
        .changes()
        .iter()
        .filter(|c| matches!(c, FakeChange::CreatedFile { name, .. } if name == "again.txt"))
        .count();
    assert_eq!(created, 1);
}

#[tokio::test]
async fn quota_errors_park_the_upload_and_are_reported_once() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    // The server refuses, although the local pre-check passed.
    backend.inject_failure(
        pdmount_backend::fake::FakeOp::UploadNew,
        pdmount_backend::BackendError::QuotaExceeded,
    );
    let engine = running(&backend, dir.path()).await;
    engine.ensure_listed(ROOT_INO).await.expect("list");
    let (attr, handle) = engine
        .create(ROOT_INO, b"big", 0o644, true, WRITE)
        .expect("create");
    engine.write_local(handle, 0, &[1; 64]).expect("write");
    engine.release_file(handle);
    let counts = engine.drain(Duration::from_secs(10)).await;
    assert_eq!((counts.pending, counts.parked), (0, 1));
    let (handle, _) = engine.open_file(attr.ino, WRITE).expect("open");
    assert!(matches!(
        engine.flush(handle),
        Err(CoreError::Errno(nix::libc::ENOSPC))
    ));
    assert!(engine.flush(handle).is_ok(), "reported once");
    engine.release_file(handle);
}

/// fsync reports a deferred error once, and the cleared error is on disk
/// when it returns: a crash right after does not report it again.
#[tokio::test]
async fn fsync_reports_a_deferred_error_once_across_a_crash() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    backend.inject_failure(
        pdmount_backend::fake::FakeOp::UploadNew,
        pdmount_backend::BackendError::QuotaExceeded,
    );
    let ino = {
        let engine = running(&backend, dir.path()).await;
        engine.ensure_listed(ROOT_INO).await.expect("list");
        let (attr, handle) = engine
            .create(ROOT_INO, b"big", 0o644, true, WRITE)
            .expect("create");
        engine.write_local(handle, 0, &[1; 64]).expect("write");
        engine.release_file(handle);
        let counts = engine.drain(Duration::from_secs(10)).await;
        assert_eq!(counts.parked, 1);
        assert!(matches!(
            engine.fsync(attr.ino).await,
            Err(CoreError::Errno(nix::libc::ENOSPC))
        ));
        engine.store.crash();
        let _ = engine.shutdown().await;
        attr.ino
    };
    let (engine, _inval) = open(&backend, dir.path()).await;
    assert!(engine.fsync(ino).await.is_ok(), "reported once");
}

#[tokio::test]
async fn an_open_unlinked_file_stays_readable() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let engine = running(&backend, dir.path()).await;
    engine.ensure_listed(ROOT_INO).await.expect("list");
    let file = found(engine.lookup(ROOT_INO, b"top.bin").expect("lookup")).ino;
    let (handle, _) = engine.open_file(file, READ).expect("open");
    engine.unlink(ROOT_INO, b"top.bin").expect("unlink");
    assert_eq!(
        engine.read(handle, 0, 100).await.expect("read"),
        b"0123456789"
    );
    engine.release_file(handle);
    engine.forget(file, 1);
    settle(&engine).await;
    assert!(!fake_tree(&backend).await.contains_key("/top.bin"));
}

#[tokio::test]
async fn read_all_helper_matches_reads() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let (engine, _inval) = open(&backend, dir.path()).await;
    engine.ensure_listed(ROOT_INO).await.expect("list");
    let file = found(engine.lookup(ROOT_INO, b"top.bin").expect("lookup")).ino;
    assert_eq!(read_all(&engine, file).await, b"0123456789");
}

#[tokio::test]
async fn a_read_over_more_blocks_than_pool_slots_holds_few() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    // 400 blocks of 4 bytes against 79 slots of the smallest pool class.
    let content: Vec<u8> = (0..1600u32).map(|i| (i % 251) as u8).collect();
    let root = backend.root().await.expect("root").link_id;
    backend
        .upload_new(
            &root,
            "many.bin",
            UploadSource::Bytes(Bytes::from(content.clone())),
            meta(),
        )
        .await
        .expect("upload");
    let (engine, _inval) = open(&backend, dir.path()).await;
    engine.ensure_listed(ROOT_INO).await.expect("list");
    let file = found(engine.lookup(ROOT_INO, b"many.bin").expect("lookup")).ino;
    // Holding every block would make each download past the 79th wait 5 s
    // for a slot.
    let started = std::time::Instant::now();
    let read = tokio::time::timeout(Duration::from_secs(5), read_file(&engine, file));
    assert_eq!(read.await.expect("the read waited for slots"), content);
    let (handle, _) = engine.open_file(file, READ).expect("open");
    let local = hot(|| engine.read_local(handle, 0, 1600)).expect("local");
    assert_eq!(local.as_deref(), Some(&content[..]));
    engine.release_file(handle);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(
        engine
            .metrics()
            .cache_pool_fallbacks
            .load(Ordering::Relaxed),
        0
    );
}

#[tokio::test]
async fn writes_beyond_the_quota_fail_at_once() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    backend.set_quota(100);
    let engine = running(&backend, dir.path()).await;
    engine.refresh_storage().await;
    engine.ensure_listed(ROOT_INO).await.expect("list");
    let (_, handle) = engine
        .create(ROOT_INO, b"f", 0o644, true, WRITE)
        .expect("create");
    engine.write_local(handle, 0, &[0; 50]).expect("fits");
    assert!(matches!(
        engine.write_local(handle, 50, &[0; 50]),
        Err(CoreError::NoSpace)
    ));
    // Overwriting within the file needs no new space.
    engine.write_local(handle, 0, &[1; 50]).expect("overwrite");
    engine.release_file(handle);
}

#[tokio::test]
async fn pdignore_keeps_new_entries_local() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    {
        let engine = running(&backend, dir.path()).await;
        engine.ensure_listed(ROOT_INO).await.expect("list");
        write_file(&engine, ROOT_INO, ".pdignore", b"*.log\nbuild/\n");
        // The rules are reloaded in the background.
        eventually("the ignore rules", || {
            engine.is_ignored(ROOT_INO, "x.log", false)
        })
        .await;
        write_file(&engine, ROOT_INO, "debug.log", b"noise");
        let build = engine.mkdir(ROOT_INO, b"build", 0o755).expect("mkdir");
        write_file(&engine, build.ino, "out.o", b"object");
        write_file(&engine, ROOT_INO, "keep.log", b"renamed later");
        engine
            .rename(
                ROOT_INO,
                b"keep.log",
                ROOT_INO,
                b"keep.txt",
                RenameMode::Replace,
            )
            .expect("rename to a synced name");
        settle(&engine).await;
        let tree = fake_tree(&backend).await;
        assert!(
            tree.contains_key("/.pdignore"),
            "the rules file itself syncs"
        );
        assert!(tree.contains_key("/keep.txt"));
        assert!(!tree.contains_key("/debug.log"));
        assert!(!tree.contains_key("/build"));
        engine.shutdown().await.expect("shutdown");
    }
    // Local-only content survives a remount and still never uploads.
    let engine = running(&backend, dir.path()).await;
    let log = found(hot(|| engine.lookup(ROOT_INO, b"debug.log")).expect("lookup")).ino;
    assert_eq!(read_file(&engine, log).await, b"noise");
    settle(&engine).await;
    assert!(!fake_tree(&backend).await.contains_key("/debug.log"));
}

// ---- access rules (§11, A1–A6) ----

fn is_read_only<T: std::fmt::Debug>(result: Result<T>) -> bool {
    matches!(result, Err(CoreError::ReadOnly))
}

#[tokio::test]
async fn read_only_subtrees_refuse_every_change() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let engine = running(&backend, dir.path()).await;
    engine.ensure_listed(ROOT_INO).await.expect("list");
    let docs = found(engine.lookup(ROOT_INO, b"docs").expect("lookup")).ino;
    engine.ensure_listed(docs).await.expect("list docs");
    let file = found(engine.lookup(docs, b"a.txt").expect("lookup")).ino;
    let sub = engine.mkdir(docs, b"sub", 0o755).expect("mkdir");
    engine
        .access_set("/docs", AccessMode::Ro, RuleSource::Cli)
        .await
        .expect("set");

    // A1: everything inside is refused, and no write bits are shown.
    assert!(is_read_only(
        engine.create(docs, b"new", 0o644, true, WRITE)
    ));
    assert!(is_read_only(engine.mkdir(docs, b"d", 0o755)));
    assert!(is_read_only(engine.open_file(file, WRITE)));
    assert!(is_read_only(engine.unlink(docs, b"a.txt")));
    assert!(is_read_only(engine.rmdir(docs, b"sub")));
    assert!(is_read_only(engine.rename(
        docs,
        b"a.txt",
        ROOT_INO,
        b"out",
        RenameMode::Replace
    )));
    assert!(is_read_only(engine.rename(
        ROOT_INO,
        b"top.bin",
        docs,
        b"in",
        RenameMode::Replace
    )));
    let chmod = SetAttr {
        mode: Some(0o777),
        ..SetAttr::default()
    };
    assert!(is_read_only(engine.setattr(file, &chmod)));
    assert_eq!(engine.getattr(file).expect("attr").perm, 0o400);
    assert!(engine.open_file(file, READ).is_ok(), "reads still work");

    // A2: a rw rule below the ro one wins inside it.
    engine
        .access_set("/docs/sub", AccessMode::Rw, RuleSource::Cli)
        .await
        .expect("set rw");
    write_file(&engine, sub.ino, "ok.txt", b"allowed");
    assert!(is_read_only(engine.create(
        docs,
        b"still-not",
        0o644,
        true,
        WRITE
    )));

    // A4: list and effective.
    let (rules, default) = engine.access_list(None).await.expect("list");
    let shown: Vec<(AccessMode, Option<&str>)> =
        rules.iter().map(|r| (r.mode, r.path.as_deref())).collect();
    assert_eq!(
        shown,
        [
            (AccessMode::Ro, Some("/docs")),
            (AccessMode::Rw, Some("/docs/sub"))
        ]
    );
    assert_eq!(default, AccessMode::Rw);
    assert_eq!(
        engine
            .access_effective("/docs/sub/ok.txt")
            .await
            .expect("effective"),
        (AccessMode::Rw, Some("/docs/sub".into()))
    );
    assert_eq!(
        engine
            .access_effective("/top.bin")
            .await
            .expect("effective"),
        (AccessMode::Rw, None)
    );

    // A3: unset at runtime restores everything, original modes included.
    assert!(engine.access_unset("/docs").await.expect("unset"));
    assert_eq!(engine.getattr(file).expect("attr").perm, 0o600);
    engine.unlink(docs, b"a.txt").expect("allowed again");
    settle(&engine).await;
    assert!(fake_tree(&backend).await.contains_key("/docs/sub/ok.txt"));
}

#[tokio::test]
async fn rules_follow_renames_and_orphan_on_deletion() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let engine = running(&backend, dir.path()).await;
    engine.ensure_listed(ROOT_INO).await.expect("list");
    let a = engine.mkdir(ROOT_INO, b"a", 0o755).expect("mkdir");
    engine.mkdir(ROOT_INO, b"b", 0o755).expect("mkdir");
    engine
        .access_set("/a", AccessMode::Ro, RuleSource::Cli)
        .await
        .expect("set");
    engine
        .access_set("/b", AccessMode::Ro, RuleSource::Cli)
        .await
        .expect("set");
    engine
        .rename(ROOT_INO, b"a", ROOT_INO, b"a2", RenameMode::Replace)
        .expect("rename");
    engine.rmdir(ROOT_INO, b"b").expect("rmdir");
    let (rules, _) = engine.access_list(None).await.expect("list");
    let shown: Vec<(Option<&str>, &str)> = rules
        .iter()
        .map(|r| (r.path.as_deref(), r.path_at_set.as_str()))
        .collect();
    assert_eq!(shown, [(Some("/a2"), "/a"), (None, "/b")]);
    assert!(is_read_only(engine.mkdir(a.ino, b"x", 0o755)));
    // A new entry with the old name does not inherit the orphaned rule.
    let b2 = engine.mkdir(ROOT_INO, b"b", 0o755).expect("mkdir again");
    engine.mkdir(b2.ino, b"x", 0o755).expect("not read-only");
    assert!(engine.access_unset("/b").await.expect("unset orphan"));
}

#[tokio::test]
async fn content_written_before_a_switch_to_ro_still_uploads() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let engine = running(&backend, dir.path()).await;
    engine.ensure_listed(ROOT_INO).await.expect("list");
    let d = engine.mkdir(ROOT_INO, b"d", 0o755).expect("mkdir");
    let (_, handle) = engine
        .create(d.ino, b"f", 0o644, true, WRITE)
        .expect("create");
    engine.write_local(handle, 0, b"first").expect("write");
    engine
        .access_set("/d", AccessMode::Ro, RuleSource::Cli)
        .await
        .expect("set");
    assert!(is_read_only(engine.write_local(handle, 5, b"second")));
    engine.release_file(handle);
    settle(&engine).await;
    assert_eq!(fake_tree(&backend).await["/d/f"].content, b"first");
}

#[tokio::test]
async fn the_owner_bits_decide_access() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let engine = running(&backend, dir.path()).await;
    engine.ensure_listed(ROOT_INO).await.expect("list");
    let d = engine.mkdir(ROOT_INO, b"d", 0o755).expect("mkdir");
    let file = write_file(&engine, d.ino, "f", b"x");
    let mode = |m| SetAttr {
        mode: Some(m),
        ..SetAttr::default()
    };
    // A 0444 file (a git object) cannot be opened for writing...
    engine.setattr(file, &mode(0o444)).expect("chmod");
    assert!(matches!(
        engine.open_file(file, WRITE),
        Err(CoreError::AccessDenied)
    ));
    assert!(engine.open_file(file, READ).is_ok());
    // ...but can be removed, which needs write access to the directory.
    engine.setattr(d.ino, &mode(0o555)).expect("chmod dir");
    assert!(matches!(
        engine.unlink(d.ino, b"f"),
        Err(CoreError::AccessDenied)
    ));
    assert!(matches!(
        engine.access(d.ino, MAY_WRITE),
        Err(CoreError::AccessDenied)
    ));
    engine.setattr(d.ino, &mode(0o755)).expect("chmod dir back");
    engine.unlink(d.ino, b"f").expect("unlink");
    // Without search permission nothing inside can be looked up.
    engine.setattr(d.ino, &mode(0o644)).expect("chmod -x");
    assert!(matches!(
        engine.lookup(d.ino, b"anything"),
        Err(CoreError::AccessDenied)
    ));
    // In a ro subtree, write access is EROFS even where the bits allow it.
    engine.setattr(d.ino, &mode(0o755)).expect("chmod back");
    engine
        .access_set("/d", AccessMode::Ro, RuleSource::Cli)
        .await
        .expect("ro");
    assert!(is_read_only(engine.access(d.ino, MAY_WRITE)));
    assert!(engine.access(d.ino, MAY_READ | MAY_EXEC).is_ok());
}

// ---- event-based sync (§9, D033, D034) ----

async fn remote_child(
    backend: &FakeBackend,
    parent: &LinkId,
    name: &str,
) -> pdmount_backend::RemoteNode {
    backend
        .list_children(parent)
        .await
        .expect("list")
        .into_iter()
        .find(|node| node.name == name)
        .unwrap_or_else(|| panic!("no remote {name}"))
}

fn listings(engine: &Engine) -> u64 {
    engine.metrics().listings.load(Ordering::Relaxed)
}

/// An engine over `seeded()` with the root and `docs` listed.
async fn listed(
    backend: &FakeBackend,
    dir: &std::path::Path,
) -> (Arc<Engine>, mpsc::UnboundedReceiver<Invalidation>, Ino) {
    let (engine, inval) = open(backend, dir).await;
    engine.ensure_listed(ROOT_INO).await.expect("list root");
    let docs = found(hot(|| engine.lookup(ROOT_INO, b"docs")).expect("lookup")).ino;
    engine.ensure_listed(docs).await.expect("list docs");
    (engine, inval, docs)
}

#[tokio::test]
async fn remote_changes_arrive_through_events() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let (engine, mut inval, docs) = listed(&backend, dir.path()).await;
    let root = backend.root().await.expect("root").link_id;
    let docs_link = remote_child(&backend, &root, "docs").await.link_id;
    let a = remote_child(&backend, &docs_link, "a.txt").await;
    let top = remote_child(&backend, &root, "top.bin").await;
    let a_ino = found(hot(|| engine.lookup(docs, b"a.txt")).expect("lookup")).ino;
    let top_ino = found(hot(|| engine.lookup(ROOT_INO, b"top.bin")).expect("lookup")).ino;

    // Another device: a new file, a rename with a new revision, a move.
    let source = |data: &'static [u8]| UploadSource::Bytes(Bytes::from_static(data));
    backend
        .upload_new(&root, "new.txt", source(b"n"), meta())
        .await
        .expect("upload");
    backend
        .rename(&a.link_id, None, "b.txt", None)
        .await
        .expect("rename");
    backend
        .upload_revision(
            &a.link_id,
            a.active_revision.as_ref().expect("rev"),
            source(b"changed"),
            meta(),
        )
        .await
        .expect("revision");
    backend
        .rename(&top.link_id, Some(&docs_link), "top.bin", None)
        .await
        .expect("move");
    while inval.try_recv().is_ok() {}

    engine.sync_events().await.expect("events");
    found(hot(|| engine.lookup(ROOT_INO, b"new.txt")).expect("lookup"));
    assert_eq!(
        hot(|| engine.lookup(docs, b"a.txt")).expect("lookup"),
        Lookup::Negative
    );
    let b = found(hot(|| engine.lookup(docs, b"b.txt")).expect("lookup"));
    assert_eq!((b.ino, b.size), (a_ino, 7));
    assert_eq!(read_file(&engine, b.ino).await, b"changed");
    assert_eq!(
        hot(|| engine.lookup(ROOT_INO, b"top.bin")).expect("lookup"),
        Lookup::Negative
    );
    let moved = found(hot(|| engine.lookup(docs, b"top.bin")).expect("lookup"));
    assert_eq!(moved.ino, top_ino);
    let sent: Vec<Invalidation> = std::iter::from_fn(|| inval.try_recv().ok()).collect();
    assert!(sent.contains(&Invalidation::Entry {
        parent: ROOT_INO,
        name: "new.txt".into()
    }));
    assert!(sent.contains(&Invalidation::Inode { ino: a_ino }));

    // Nothing left: polling again lists nothing.
    let before = listings(&engine);
    engine.sync_events().await.expect("events");
    assert_eq!(listings(&engine), before);
}

#[tokio::test]
async fn a_remote_trash_removes_the_entry() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let (engine, mut inval, _docs) = listed(&backend, dir.path()).await;
    let root = backend.root().await.expect("root").link_id;
    let top = remote_child(&backend, &root, "top.bin").await;
    let top_ino = found(hot(|| engine.lookup(ROOT_INO, b"top.bin")).expect("lookup")).ino;
    backend.trash(&[top.link_id]).await.expect("trash");
    while inval.try_recv().is_ok() {}
    engine.sync_events().await.expect("events");
    assert_eq!(
        hot(|| engine.lookup(ROOT_INO, b"top.bin")).expect("lookup"),
        Lookup::Negative
    );
    let sent: Vec<Invalidation> = std::iter::from_fn(|| inval.try_recv().ok()).collect();
    assert!(sent.contains(&Invalidation::Delete {
        parent: ROOT_INO,
        child: top_ino,
        name: "top.bin".into()
    }));
}

#[tokio::test]
async fn own_changes_come_back_as_echoes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let engine = running(&backend, dir.path()).await;
    engine.ensure_listed(ROOT_INO).await.expect("list");
    engine.sync_events().await.expect("events");
    let file = write_file(&engine, ROOT_INO, "mine.txt", b"mine");
    let folder = engine.mkdir(ROOT_INO, b"sub", 0o755).expect("mkdir").ino;
    engine
        .rename(
            ROOT_INO,
            b"top.bin",
            folder,
            b"top.bin",
            RenameMode::Replace,
        )
        .expect("rename");
    engine.unlink(ROOT_INO, b"mine.txt").expect("unlink");
    let _ = file;
    settle(&engine).await;
    let before = listings(&engine);
    engine.sync_events().await.expect("events");
    assert_eq!(listings(&engine), before, "echoes must not list anything");
    found(hot(|| engine.lookup(folder, b"top.bin")).expect("lookup"));
}

#[tokio::test]
async fn events_keep_local_changes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let (engine, _inval, docs) = listed(&backend, dir.path()).await;
    let root = backend.root().await.expect("root").link_id;
    let docs_link = remote_child(&backend, &root, "docs").await.link_id;
    let a = remote_child(&backend, &docs_link, "a.txt").await;
    let a_ino = found(hot(|| engine.lookup(docs, b"a.txt")).expect("lookup")).ino;
    let (handle, _) = engine
        .open_file(
            a_ino,
            OpenMode {
                read: false,
                write: true,
                truncate: true,
            },
        )
        .expect("open");
    engine.write_local(handle, 0, b"local").expect("write");
    engine.release_file(handle);

    // Another device writes the same file, then trashes it.
    backend
        .upload_revision(
            &a.link_id,
            a.active_revision.as_ref().expect("rev"),
            UploadSource::Bytes(Bytes::from_static(b"remote")),
            meta(),
        )
        .await
        .expect("revision");
    engine.sync_events().await.expect("events");
    assert_eq!(read_file(&engine, a_ino).await, b"local");
    backend.trash(&[a.link_id]).await.expect("trash");
    engine.sync_events().await.expect("events");
    let kept = found(hot(|| engine.lookup(docs, b"a.txt")).expect("lookup"));
    assert_eq!(kept.ino, a_ino);
    assert_eq!(read_file(&engine, a_ino).await, b"local");
}

#[tokio::test]
async fn a_directory_moved_between_listed_directories_keeps_its_listing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let (engine, _inval, docs) = listed(&backend, dir.path()).await;
    let root = backend.root().await.expect("root").link_id;
    let docs_link = remote_child(&backend, &root, "docs").await.link_id;
    let dst = backend
        .create_folder(&root, "dst", UNIX_EPOCH)
        .await
        .expect("mkdir");
    engine.sync_events().await.expect("events");
    let dst_ino = found(hot(|| engine.lookup(ROOT_INO, b"dst")).expect("lookup")).ino;
    engine.ensure_listed(dst_ino).await.expect("list dst");
    backend
        .rename(&docs_link, Some(&dst), "docs", None)
        .await
        .expect("move");
    engine.sync_events().await.expect("events");
    assert_eq!(
        hot(|| engine.lookup(ROOT_INO, b"docs")).expect("lookup"),
        Lookup::Negative
    );
    assert_eq!(
        found(hot(|| engine.lookup(dst_ino, b"docs")).expect("lookup")).ino,
        docs
    );
    // Still listed: answered from RAM.
    found(hot(|| engine.lookup(docs, b"a.txt")).expect("lookup"));
}

#[tokio::test]
async fn pages_are_drained_and_the_cursor_survives_a_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    backend.set_event_page_size(1);
    let root = backend.root().await.expect("root").link_id;
    {
        let (engine, _inval, _docs) = listed(&backend, dir.path()).await;
        for name in ["one", "two", "three"] {
            backend
                .upload_new(
                    &root,
                    name,
                    UploadSource::Bytes(Bytes::from_static(b"x")),
                    meta(),
                )
                .await
                .expect("upload");
        }
        engine.sync_events().await.expect("events");
        for name in ["one", "two", "three"] {
            found(hot(|| engine.lookup(ROOT_INO, name.as_bytes())).expect("lookup"));
        }
        engine.shutdown().await.expect("shutdown");
    }
    let (engine, _inval) = open(&backend, dir.path()).await;
    assert_eq!(
        engine.event_cursor.lock().await.clone(),
        Some(backend.latest_event_id().await.expect("latest"))
    );
    engine.sync_events().await.expect("events");
    assert_eq!(listings(&engine), 0, "nothing new since the stored cursor");
}

#[tokio::test]
async fn a_refresh_lists_again_and_adopts_a_new_cursor() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let (engine, _inval, _docs) = listed(&backend, dir.path()).await;
    let root = backend.root().await.expect("root").link_id;
    backend
        .upload_new(
            &root,
            "new.txt",
            UploadSource::Bytes(Bytes::from_static(b"n")),
            meta(),
        )
        .await
        .expect("upload");
    backend.request_refresh();
    let before = listings(&engine);
    engine.sync_events().await.expect("events");
    refreshed(&engine).await;
    assert_eq!(listings(&engine) - before, 2, "root and docs listed again");
    found(hot(|| engine.lookup(ROOT_INO, b"new.txt")).expect("lookup"));
    let latest = backend.latest_event_id().await.expect("latest");
    assert_eq!(
        engine.event_cursor.lock().await.clone(),
        Some(latest.clone())
    );
    assert_eq!(stored_cursor(&engine).await, Some(latest));
}

/// Waits for a refresh to finish.
async fn refreshed(engine: &Engine) {
    eventually_within("the refresh", Duration::from_secs(20), || {
        !engine.events_refreshing()
    })
    .await;
}

async fn stored_cursor(engine: &Engine) -> Option<EventId> {
    engine.store.flush().await.expect("flush");
    crate::events::load_cursor(&engine.store).expect("cursor")
}

/// D054: a listing requested after a remote change shows it, even when a
/// listing fetched before the change is still in flight (B2: the event
/// stream would store its cursor past a change it never applied).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
// Holding the tree while the remote changes is what makes the listing old.
#[allow(clippy::await_holding_lock)]
async fn a_listing_request_is_not_answered_by_an_older_listing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let (engine, _inval) = open(&backend, dir.path()).await;
    engine.ensure_listed(ROOT_INO).await.expect("list");
    let docs = found(engine.lookup(ROOT_INO, b"docs").expect("lookup")).ino;
    engine.ensure_listed(docs).await.expect("list docs");
    let docs_link = remote_of(&engine, docs).expect("remote").link;

    // An older listing takes its snapshot after the fake's latency, then
    // waits for the tree to apply it. While the tree is held, this thread
    // waits with std sleeps: the blocked listing may hold up the worker
    // that drives tokio's timers.
    backend.set_latency(Duration::from_millis(300));
    let older = {
        let engine = Arc::clone(&engine);
        tokio::spawn(async move { engine.list(docs).await })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    let tree = engine.tree.write();
    std::thread::sleep(Duration::from_millis(400));
    remote_file(&backend, &docs_link, "new.txt", b"after").await;
    // Asked after the change: it joins the older listing first.
    let fresh = {
        let engine = Arc::clone(&engine);
        tokio::spawn(async move { engine.list(docs).await })
    };
    std::thread::sleep(Duration::from_millis(200));
    drop(tree);
    older.await.expect("task").expect("older listing");
    fresh.await.expect("task").expect("fresh listing");
    backend.set_latency(Duration::ZERO);
    assert!(
        matches!(
            hot(|| engine.lookup(docs, b"new.txt")).expect("lookup"),
            Lookup::Found(_)
        ),
        "the change made before the request is shown"
    );
}

#[tokio::test]
async fn events_flow_while_a_refresh_lists_again() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let (engine, _inval, docs) = listed(&backend, dir.path()).await;
    let root = backend.root().await.expect("root").link_id;
    let docs_link = remote_child(&backend, &root, "docs").await.link_id;
    let old = stored_cursor(&engine).await;
    // The refresh's first listing stops; it holds the background turn, so
    // nothing else reaches the server meanwhile.
    let pause = backend.pause_at(FakePoint::ListBeforeRead);
    backend.request_refresh();
    engine.sync_events().await.expect("events");
    pause.reached().await;
    assert!(engine.events_refreshing());
    assert_eq!(
        stored_cursor(&engine).await,
        old,
        "no cursor is stored mid-refresh"
    );
    backend
        .upload_new(
            &docs_link,
            "during.txt",
            UploadSource::Bytes(Bytes::from_static(b"d")),
            meta(),
        )
        .await
        .expect("upload");
    // An event pass that holds the cursor lock before the refresh ends runs
    // mid-refresh: the refresh ends only under that lock.
    let events = tokio::spawn({
        let engine = Arc::clone(&engine);
        async move { engine.sync_events().await }
    });
    while engine.event_cursor.try_lock().is_ok() {
        tokio::task::yield_now().await;
    }
    pause.release();
    events.await.expect("task").expect("events");
    found(hot(|| engine.lookup(docs, b"during.txt")).expect("lookup"));
    // Read first: still refreshing afterwards means nothing was stored yet.
    let stored = stored_cursor(&engine).await;
    if engine.events_refreshing() {
        assert_eq!(stored, old, "no cursor is stored mid-refresh");
    }
    refreshed(&engine).await;
    assert_eq!(
        stored_cursor(&engine).await,
        Some(backend.latest_event_id().await.expect("latest"))
    );
}

#[tokio::test]
async fn atime_is_kept_locally_and_survives_a_remount() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let atime = UNIX_EPOCH + Duration::from_secs(1_900_000_000);
    let mtime = UNIX_EPOCH + Duration::from_secs(1_950_000_000);
    let ino = {
        let (engine, _inval) = open(&backend, dir.path()).await;
        engine.ensure_listed(ROOT_INO).await.expect("list");
        let ino = write_file(&engine, ROOT_INO, "times", b"t");
        let attr = engine.getattr(ino).expect("attr");
        assert_eq!(attr.atime, attr.mtime, "an unset atime follows mtime");
        let change = SetAttr {
            atime: Some(atime),
            mtime: Some(mtime),
            ..SetAttr::default()
        };
        let attr = engine.setattr(ino, &change).expect("utimensat");
        assert_eq!((attr.atime, attr.mtime), (atime, mtime));
        engine.shutdown().await.expect("shutdown");
        ino
    };
    let (engine, _inval) = open(&backend, dir.path()).await;
    let attr = engine.getattr(ino).expect("attr");
    assert_eq!((attr.atime, attr.mtime), (atime, mtime));
}

#[tokio::test]
async fn ftruncate_through_a_writable_handle_ignores_the_mode() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let (engine, _inval) = open(&backend, dir.path()).await;
    engine.ensure_listed(ROOT_INO).await.expect("list");
    // open(O_CREAT | O_RDWR, 0) hands the creator a writable handle.
    let (attr, handle) = engine
        .create(ROOT_INO, b"mode0", 0, true, WRITE)
        .expect("create");
    engine.write_local(handle, 0, b"data").expect("write");
    let truncate = SetAttr {
        size: Some(0),
        ..SetAttr::default()
    };
    assert!(
        matches!(
            engine.setattr(attr.ino, &truncate),
            Err(CoreError::AccessDenied)
        ),
        "truncate(2) by path checks the mode"
    );
    let ftruncate = SetAttr {
        handle: Some(handle),
        ..truncate
    };
    assert_eq!(
        engine
            .setattr(attr.ino, &ftruncate)
            .expect("ftruncate")
            .size,
        0
    );
    engine.release_file(handle);
}

#[tokio::test]
async fn an_unlinked_open_file_has_no_links() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let (engine, _inval) = open(&backend, dir.path()).await;
    engine.ensure_listed(ROOT_INO).await.expect("list");
    let ino = write_file(&engine, ROOT_INO, "gone", b"x");
    let (handle, _) = engine.open_file(ino, READ).expect("open");
    assert_eq!(engine.getattr(ino).expect("attr").nlink, 1);
    engine.unlink(ROOT_INO, b"gone").expect("unlink");
    assert_eq!(engine.getattr(ino).expect("fstat").nlink, 0);
    engine.release_file(handle);
}

#[test]
fn the_test_clock_is_the_documented_time() {
    let at = chrono::DateTime::<chrono::Utc>::from(UNIX_EPOCH + Duration::from_secs(CLOCK_SECS));
    assert_eq!(at.to_rfc3339(), "2026-10-22T10:00:00+00:00");
}

#[tokio::test]
async fn the_tree_names_conflict_copies_after_the_engine_device() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let (engine, _inval) = open_device(&backend, dir.path(), config(), "laptop").await;
    assert_eq!(engine.device().name, "laptop");
    let tree = engine.tree.read();
    let first = tree.pick_conflict_name(ROOT_INO, "top.bin", &|_| false);
    assert_eq!(first, "top (conflict laptop 2026-10-22 10-00-00 UTC).bin");
    // A taken name moves on to the next attempt.
    let second = tree.pick_conflict_name(ROOT_INO, "top.bin", &|name| name == first);
    assert_eq!(
        second,
        "top (conflict laptop 2026-10-22 10-00-00 UTC 2).bin"
    );
}

fn note(kind: ConflictKind, ino: Ino, name: &str) -> ConflictNote {
    ConflictNote {
        link: Some(LinkId::new("l1")),
        conflict_name: Some(format!("{name} (copy)")),
        local_rev: Some(RevisionId("r1".to_owned())),
        remote_rev: Some(RevisionId("r2".to_owned())),
        detail: Some("why".to_owned()),
        ..ConflictNote::new(kind, ino, name.to_owned())
    }
}

#[tokio::test]
async fn conflicts_round_trip_and_ids_never_repeat() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let (engine, _inval) = open_device(&backend, dir.path(), config(), "laptop").await;
    let mut notices = engine.subscribe_conflicts();
    engine.ensure_listed(ROOT_INO).await.expect("list");
    let ino = write_file(&engine, ROOT_INO, "a.txt", b"x");
    let first = engine.record_conflict(note(ConflictKind::EditEdit, ino, "a.txt"));
    let second = engine.record_conflict(ConflictNote {
        other_ino: Some(ino + 1),
        ..note(ConflictKind::RenameRename, ino, "a.txt")
    });
    assert_eq!((first, second), (1, 2));
    assert!(engine.set_conflict_state(first, ConflictState::Resolved));
    assert!(!engine.set_conflict_state(99, ConflictState::Resolved));
    let notice = notices.try_recv().expect("notice");
    assert_eq!(
        (notice.id, notice.kind, notice.path.as_deref()),
        (1, ConflictKind::EditEdit, Some("/a.txt"))
    );
    assert_eq!(notices.try_recv().expect("second").id, 2);
    assert_eq!(engine.conflict_count(), 1);
    assert_eq!(engine.conflicts_for(ino + 1).len(), 1);
    engine.shutdown().await.expect("shutdown");
    drop(engine);

    let (engine, _inval) = open_device(&backend, dir.path(), config(), "laptop").await;
    let all = engine.conflicts();
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].state, ConflictState::Resolved);
    assert_eq!(all[1].kind, ConflictKind::RenameRename);
    assert_eq!(all[1].state, ConflictState::Notice);
    assert_eq!(all[1].other_ino, Some(ino + 1));
    assert_eq!(all[1].local_device, "laptop");
    assert_eq!(all[1].created_at, CLOCK_SECS as i64);
    assert_eq!(all[1].link, Some(LinkId::new("l1")));
    assert_eq!(all[1].detail.as_deref(), Some("why"));
    let third = engine.record_conflict(note(ConflictKind::NameTaken, ino, "a.txt"));
    assert_eq!(third, 3);
}

/// §10.8: the xattrs mark both copies, also the other copy that the conflict
/// knows only by its link.
#[tokio::test]
async fn the_other_copy_known_by_its_link_is_marked() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let engine = running(&backend, dir.path()).await;
    engine.ensure_listed(ROOT_INO).await.expect("list");
    let other = found(engine.lookup(ROOT_INO, b"top.bin").expect("lookup")).ino;
    let link = remote_of(&engine, other).expect("remote").link;
    let copy = write_file(&engine, ROOT_INO, "top (copy).bin", b"mine");
    engine.record_conflict(ConflictNote {
        link: Some(link),
        ..note(ConflictKind::CreateCreate, copy, "top.bin")
    });
    let xattr = |ino| engine.xattr(ino, "user.pdmount.state").expect("xattr");
    assert_eq!(xattr(copy).as_deref(), Some("conflict"));
    assert_eq!(xattr(other).as_deref(), Some("conflict"));
    let pointer = engine
        .xattr(other, "user.pdmount.conflict")
        .expect("xattr")
        .expect("set");
    assert!(pointer.contains("top (copy).bin"), "{pointer}");
    let shown = engine.conflict_show("/top.bin").await.expect("show");
    assert_eq!(
        (shown.path.as_deref(), shown.other_path.as_deref()),
        (Some("/top (copy).bin"), Some("/top.bin"))
    );
}

// ---- listing rules of the conflict engine (§10.5, D040) ----

/// A config whose uploads wait for `drain`: nothing is sent unasked.
fn lazy_config() -> Config {
    let mut config = Config::default();
    config.upload.debounce = HumanDuration(Duration::from_secs(60));
    config
}

/// `listed()` with the root and `docs` listed, uploads held back.
async fn listed_lazy(backend: &FakeBackend, dir: &std::path::Path) -> (Arc<Engine>, Ino, LinkId) {
    let (engine, _inval) = open_with(backend, dir, lazy_config()).await;
    engine.ensure_listed(ROOT_INO).await.expect("list root");
    let docs = found(hot(|| engine.lookup(ROOT_INO, b"docs")).expect("lookup")).ino;
    engine.ensure_listed(docs).await.expect("list docs");
    let root = backend.root().await.expect("root").link_id;
    let docs_link = remote_child(backend, &root, "docs").await.link_id;
    (engine, docs, docs_link)
}

/// Another device's new revision of `link`.
async fn remote_revision(backend: &FakeBackend, link: &LinkId, data: &'static [u8]) {
    let node = backend
        .get_nodes(std::slice::from_ref(link))
        .await
        .expect("get")
        .remove(0)
        .expect("node");
    backend
        .upload_revision(
            link,
            node.active_revision.as_ref().expect("rev"),
            UploadSource::Bytes(Bytes::from_static(data)),
            meta(),
        )
        .await
        .expect("revision");
}

/// Overwrites a whole file through the engine and closes it.
fn overwrite(engine: &Engine, ino: Ino, data: &[u8]) {
    let (handle, _) = engine
        .open_file(
            ino,
            OpenMode {
                read: false,
                write: true,
                truncate: true,
            },
        )
        .expect("open");
    engine.write_local(handle, 0, data).expect("write");
    engine.release_file(handle);
}

fn kinds(engine: &Engine) -> Vec<ConflictKind> {
    engine.conflicts().iter().map(|c| c.kind).collect()
}

fn remote_of(engine: &Engine, ino: Ino) -> Option<RemoteState> {
    engine.tree.read().get(ino).and_then(|n| n.remote.clone())
}

#[tokio::test]
async fn m3_remote_delete_vs_local_edit_resurrects_the_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let (engine, docs, docs_link) = listed_lazy(&backend, dir.path()).await;
    let a = remote_child(&backend, &docs_link, "a.txt").await;
    let a_ino = found(hot(|| engine.lookup(docs, b"a.txt")).expect("lookup")).ino;
    overwrite(&engine, a_ino, b"local edit");

    backend
        .trash(std::slice::from_ref(&a.link_id))
        .await
        .expect("trash");
    engine.sync_events().await.expect("events");
    let kept = found(hot(|| engine.lookup(docs, b"a.txt")).expect("lookup"));
    assert_eq!(kept.ino, a_ino);
    assert!(remote_of(&engine, a_ino).is_none(), "created again later");
    assert_eq!(kinds(&engine), [ConflictKind::RemoteDeleteLocalEdit]);
    assert_eq!(read_file(&engine, a_ino).await, b"local edit");

    engine.start_background();
    settle(&engine).await;
    let tree = fake_tree(&backend).await;
    assert_eq!(tree["/docs/a.txt"].content, b"local edit");
    let old = backend
        .get_nodes(&[a.link_id])
        .await
        .expect("get")
        .remove(0)
        .expect("node");
    assert!(old.trashed, "the old link stays in the trash");
    assert_eq!(read_file(&engine, a_ino).await, b"local edit");
}

#[tokio::test]
async fn m6_remote_folder_delete_resurrects_a_minimal_chain() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let root = backend.root().await.expect("root").link_id;
    let docs_link = remote_child(&backend, &root, "docs").await.link_id;
    let sub = backend
        .create_folder(&docs_link, "sub", UNIX_EPOCH)
        .await
        .expect("sub");
    let deep = backend
        .create_folder(&sub, "deep", UNIX_EPOCH)
        .await
        .expect("deep");
    backend
        .upload_new(
            &sub,
            "clean.txt",
            UploadSource::Bytes(Bytes::from_static(b"clean")),
            meta(),
        )
        .await
        .expect("upload");
    let _ = deep;

    let (engine, mut inval) = open_with(&backend, dir.path(), lazy_config()).await;
    engine.ensure_listed(ROOT_INO).await.expect("list root");
    let docs = found(hot(|| engine.lookup(ROOT_INO, b"docs")).expect("lookup")).ino;
    engine.ensure_listed(docs).await.expect("list docs");
    let sub_ino = found(hot(|| engine.lookup(docs, b"sub")).expect("lookup")).ino;
    engine.ensure_listed(sub_ino).await.expect("list sub");
    let deep_ino = found(hot(|| engine.lookup(sub_ino, b"deep")).expect("lookup")).ino;
    engine.ensure_listed(deep_ino).await.expect("list deep");
    let a_ino = found(hot(|| engine.lookup(docs, b"a.txt")).expect("lookup")).ino;
    let clean_ino = found(hot(|| engine.lookup(sub_ino, b"clean.txt")).expect("lookup")).ino;
    write_file(&engine, docs, "new.txt", b"new");
    let x = write_file(&engine, deep_ino, "x", b"deep content");
    while inval.try_recv().is_ok() {}

    backend.trash(&[docs_link]).await.expect("trash");
    engine.sync_events().await.expect("events");

    let sent: Vec<Invalidation> = std::iter::from_fn(|| inval.try_recv().ok()).collect();
    for (parent, child, name) in [(docs, a_ino, "a.txt"), (sub_ino, clean_ino, "clean.txt")] {
        assert!(
            sent.contains(&Invalidation::Delete {
                parent,
                child,
                name: name.into()
            }),
            "{name} should be removed: {sent:?}"
        );
    }
    {
        let tree = engine.tree.read();
        assert!(tree.get(a_ino).is_none_or(|n| !n.attached));
        assert!(tree.get(clean_ino).is_none_or(|n| !n.attached));
        for ino in [docs, sub_ino, deep_ino, x] {
            let node = tree.get(ino).expect("kept");
            assert!(
                node.attached && node.remote.is_none(),
                "{ino} is created again"
            );
        }
        tree.check_invariants().expect("consistent");
    }
    assert_eq!(kinds(&engine), [ConflictKind::RemoteDeleteLocalEdit]);

    engine.start_background();
    settle(&engine).await;
    let tree = fake_tree(&backend).await;
    let paths: Vec<&str> = tree.keys().map(String::as_str).collect();
    assert_eq!(
        paths,
        [
            "/docs",
            "/docs/new.txt",
            "/docs/sub",
            "/docs/sub/deep",
            "/docs/sub/deep/x",
            "/top.bin"
        ]
    );
    assert_eq!(tree["/docs/sub/deep/x"].content, b"deep content");
}

#[tokio::test]
async fn remote_delete_wins_over_a_local_clean_rename() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let (engine, docs, docs_link) = listed_lazy(&backend, dir.path()).await;
    let a = remote_child(&backend, &docs_link, "a.txt").await;
    let a_ino = found(hot(|| engine.lookup(docs, b"a.txt")).expect("lookup")).ino;
    engine
        .rename(docs, b"a.txt", docs, b"renamed.txt", RenameMode::Replace)
        .expect("rename");

    backend.trash(&[a.link_id]).await.expect("trash");
    engine.sync_events().await.expect("events");
    for name in [&b"a.txt"[..], b"renamed.txt"] {
        assert_eq!(
            hot(|| engine.lookup(docs, name)).expect("lookup"),
            Lookup::Negative
        );
    }
    let all = engine.conflicts();
    assert_eq!(kinds(&engine), [ConflictKind::RemoteDeleteLocalRename]);
    assert_eq!(all[0].ino, a_ino);
    assert_eq!(all[0].orig_name, "renamed.txt");
    engine.start_background();
    settle(&engine).await;
    assert!(!fake_tree(&backend).await.contains_key("/docs/renamed.txt"));
}

#[tokio::test]
async fn rename_rename_first_committer_wins_at_listing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    // The executor is not started: the local rename stays pending.
    let (engine, docs, docs_link) = listed_lazy(&backend, dir.path()).await;
    let a = remote_child(&backend, &docs_link, "a.txt").await;
    let a_ino = found(hot(|| engine.lookup(docs, b"a.txt")).expect("lookup")).ino;
    engine
        .rename(docs, b"a.txt", docs, b"mine.txt", RenameMode::Replace)
        .expect("rename");

    backend
        .rename(&a.link_id, None, "theirs.txt", None)
        .await
        .expect("remote rename");
    engine.sync_events().await.expect("events");
    let theirs = found(hot(|| engine.lookup(docs, b"theirs.txt")).expect("lookup"));
    assert_eq!(theirs.ino, a_ino, "the inode follows the remote name");
    assert_eq!(
        hot(|| engine.lookup(docs, b"mine.txt")).expect("lookup"),
        Lookup::Negative
    );
    let all = engine.conflicts();
    assert_eq!(kinds(&engine), [ConflictKind::RenameRename]);
    assert_eq!(all[0].orig_name, "mine.txt");
    assert!(!engine.tree.read().placement_pending(a_ino));
}

#[tokio::test]
async fn layer_one_flags_remote_newer_on_dirty_files() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let (engine, docs, docs_link) = listed_lazy(&backend, dir.path()).await;
    let mut notices = engine.subscribe_conflicts();
    let a = remote_child(&backend, &docs_link, "a.txt").await;
    let a_ino = found(hot(|| engine.lookup(docs, b"a.txt")).expect("lookup")).ino;
    overwrite(&engine, a_ino, b"local edit");
    assert!(!engine.tree.read().get(a_ino).expect("a").remote_newer);

    remote_revision(&backend, &a.link_id, b"remote edit").await;
    engine.sync_events().await.expect("events");
    assert!(engine.tree.read().get(a_ino).expect("a").remote_newer);
    let notice = notices.try_recv().expect("notice");
    assert_eq!((notice.id, notice.kind), (0, ConflictKind::EditEdit));
    assert_eq!(notice.path.as_deref(), Some("/docs/a.txt"));
    assert_eq!(read_file(&engine, a_ino).await, b"local edit");
    assert!(engine.conflicts().is_empty(), "the upload registers it");

    engine.shutdown().await.expect("shutdown");
    drop(engine);
    let (engine, _inval) = open_with(&backend, dir.path(), lazy_config()).await;
    assert!(engine.tree.read().get(a_ino).expect("a").remote_newer);
}

#[tokio::test]
async fn m4_local_delete_vs_remote_edit_revives_at_listing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let (engine, docs, docs_link) = listed_lazy(&backend, dir.path()).await;
    let a = remote_child(&backend, &docs_link, "a.txt").await;
    let a_ino = found(hot(|| engine.lookup(docs, b"a.txt")).expect("lookup")).ino;
    engine.unlink(docs, b"a.txt").expect("unlink");
    assert!(engine.tree.read().get(a_ino).expect("a").deleted);

    remote_revision(&backend, &a.link_id, b"edited elsewhere").await;
    engine.sync_events().await.expect("events");
    let back = found(hot(|| engine.lookup(docs, b"a.txt")).expect("lookup"));
    assert_ne!(back.ino, a_ino, "the link comes back as a new inode");
    assert_eq!(read_file(&engine, back.ino).await, b"edited elsewhere");
    let all = engine.conflicts();
    assert_eq!(kinds(&engine), [ConflictKind::LocalDeleteRemoteEdit]);
    assert_eq!((all[0].ino, all[0].other_ino), (a_ino, Some(back.ino)));
    assert_eq!(all[0].state, ConflictState::Open);
    assert!(engine.tree.read().get(a_ino).is_none_or(|n| !n.deleted));

    // Nothing trashes it now.
    engine.start_background();
    settle(&engine).await;
    assert_eq!(
        fake_tree(&backend).await["/docs/a.txt"].content,
        b"edited elsewhere"
    );
}

#[tokio::test]
async fn a_remote_rename_onto_a_locally_used_name_shows_after_the_clash() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let (engine, docs, docs_link) = listed_lazy(&backend, dir.path()).await;
    let a = remote_child(&backend, &docs_link, "a.txt").await;
    let a_ino = found(hot(|| engine.lookup(docs, b"a.txt")).expect("lookup")).ino;
    let local = write_file(&engine, docs, "b.txt", b"local b");

    backend
        .rename(&a.link_id, None, "b.txt", None)
        .await
        .expect("remote rename");
    engine.sync_events().await.expect("events");
    assert_eq!(
        hot(|| engine.lookup(docs, b"a.txt")).expect("lookup"),
        Lookup::Negative
    );
    let held = found(hot(|| engine.lookup(docs, b"b.txt")).expect("lookup"));
    assert_eq!(held.ino, local, "the local file keeps the name");
    assert_eq!(read_file(&engine, local).await, b"local b");
    assert!(engine.tree.read().get(a_ino).is_none_or(|n| !n.attached));

    // The local file gives way; the remote one appears at the next listing.
    engine
        .rename(docs, b"b.txt", docs, b"c.txt", RenameMode::Replace)
        .expect("rename");
    engine.list(docs).await.expect("list");
    let shown = found(hot(|| engine.lookup(docs, b"b.txt")).expect("lookup"));
    assert_ne!(shown.ino, local);
    assert_eq!(read_file(&engine, shown.ino).await, b"hello world");
    assert_eq!(
        found(hot(|| engine.lookup(docs, b"c.txt")).expect("lookup")).ino,
        local
    );
    engine.tree.read().check_invariants().expect("consistent");
}

// ---- the upload path of the conflict engine (§10.3, §10.4, D040) ----

/// Waits up to 10 s for `check`.
async fn eventually(what: &str, check: impl FnMut() -> bool) {
    eventually_within(what, Duration::from_secs(10), check).await;
}

async fn eventually_within(what: &str, timeout: Duration, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !check() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn client(uid: &str) -> FakeClient {
    FakeClient {
        uid: Some(uid.to_owned()),
        ..FakeClient::default()
    }
}

/// An official app: it claims no SHA-1 and no mode.
fn official_app() -> FakeClient {
    FakeClient {
        uid: Some("app".to_owned()),
        claims_sha1: false,
        claims_mode: false,
    }
}

const CONFLICT_COPY: &str = "a (conflict dev-a 2026-10-22 10-00-00 UTC).txt";

#[tokio::test]
async fn m1_edit_edit_caught_by_the_pre_check() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let (engine, mut inval) = open_with(&backend, dir.path(), lazy_config()).await;
    engine.ensure_listed(ROOT_INO).await.expect("list root");
    let docs = found(hot(|| engine.lookup(ROOT_INO, b"docs")).expect("lookup")).ino;
    engine.ensure_listed(docs).await.expect("list docs");
    let root = backend.root().await.expect("root").link_id;
    let docs_link = remote_child(&backend, &root, "docs").await.link_id;
    let a = remote_child(&backend, &docs_link, "a.txt").await;
    let a_ino = found(hot(|| engine.lookup(docs, b"a.txt")).expect("lookup")).ino;

    // A handle opened before the split keeps writing into the copy.
    let (handle, _) = engine
        .open_file(
            a_ino,
            OpenMode {
                read: true,
                write: true,
                truncate: true,
            },
        )
        .expect("open");
    engine
        .write_local(handle, 0, b"local edit A")
        .expect("write");
    remote_revision(&backend, &a.link_id, b"remote edit B, longer").await;
    while inval.try_recv().is_ok() {}

    engine.start_background();
    engine.journal.force(a_ino);
    eventually("the conflict", || !engine.conflicts().is_empty()).await;
    let mut seen = Vec::new();
    while let Ok(invalidation) = inval.try_recv() {
        seen.push(invalidation);
    }
    for expected in [
        Invalidation::Entry {
            parent: docs,
            name: "a.txt".into(),
        },
        Invalidation::Entry {
            parent: docs,
            name: CONFLICT_COPY.into(),
        },
        Invalidation::Inode { ino: docs },
    ] {
        assert!(seen.contains(&expected), "{expected:?} not in {seen:?}");
    }
    let all = engine.conflicts();
    assert_eq!(kinds(&engine), [ConflictKind::EditEdit]);
    assert_eq!(all[0].state, ConflictState::Open);
    assert_eq!(all[0].ino, a_ino);
    assert_eq!(all[0].orig_name, "a.txt");
    assert_eq!(all[0].conflict_name.as_deref(), Some(CONFLICT_COPY));
    assert_eq!(all[0].link.as_ref(), Some(&a.link_id));

    let server = found(hot(|| engine.lookup(docs, b"a.txt")).expect("lookup"));
    assert_ne!(server.ino, a_ino, "the server's version is a new inode");
    assert_eq!(all[0].other_ino, Some(server.ino));
    let copy = found(hot(|| engine.lookup(docs, CONFLICT_COPY.as_bytes())).expect("lookup"));
    assert_eq!(copy.ino, a_ino, "the edited file keeps its inode");
    assert_eq!(
        read_file(&engine, server.ino).await,
        b"remote edit B, longer"
    );

    engine.write_local(handle, 12, b"!").expect("write on");
    engine.release_file(handle);
    settle(&engine).await;
    let tree = fake_tree(&backend).await;
    assert_eq!(tree["/docs/a.txt"].content, b"remote edit B, longer");
    assert_eq!(
        tree[&format!("/docs/{CONFLICT_COPY}")].content,
        b"local edit A!"
    );
    assert_eq!(read_file(&engine, a_ino).await, b"local edit A!");
    assert_eq!(engine.conflicts().len(), 1, "one conflict, once");
    engine.tree.read().check_invariants().expect("consistent");
}

#[tokio::test]
async fn m1_identical_content_is_never_a_conflict() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let (engine, docs, docs_link) = listed_lazy(&backend, dir.path()).await;
    let a = remote_child(&backend, &docs_link, "a.txt").await;
    let a_ino = found(hot(|| engine.lookup(docs, b"a.txt")).expect("lookup")).ino;
    overwrite(&engine, a_ino, b"the same edit");
    remote_revision(&backend, &a.link_id, b"the same edit").await;

    engine.start_background();
    settle(&engine).await;
    assert!(engine.conflicts().is_empty(), "{:?}", engine.conflicts());
    let tree = fake_tree(&backend).await;
    assert_eq!(tree["/docs/a.txt"].content, b"the same edit");
    assert!(
        tree.keys().all(|path| !path.contains("conflict")),
        "{tree:?}"
    );
    assert_eq!(
        found(hot(|| engine.lookup(docs, b"a.txt")).expect("lookup")).ino,
        a_ino
    );
    assert!(!engine.tree.read().get(a_ino).expect("a").remote_newer);
}

#[tokio::test]
async fn m1_two_engines() {
    let dir_a = tempfile::tempdir().expect("tempdir");
    let dir_b = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let (a, docs_a, _) = listed_lazy(&backend.with_client(client("pdmount-a")), dir_a.path()).await;
    let b_backend = backend.with_client(client("pdmount-b"));
    let (b, _inval) = open_device(&b_backend, dir_b.path(), config(), "dev-b").await;
    b.ensure_listed(ROOT_INO).await.expect("list root");
    let docs_b = found(hot(|| b.lookup(ROOT_INO, b"docs")).expect("lookup")).ino;
    b.ensure_listed(docs_b).await.expect("list docs");
    b.start_background();

    let a_file = found(hot(|| a.lookup(docs_a, b"a.txt")).expect("lookup")).ino;
    let b_file = found(hot(|| b.lookup(docs_b, b"a.txt")).expect("lookup")).ino;
    overwrite(&a, a_file, b"written on A");
    overwrite(&b, b_file, b"written on B!");
    settle(&b).await;

    a.start_background();
    settle(&a).await;
    assert_eq!(kinds(&a), [ConflictKind::EditEdit]);
    assert!(b.conflicts().is_empty(), "the first committer sees nothing");
    let tree = fake_tree(&backend).await;
    assert_eq!(tree["/docs/a.txt"].content, b"written on B!");
    assert_eq!(
        tree[&format!("/docs/{CONFLICT_COPY}")].content,
        b"written on A"
    );

    // Both converge on the same two files.
    b.sync_events().await.expect("events b");
    a.sync_events().await.expect("events a");
    for (engine, docs) in [(&a, docs_a), (&b, docs_b)] {
        let main = found(hot(|| engine.lookup(docs, b"a.txt")).expect("lookup"));
        assert_eq!(read_file(engine, main.ino).await, b"written on B!");
        let copy = found(hot(|| engine.lookup(docs, CONFLICT_COPY.as_bytes())).expect("lookup"));
        assert_eq!(read_file(engine, copy.ino).await, b"written on A");
    }
}

#[tokio::test]
async fn m2_edit_edit_interleaved_at_commit() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let a_backend = backend.with_client(client("pdmount-a"));
    let (engine, docs, docs_link) = listed_lazy(&a_backend, dir.path()).await;
    let a = remote_child(&backend, &docs_link, "a.txt").await;
    let a_ino = found(hot(|| engine.lookup(docs, b"a.txt")).expect("lookup")).ino;
    overwrite(&engine, a_ino, b"local edit A");

    let pause = a_backend.pause_at(FakePoint::RevisionBeforeCommit);
    engine.start_background();
    let draining = {
        let engine = Arc::clone(&engine);
        tokio::spawn(async move { engine.drain(Duration::from_secs(10)).await })
    };
    // The pre-check passed; B commits while A's revision is in flight.
    pause.reached().await;
    remote_revision(&backend, &a.link_id, b"other edit B").await;
    pause.release();
    let counts = draining.await.expect("drain");
    assert_eq!((counts.pending, counts.parked), (0, 0), "{counts:?}");

    assert_eq!(kinds(&engine), [ConflictKind::EditEdit]);
    let tree = fake_tree(&backend).await;
    assert_eq!(tree["/docs/a.txt"].content, b"other edit B");
    assert_eq!(
        tree[&format!("/docs/{CONFLICT_COPY}")].content,
        b"local edit A"
    );
    let server = found(hot(|| engine.lookup(docs, b"a.txt")).expect("lookup"));
    assert_ne!(server.ino, a_ino);
    assert_eq!(read_file(&engine, server.ino).await, b"other edit B");
}

#[tokio::test]
async fn remote_newer_triggers_the_upload_promptly() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let (engine, docs, docs_link) = listed_lazy(&backend, dir.path()).await;
    let a = remote_child(&backend, &docs_link, "a.txt").await;
    let a_ino = found(hot(|| engine.lookup(docs, b"a.txt")).expect("lookup")).ino;
    overwrite(&engine, a_ino, b"local edit");
    remote_revision(&backend, &a.link_id, b"remote edit").await;
    engine.sync_events().await.expect("events");
    assert!(engine.tree.read().get(a_ino).expect("a").remote_newer);

    // The debounce is a minute; nothing forces the upload.
    engine.start_background();
    eventually("the conflict", || !engine.conflicts().is_empty()).await;
    assert_eq!(kinds(&engine), [ConflictKind::EditEdit]);
    settle(&engine).await;
    let tree = fake_tree(&backend).await;
    assert_eq!(tree["/docs/a.txt"].content, b"remote edit");
    assert_eq!(
        tree[&format!("/docs/{CONFLICT_COPY}")].content,
        b"local edit"
    );
}

#[tokio::test]
async fn a_file_trashed_before_its_upload_is_created_again() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let (engine, docs, docs_link) = listed_lazy(&backend, dir.path()).await;
    let a = remote_child(&backend, &docs_link, "a.txt").await;
    let a_ino = found(hot(|| engine.lookup(docs, b"a.txt")).expect("lookup")).ino;
    overwrite(&engine, a_ino, b"local edit");
    // No event reaches the engine: the pre-check finds out.
    backend
        .trash(std::slice::from_ref(&a.link_id))
        .await
        .expect("trash");

    engine.start_background();
    settle(&engine).await;
    assert_eq!(kinds(&engine), [ConflictKind::RemoteDeleteLocalEdit]);
    assert_eq!(
        fake_tree(&backend).await["/docs/a.txt"].content,
        b"local edit"
    );
    assert_eq!(read_file(&engine, a_ino).await, b"local edit");
}

#[tokio::test]
async fn m7_official_app_edit_of_a_clean_file_is_adopted() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let (engine, docs, docs_link) = listed_lazy(&backend, dir.path()).await;
    let a = remote_child(&backend, &docs_link, "a.txt").await;
    let a_ino = found(hot(|| engine.lookup(docs, b"a.txt")).expect("lookup")).ino;
    let app = backend.with_client(official_app());
    app.upload_revision(
        &a.link_id,
        a.active_revision.as_ref().expect("rev"),
        UploadSource::Bytes(Bytes::from_static(b"edited by the app")),
        meta(),
    )
    .await
    .expect("revision");

    engine.sync_events().await.expect("events");
    assert_eq!(read_file(&engine, a_ino).await, b"edited by the app");
    assert!(engine.conflicts().is_empty());
}

#[tokio::test]
async fn m7_official_app_edit_races_a_metadata_revision_without_a_conflict() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let (engine, docs, docs_link) = listed_lazy(&backend, dir.path()).await;
    let a = remote_child(&backend, &docs_link, "a.txt").await;
    let a_ino = found(hot(|| engine.lookup(docs, b"a.txt")).expect("lookup")).ino;
    engine
        .setattr(
            a_ino,
            &SetAttr {
                mode: Some(0o640),
                ..SetAttr::default()
            },
        )
        .expect("chmod");
    // The app's edit is not known here when the metadata revision goes up.
    let app = backend.with_client(official_app());
    app.upload_revision(
        &a.link_id,
        a.active_revision.as_ref().expect("rev"),
        UploadSource::Bytes(Bytes::from_static(b"edited by the app")),
        meta(),
    )
    .await
    .expect("revision");

    engine.start_background();
    settle(&engine).await;
    assert!(engine.conflicts().is_empty(), "{:?}", engine.conflicts());
    let tree = fake_tree(&backend).await;
    assert_eq!(tree["/docs/a.txt"].content, b"edited by the app");
    assert_eq!(tree["/docs/a.txt"].mode, Some(0o640), "the chmod went up");
    assert_eq!(read_file(&engine, a_ino).await, b"edited by the app");
    assert_eq!(engine.getattr(a_ino).expect("attr").perm, 0o640);
}

#[tokio::test]
async fn identical_content_without_a_claimed_sha1_is_probed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let (engine, docs, docs_link) = listed_lazy(&backend, dir.path()).await;
    let a = remote_child(&backend, &docs_link, "a.txt").await;
    let top = remote_child(
        &backend,
        &backend.root().await.expect("root").link_id,
        "top.bin",
    )
    .await;
    let a_ino = found(hot(|| engine.lookup(docs, b"a.txt")).expect("lookup")).ino;
    let top_ino = found(hot(|| engine.lookup(ROOT_INO, b"top.bin")).expect("lookup")).ino;
    let app = backend.with_client(official_app());
    for (node, content) in [(&a, &b"same bytes"[..]), (&top, &b"abcdefghij"[..])] {
        app.upload_revision(
            &node.link_id,
            node.active_revision.as_ref().expect("rev"),
            UploadSource::Bytes(Bytes::copy_from_slice(content)),
            meta(),
        )
        .await
        .expect("revision");
    }
    assert_eq!(
        backend
            .get_nodes(std::slice::from_ref(&a.link_id))
            .await
            .expect("get")[0]
            .as_ref()
            .expect("a")
            .sha1,
        None
    );

    // Same bytes as the app's: no conflict. Same size, other bytes: one.
    overwrite(&engine, a_ino, b"same bytes");
    overwrite(&engine, top_ino, b"ABCDEFGHIJ");
    engine.start_background();
    settle(&engine).await;
    assert_eq!(kinds(&engine), [ConflictKind::EditEdit]);
    let tree = fake_tree(&backend).await;
    assert_eq!(tree["/docs/a.txt"].content, b"same bytes");
    assert_eq!(tree["/top.bin"].content, b"abcdefghij");
    assert_eq!(
        tree["/top (conflict dev-a 2026-10-22 10-00-00 UTC).bin"].content,
        b"ABCDEFGHIJ"
    );
}

// ---- create/create (§10.6, M7) ----

/// Another device's new file `name` in `parent`.
async fn remote_file(backend: &FakeBackend, parent: &LinkId, name: &str, data: &'static [u8]) {
    backend
        .with_client(client("pdmount-b"))
        .upload_new(
            parent,
            name,
            UploadSource::Bytes(Bytes::from_static(data)),
            meta(),
        )
        .await
        .expect("remote file");
}

/// Another device's new folder `name` in `parent`.
async fn remote_folder(backend: &FakeBackend, parent: &LinkId, name: &str) -> LinkId {
    backend
        .with_client(client("pdmount-b"))
        .create_folder(parent, name, UNIX_EPOCH)
        .await
        .expect("remote folder")
}

fn conflict_name(name: &str, ext: &str) -> String {
    format!("{name} (conflict dev-a 2026-10-22 10-00-00 UTC){ext}")
}

#[tokio::test]
async fn m7_same_name_files_on_two_devices_keep_both() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let (engine, docs, docs_link) = listed_lazy(&backend, dir.path()).await;
    let own = write_file(&engine, docs, "new.txt", b"written on A");
    remote_file(&backend, &docs_link, "new.txt", b"written on B").await;

    engine.start_background();
    settle(&engine).await;
    assert_eq!(kinds(&engine), [ConflictKind::CreateCreate]);
    let copy = conflict_name("new", ".txt");
    let tree = fake_tree(&backend).await;
    assert_eq!(tree["/docs/new.txt"].content, b"written on B");
    assert_eq!(tree[&format!("/docs/{copy}")].content, b"written on A");

    let all = engine.conflicts();
    assert_eq!(all[0].state, ConflictState::Open);
    assert_eq!(all[0].ino, own);
    assert_eq!(all[0].orig_name, "new.txt");
    assert_eq!(all[0].conflict_name.as_deref(), Some(copy.as_str()));
    let server = found(hot(|| engine.lookup(docs, b"new.txt")).expect("lookup"));
    assert_ne!(server.ino, own, "the server's file is a new inode");
    assert_eq!(all[0].other_ino, Some(server.ino));
    assert_eq!(read_file(&engine, server.ino).await, b"written on B");
    let mine = found(hot(|| engine.lookup(docs, copy.as_bytes())).expect("lookup"));
    assert_eq!(mine.ino, own, "the local file keeps its inode");
    assert_eq!(read_file(&engine, own).await, b"written on A");
    engine.tree.read().check_invariants().expect("consistent");
}

#[tokio::test]
async fn m7_same_name_identical_files_merge_silently() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let (engine, docs, docs_link) = listed_lazy(&backend, dir.path()).await;
    let own = write_file(&engine, docs, "new.txt", b"the same bytes");
    remote_file(&backend, &docs_link, "new.txt", b"the same bytes").await;
    let remote = remote_child(&backend, &docs_link, "new.txt").await;

    engine.start_background();
    settle(&engine).await;
    assert!(engine.conflicts().is_empty(), "{:?}", engine.conflicts());
    let tree = fake_tree(&backend).await;
    assert!(
        tree.keys().all(|path| !path.contains("conflict")),
        "{tree:?}"
    );
    assert_eq!(tree["/docs/new.txt"].content, b"the same bytes");
    let created = backend
        .changes()
        .iter()
        .filter(|c| matches!(c, FakeChange::CreatedFile { name, .. } if name == "new.txt"))
        .count();
    assert_eq!(created, 1, "nothing was created a second time");
    let attr = found(hot(|| engine.lookup(docs, b"new.txt")).expect("lookup"));
    assert_eq!(attr.ino, own);
    assert_eq!(
        engine.tree.read().get(own).and_then(|n| n.link().cloned()),
        Some(remote.link_id)
    );
    assert_eq!(read_file(&engine, own).await, b"the same bytes");
}

#[tokio::test]
async fn m7_same_name_folders_merge_recursively() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let (engine, docs, docs_link) = listed_lazy(&backend, dir.path()).await;
    // Both devices made proj/ and proj/sub/, with different files in them.
    let proj = engine.mkdir(docs, b"proj", 0o755).expect("mkdir");
    write_file(&engine, proj.ino, "a.txt", b"from A");
    let sub = engine.mkdir(proj.ino, b"sub", 0o755).expect("mkdir sub");
    write_file(&engine, sub.ino, "y.txt", b"y from A");
    let remote_proj = remote_folder(&backend, &docs_link, "proj").await;
    remote_file(&backend, &remote_proj, "b.txt", b"from B").await;
    let remote_sub = remote_folder(&backend, &remote_proj, "sub").await;
    remote_file(&backend, &remote_sub, "x.txt", b"x from B").await;

    engine.start_background();
    settle(&engine).await;
    assert!(engine.conflicts().is_empty(), "{:?}", engine.conflicts());
    let tree = fake_tree(&backend).await;
    for (path, content) in [
        ("/docs/proj/a.txt", &b"from A"[..]),
        ("/docs/proj/b.txt", b"from B"),
        ("/docs/proj/sub/x.txt", b"x from B"),
        ("/docs/proj/sub/y.txt", b"y from A"),
    ] {
        assert_eq!(tree[path].content, content, "{path}");
    }
    assert!(
        tree.keys().all(|path| !path.contains("conflict")),
        "{tree:?}"
    );
    let folders = backend
        .changes()
        .iter()
        .filter(|c| matches!(c, FakeChange::CreatedFolder { name, .. } if name == "proj"))
        .count();
    assert_eq!(folders, 1, "no second proj/");

    // The merged folder shows the other device's files.
    eventually("the merged listing", || {
        matches!(
            hot(|| engine.lookup(proj.ino, b"b.txt")),
            Ok(Lookup::Found(_))
        )
    })
    .await;
    engine.tree.read().check_invariants().expect("consistent");
}

#[tokio::test]
async fn a_folder_and_a_file_with_one_name_keep_both() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let (engine, docs, docs_link) = listed_lazy(&backend, dir.path()).await;
    // A: a folder where B has a file, and a file where B has a folder.
    let folder = engine.mkdir(docs, b"one", 0o755).expect("mkdir");
    write_file(&engine, folder.ino, "inside.txt", b"inside");
    let file = write_file(&engine, docs, "two", b"a file");
    remote_file(&backend, &docs_link, "one", b"B's file").await;
    let remote_two = remote_folder(&backend, &docs_link, "two").await;
    remote_file(&backend, &remote_two, "theirs.txt", b"theirs").await;

    engine.start_background();
    settle(&engine).await;
    let mut seen = kinds(&engine);
    seen.dedup();
    assert_eq!(seen, [ConflictKind::CreateCreate]);
    assert_eq!(engine.conflicts().len(), 2);
    let tree = fake_tree(&backend).await;
    let one = conflict_name("one", "");
    let two = conflict_name("two", "");
    assert_eq!(tree["/docs/one"].content, b"B's file");
    assert_eq!(tree[&format!("/docs/{one}/inside.txt")].content, b"inside");
    assert_eq!(tree[&format!("/docs/{two}")].content, b"a file");
    assert_eq!(tree["/docs/two/theirs.txt"].content, b"theirs");
    let moved = found(hot(|| engine.lookup(docs, one.as_bytes())).expect("lookup"));
    assert_eq!(moved.ino, folder.ino);
    let own = found(hot(|| engine.lookup(docs, two.as_bytes())).expect("lookup"));
    assert_eq!(own.ino, file);
    engine.tree.read().check_invariants().expect("consistent");
}

#[tokio::test]
async fn a_local_only_file_yields_its_name_at_once() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let engine = running(&backend, dir.path()).await;
    engine.ensure_listed(ROOT_INO).await.expect("list");
    write_file(&engine, ROOT_INO, ".pdignore", b"*.log\n");
    eventually("the ignore rules", || {
        engine.is_ignored(ROOT_INO, "x.log", false)
    })
    .await;
    let own = write_file(&engine, ROOT_INO, "debug.log", b"local noise");
    settle(&engine).await;
    let root = backend.root().await.expect("root").link_id;
    remote_file(&backend, &root, "debug.log", b"remote log").await;

    // The next listing shows the remote file; the local one moves aside.
    engine.list(ROOT_INO).await.expect("list");
    let remote = found(hot(|| engine.lookup(ROOT_INO, b"debug.log")).expect("lookup"));
    assert_ne!(remote.ino, own);
    assert_eq!(read_file(&engine, remote.ino).await, b"remote log");
    let name = conflict_name("debug", ".log");
    let local = found(hot(|| engine.lookup(ROOT_INO, name.as_bytes())).expect("lookup"));
    assert_eq!(local.ino, own);
    assert_eq!(read_file(&engine, own).await, b"local noise");
    assert_eq!(kinds(&engine), [ConflictKind::NameTaken]);
    settle(&engine).await;
    assert!(!fake_tree(&backend).await.contains_key(&format!("/{name}")));

    // keep-remote would wait for an upload that never comes, and the copy
    // would never reach the trash: refused at once.
    let error = tokio::time::timeout(
        Duration::from_secs(5),
        engine.conflict_resolve(&format!("/{name}"), Resolution::KeepRemote),
    )
    .await
    .expect("answers at once")
    .expect_err("refused");
    assert!(error.to_string().contains("local-only"), "{error}");
    assert_eq!(read_file(&engine, own).await, b"local noise");
}

#[tokio::test]
async fn a_new_file_in_a_folder_trashed_remotely_is_created_again() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let (engine, docs, docs_link) = listed_lazy(&backend, dir.path()).await;
    write_file(&engine, docs, "new.txt", b"written on A");
    // No event reaches the engine: the upload finds out.
    backend
        .trash(std::slice::from_ref(&docs_link))
        .await
        .expect("trash docs");

    engine.start_background();
    settle(&engine).await;
    let tree = fake_tree(&backend).await;
    assert_eq!(tree["/docs/new.txt"].content, b"written on A");
}

// ---- the namespace executor (§8.3, §8.4, §10.5, D040) ----

/// Runs the journal alone: no event loop, so the executor's own checks
/// are what meets the remote changes.
fn start_journal(engine: &Arc<Engine>) {
    let engine = Arc::clone(engine);
    tokio::spawn(async move { engine.run_journal().await });
}

fn sha1_of(data: &[u8]) -> [u8; 20] {
    use sha1::Digest;
    sha1::Sha1::digest(data).into()
}

async fn is_trashed(backend: &FakeBackend, link: &LinkId) -> bool {
    backend
        .get_nodes(std::slice::from_ref(link))
        .await
        .expect("get")
        .remove(0)
        .is_some_and(|node| node.trashed)
}

#[tokio::test]
async fn m4_local_delete_vs_remote_edit_is_not_trashed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let (engine, docs, docs_link) = listed_lazy(&backend, dir.path()).await;
    let a = remote_child(&backend, &docs_link, "a.txt").await;
    let a_ino = found(hot(|| engine.lookup(docs, b"a.txt")).expect("lookup")).ino;
    engine.unlink(docs, b"a.txt").expect("unlink");
    remote_revision(&backend, &a.link_id, b"edited elsewhere").await;

    start_journal(&engine);
    settle(&engine).await;
    assert!(!is_trashed(&backend, &a.link_id).await);
    assert_eq!(
        fake_tree(&backend).await["/docs/a.txt"].content,
        b"edited elsewhere"
    );
    let back = found(hot(|| engine.lookup(docs, b"a.txt")).expect("lookup"));
    assert_ne!(back.ino, a_ino);
    assert_eq!(read_file(&engine, back.ino).await, b"edited elsewhere");
    let all = engine.conflicts();
    assert_eq!(kinds(&engine), [ConflictKind::LocalDeleteRemoteEdit]);
    assert_eq!((all[0].ino, all[0].other_ino), (a_ino, Some(back.ino)));
    assert_eq!(all[0].state, ConflictState::Open);
    engine.tree.read().check_invariants().expect("consistent");
}

#[tokio::test]
async fn m4_trash_window_is_recoverable() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let a_backend = backend.with_client(client("pdmount-a"));
    let (engine, docs, docs_link) = listed_lazy(&a_backend, dir.path()).await;
    let a = remote_child(&backend, &docs_link, "a.txt").await;
    engine.unlink(docs, b"a.txt").expect("unlink");

    // The check passed; B writes before the trash commits (§10.5: the
    // server has no precondition on trash).
    let pause = a_backend.pause_at(FakePoint::TrashBeforeCommit);
    start_journal(&engine);
    pause.reached().await;
    remote_revision(&backend, &a.link_id, b"written while trashing").await;
    pause.release();
    settle(&engine).await;
    assert!(is_trashed(&backend, &a.link_id).await);
    assert!(
        backend
            .all_revision_sha1s()
            .contains(&sha1_of(b"written while trashing")),
        "B's revision stays reachable in the trash"
    );
    assert!(engine.conflicts().is_empty());
}

#[tokio::test]
async fn m5_rename_rename_caught_by_the_pre_check() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let (engine, docs, docs_link) = listed_lazy(&backend, dir.path()).await;
    let a = remote_child(&backend, &docs_link, "a.txt").await;
    let a_ino = found(hot(|| engine.lookup(docs, b"a.txt")).expect("lookup")).ino;
    engine
        .rename(docs, b"a.txt", docs, b"mine.txt", RenameMode::Replace)
        .expect("rename");
    backend
        .rename(&a.link_id, None, "theirs.txt", None)
        .await
        .expect("remote rename");

    start_journal(&engine);
    settle(&engine).await;
    // The remote committed first and keeps the name.
    assert_eq!(
        found(hot(|| engine.lookup(docs, b"theirs.txt")).expect("lookup")).ino,
        a_ino
    );
    assert_eq!(
        hot(|| engine.lookup(docs, b"mine.txt")).expect("lookup"),
        Lookup::Negative
    );
    assert_eq!(kinds(&engine), [ConflictKind::RenameRename]);
    let tree = fake_tree(&backend).await;
    assert!(tree.contains_key("/docs/theirs.txt"), "{tree:?}");
    assert!(!tree.contains_key("/docs/mine.txt"), "{tree:?}");
    engine.tree.read().check_invariants().expect("consistent");
}

#[tokio::test]
async fn m5_rename_rename_caught_by_the_server_hash() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let a_backend = backend.with_client(client("pdmount-a"));
    let (engine, docs, docs_link) = listed_lazy(&a_backend, dir.path()).await;
    let a = remote_child(&backend, &docs_link, "a.txt").await;
    let a_ino = found(hot(|| engine.lookup(docs, b"a.txt")).expect("lookup")).ino;
    engine
        .rename(docs, b"a.txt", docs, b"mine.txt", RenameMode::Replace)
        .expect("rename");

    // The pre-check passed; B renames before A's rename commits.
    let pause = a_backend.pause_at(FakePoint::RenameBeforeCommit);
    start_journal(&engine);
    pause.reached().await;
    backend
        .rename(&a.link_id, None, "theirs.txt", None)
        .await
        .expect("remote rename");
    pause.release();
    settle(&engine).await;
    assert_eq!(
        found(hot(|| engine.lookup(docs, b"theirs.txt")).expect("lookup")).ino,
        a_ino
    );
    assert_eq!(kinds(&engine), [ConflictKind::RenameRename]);
    let tree = fake_tree(&backend).await;
    assert!(tree.contains_key("/docs/theirs.txt"), "{tree:?}");
    assert!(!tree.contains_key("/docs/mine.txt"), "{tree:?}");
}

#[tokio::test]
async fn rename_race_without_hash_enforcement_is_last_writer_wins() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    backend.set_name_hash_enforced(false);
    let a_backend = backend.with_client(client("pdmount-a"));
    let (engine, docs, docs_link) = listed_lazy(&a_backend, dir.path()).await;
    let a = remote_child(&backend, &docs_link, "a.txt").await;
    engine
        .rename(docs, b"a.txt", docs, b"mine.txt", RenameMode::Replace)
        .expect("rename");

    let pause = a_backend.pause_at(FakePoint::RenameBeforeCommit);
    start_journal(&engine);
    pause.reached().await;
    backend
        .rename(&a.link_id, None, "theirs.txt", None)
        .await
        .expect("remote rename");
    pause.release();
    settle(&engine).await;
    // Documents the case in which the server does not enforce the hash
    // (§17): A's rename lands after B's.
    let tree = fake_tree(&backend).await;
    assert!(tree.contains_key("/docs/mine.txt"), "{tree:?}");
    assert!(!tree.contains_key("/docs/theirs.txt"), "{tree:?}");
    assert!(engine.conflicts().is_empty());
}

#[tokio::test]
async fn move_cycles_keep_the_server_placement() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let root = backend.root().await.expect("root").link_id;
    let docs_link = remote_child(&backend, &root, "docs").await.link_id;
    let x_link = backend
        .create_folder(&docs_link, "x", UNIX_EPOCH)
        .await
        .expect("x");
    let y_link = backend
        .create_folder(&docs_link, "y", UNIX_EPOCH)
        .await
        .expect("y");
    let (engine, docs, _) = listed_lazy(&backend, dir.path()).await;
    let x = found(hot(|| engine.lookup(docs, b"x")).expect("lookup")).ino;
    let y = found(hot(|| engine.lookup(docs, b"y")).expect("lookup")).ino;
    engine.ensure_listed(y).await.expect("list y");

    // A moves x into y; B moves y into x.
    engine
        .rename(docs, b"x", y, b"x", RenameMode::Replace)
        .expect("move");
    backend
        .rename(&y_link, Some(&x_link), "y", None)
        .await
        .expect("remote move");

    start_journal(&engine);
    settle(&engine).await;
    assert_eq!(
        found(hot(|| engine.lookup(docs, b"x")).expect("lookup")).ino,
        x,
        "x is back where the server has it"
    );
    assert_eq!(kinds(&engine), [ConflictKind::MoveCycle]);
    let tree = fake_tree(&backend).await;
    assert!(
        backend
            .list_children(&x_link)
            .await
            .expect("list")
            .iter()
            .any(|node| node.name == "y"),
        "{tree:?}"
    );
    engine.tree.read().check_invariants().expect("consistent");
}

#[tokio::test]
async fn a_local_folder_delete_keeps_what_is_new_in_a_nested_folder() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let root = backend.root().await.expect("root").link_id;
    let docs_link = remote_child(&backend, &root, "docs").await.link_id;
    let sub_link = backend
        .create_folder(&docs_link, "sub", UNIX_EPOCH)
        .await
        .expect("sub");
    let inner_link = backend
        .create_folder(&sub_link, "inner", UNIX_EPOCH)
        .await
        .expect("inner");
    backend
        .upload_new(
            &inner_link,
            "old.txt",
            UploadSource::Bytes(Bytes::from_static(b"old")),
            meta(),
        )
        .await
        .expect("old");
    let (engine, docs, _) = listed_lazy(&backend, dir.path()).await;
    let sub = found(hot(|| engine.lookup(docs, b"sub")).expect("lookup")).ino;
    engine.ensure_listed(sub).await.expect("list sub");
    let inner = found(hot(|| engine.lookup(sub, b"inner")).expect("lookup")).ino;
    engine.ensure_listed(inner).await.expect("list inner");
    engine.unlink(inner, b"old.txt").expect("unlink");
    engine.rmdir(sub, b"inner").expect("rmdir inner");
    engine.rmdir(docs, b"sub").expect("rmdir sub");
    // B adds a file inside the nested folder.
    backend
        .upload_new(
            &inner_link,
            "new.txt",
            UploadSource::Bytes(Bytes::from_static(b"new")),
            meta(),
        )
        .await
        .expect("new");

    start_journal(&engine);
    settle(&engine).await;
    assert!(!is_trashed(&backend, &sub_link).await);
    assert!(!is_trashed(&backend, &inner_link).await);
    let tree = fake_tree(&backend).await;
    assert_eq!(tree["/docs/sub/inner/new.txt"].content, b"new");
    assert!(!tree.contains_key("/docs/sub/inner/old.txt"), "{tree:?}");
    assert_eq!(
        kinds(&engine),
        [
            ConflictKind::LocalDeleteRemoteChildren,
            ConflictKind::LocalDeleteRemoteChildren
        ]
    );
    engine.tree.read().check_invariants().expect("consistent");
}

#[tokio::test]
async fn a_local_folder_delete_keeps_remote_children_it_did_not_know() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let root = backend.root().await.expect("root").link_id;
    let docs_link = remote_child(&backend, &root, "docs").await.link_id;
    let sub_link = backend
        .create_folder(&docs_link, "sub", UNIX_EPOCH)
        .await
        .expect("sub");
    backend
        .upload_new(
            &sub_link,
            "old.txt",
            UploadSource::Bytes(Bytes::from_static(b"old")),
            meta(),
        )
        .await
        .expect("old");
    let (engine, docs, _) = listed_lazy(&backend, dir.path()).await;
    let sub = found(hot(|| engine.lookup(docs, b"sub")).expect("lookup")).ino;
    engine.ensure_listed(sub).await.expect("list sub");
    engine.unlink(sub, b"old.txt").expect("unlink");
    engine.rmdir(docs, b"sub").expect("rmdir");
    // B adds a file the engine has not heard of.
    backend
        .upload_new(
            &sub_link,
            "new.txt",
            UploadSource::Bytes(Bytes::from_static(b"new")),
            meta(),
        )
        .await
        .expect("new");

    start_journal(&engine);
    settle(&engine).await;
    assert!(!is_trashed(&backend, &sub_link).await);
    let tree = fake_tree(&backend).await;
    assert_eq!(tree["/docs/sub/new.txt"].content, b"new");
    assert!(!tree.contains_key("/docs/sub/old.txt"), "{tree:?}");
    let all = engine.conflicts();
    assert_eq!(kinds(&engine), [ConflictKind::LocalDeleteRemoteChildren]);
    assert_eq!(all[0].detail.as_deref(), Some("1"));
    let back = found(hot(|| engine.lookup(docs, b"sub")).expect("lookup"));
    assert_eq!(all[0].other_ino, Some(back.ino));
    engine.tree.read().check_invariants().expect("consistent");
}

#[tokio::test]
async fn a_folder_waits_for_children_moved_out() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let root = backend.root().await.expect("root").link_id;
    let docs_link = remote_child(&backend, &root, "docs").await.link_id;
    let sub_link = backend
        .create_folder(&docs_link, "sub", UNIX_EPOCH)
        .await
        .expect("sub");
    backend
        .upload_new(
            &sub_link,
            "old.txt",
            UploadSource::Bytes(Bytes::from_static(b"old")),
            meta(),
        )
        .await
        .expect("old");
    let (engine, docs, _) = listed_lazy(&backend, dir.path()).await;
    let sub = found(hot(|| engine.lookup(docs, b"sub")).expect("lookup")).ino;
    engine.ensure_listed(sub).await.expect("list sub");
    engine
        .rename(sub, b"old.txt", docs, b"moved.txt", RenameMode::Replace)
        .expect("move out");
    engine.rmdir(docs, b"sub").expect("rmdir");
    // The move fails once; the folder must not be trashed around the file.
    backend.inject_failure(
        pdmount_backend::fake::FakeOp::Rename,
        pdmount_backend::BackendError::Transient("try again".into()),
    );

    start_journal(&engine);
    settle(&engine).await;
    let tree = fake_tree(&backend).await;
    assert_eq!(tree["/docs/moved.txt"].content, b"old");
    assert!(is_trashed(&backend, &sub_link).await);
    assert!(engine.conflicts().is_empty(), "{:?}", engine.conflicts());
}

/// A create/create conflict, uploaded: `/docs/new.txt` holds B's content, the
/// local copy A's under its conflict name. Returns the engine, the local
/// copy's inode and the server file's inode.
async fn settled_create_create(
    backend: &FakeBackend,
    dir: &std::path::Path,
) -> (Arc<Engine>, Ino, Ino, Ino) {
    let (engine, docs, docs_link) = listed_lazy(backend, dir).await;
    let own = write_file(&engine, docs, "new.txt", b"written on A");
    remote_file(backend, &docs_link, "new.txt", b"written on B").await;
    engine.start_background();
    settle(&engine).await;
    assert_eq!(kinds(&engine), [ConflictKind::CreateCreate]);
    let server = found(hot(|| engine.lookup(docs, b"new.txt")).expect("lookup")).ino;
    (engine, docs, own, server)
}

#[tokio::test]
async fn resolve_keep_local_replaces_the_server_file_with_the_local_copy() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let (engine, docs, own, _server) = settled_create_create(&backend, dir.path()).await;

    let view = engine
        .conflict_resolve("1", Resolution::KeepLocal)
        .await
        .expect("resolve");
    assert_eq!(view.state, ConflictState::Resolved);
    settle(&engine).await;

    let tree = fake_tree(&backend).await;
    assert_eq!(tree["/docs/new.txt"].content, b"written on A");
    assert!(!tree.contains_key(&format!("/docs/{}", conflict_name("new", ".txt"))));
    let mine = found(hot(|| engine.lookup(docs, b"new.txt")).expect("lookup"));
    assert_eq!(mine.ino, own, "the local copy keeps its inode");
    assert_eq!(read_file(&engine, own).await, b"written on A");
    assert_eq!(engine.conflict_count(), 0);
    engine.tree.read().check_invariants().expect("consistent");
}

#[tokio::test]
async fn resolve_keep_remote_waits_for_the_upload_then_trashes_the_copy() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let (engine, docs, own, server) = settled_create_create(&backend, dir.path()).await;
    let copy_link = remote_of(&engine, own).expect("uploaded").link;
    // Another change to the copy that has not been uploaded yet.
    overwrite(&engine, own, b"edited once more");

    let view = engine
        .conflict_resolve("/docs/new.txt", Resolution::KeepRemote)
        .await
        .expect("resolve");
    assert_eq!(view.state, ConflictState::Resolved);
    settle(&engine).await;

    let tree = fake_tree(&backend).await;
    assert_eq!(tree["/docs/new.txt"].content, b"written on B");
    assert!(!tree.contains_key(&format!("/docs/{}", conflict_name("new", ".txt"))));
    assert!(is_trashed(&backend, &copy_link).await);
    // The content in the trash is the last edit: nothing was lost.
    let revisions = backend.all_revision_sha1s();
    assert!(revisions.contains(&sha1_of(b"edited once more")));
    let kept = found(hot(|| engine.lookup(docs, b"new.txt")).expect("lookup"));
    assert_eq!(kept.ino, server);
    engine.tree.read().check_invariants().expect("consistent");
}

#[tokio::test]
async fn resolve_refuses_choices_that_do_not_apply() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let (engine, _docs, own, _server) = settled_create_create(&backend, dir.path()).await;
    let cycle = engine.record_conflict(ConflictNote::new(
        ConflictKind::MoveCycle,
        own,
        "new.txt".into(),
    ));
    for choice in [Resolution::KeepLocal, Resolution::KeepRemote] {
        let error = engine
            .conflict_resolve(&cycle.to_string(), choice)
            .await
            .expect_err("does not apply");
        assert!(
            error.to_string().contains("not applicable to move_cycle"),
            "{error}"
        );
    }
    let error = engine
        .conflict_resolve("99", Resolution::KeepBoth)
        .await
        .expect_err("unknown id");
    assert!(error.to_string().contains("no conflict 99"), "{error}");
    let error = engine
        .conflict_resolve("/docs/nothing.txt", Resolution::KeepBoth)
        .await
        .expect_err("no such path");
    assert!(matches!(error, CoreError::NotFound), "{error}");

    engine
        .conflict_resolve(&cycle.to_string(), Resolution::KeepBoth)
        .await
        .expect("dismissed");
    let error = engine
        .conflict_resolve(&cycle.to_string(), Resolution::KeepBoth)
        .await
        .expect_err("resolved already");
    assert!(error.to_string().contains("resolved already"), "{error}");
    // Nothing was changed by the refusals.
    assert_eq!(engine.conflict_open_count(), 1);
}

#[tokio::test]
async fn resolve_keep_local_reports_a_missing_copy() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let (engine, docs, _own, _server) = settled_create_create(&backend, dir.path()).await;
    engine
        .unlink(docs, b"new.txt")
        .expect("remove the server file");
    let error = engine
        .conflict_resolve("1", Resolution::KeepLocal)
        .await
        .expect_err("gone");
    assert!(
        error.to_string().contains("other copy no longer exists"),
        "{error}"
    );
    assert_eq!(engine.conflict_open_count(), 1, "still open");
}

#[tokio::test]
async fn conflicts_list_and_show_find_both_copies() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let (engine, _docs, _own, _server) = settled_create_create(&backend, dir.path()).await;
    let copy = format!("/docs/{}", conflict_name("new", ".txt"));

    for target in ["1", "/docs/new.txt", copy.as_str()] {
        let view = engine.conflict_show(target).await.expect("show");
        assert_eq!(view.id, 1, "{target}");
        assert_eq!(view.kind, ConflictKind::CreateCreate);
        assert_eq!(view.path.as_deref(), Some(copy.as_str()));
        assert_eq!(view.other_path.as_deref(), Some("/docs/new.txt"));
        assert_eq!(view.local_device, "dev-a");
    }
    assert_eq!(
        engine.conflicts_list(None, false).await.expect("all").len(),
        1
    );
    assert_eq!(
        engine
            .conflicts_list(Some("/docs"), false)
            .await
            .expect("docs")
            .len(),
        1
    );
    assert!(
        engine
            .conflicts_list(Some("/docs/new.txt"), false)
            .await
            .expect("file")
            .len()
            == 1
    );

    engine
        .conflict_resolve("1", Resolution::KeepBoth)
        .await
        .expect("dismiss");
    assert!(
        engine
            .conflicts_list(None, false)
            .await
            .expect("open")
            .is_empty()
    );
    assert_eq!(
        engine.conflicts_list(None, true).await.expect("all").len(),
        1
    );
    let error = engine
        .conflict_show("/docs/new.txt")
        .await
        .expect_err("none");
    assert!(
        error.to_string().contains("no unresolved conflict"),
        "{error}"
    );
}

#[tokio::test]
async fn xattrs_report_state_and_conflict() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let (engine, docs, docs_link) = listed_lazy(&backend, dir.path()).await;
    let state = |ino| engine.xattr(ino, "user.pdmount.state").expect("state");

    // Not started yet: the new file waits for its upload.
    let own = write_file(&engine, docs, "new.txt", b"written on A");
    assert_eq!(state(own).as_deref(), Some("pending"));
    assert_eq!(
        engine.xattr(own, "user.pdmount.link_id").expect("link"),
        None
    );
    assert_eq!(
        engine
            .xattr(own, "user.pdmount.access")
            .expect("access")
            .as_deref(),
        Some("rw")
    );
    assert_eq!(engine.xattr(own, "user.pdmount.other").expect("name"), None);
    assert!(matches!(
        engine.xattr(9_999_999, "user.pdmount.state"),
        Err(CoreError::NotFound)
    ));
    let ignored = write_file(&engine, ROOT_INO, "scratch.tmp", b"x");
    engine
        .tree
        .write()
        .get_mut(ignored)
        .expect("node")
        .local_only = true;
    assert_eq!(state(ignored).as_deref(), Some("local-only"));

    remote_file(&backend, &docs_link, "new.txt", b"written on B").await;
    engine.start_background();
    settle(&engine).await;
    let server = found(hot(|| engine.lookup(docs, b"new.txt")).expect("lookup")).ino;
    let copy = format!("/docs/{}", conflict_name("new", ".txt"));

    assert_eq!(state(own).as_deref(), Some("conflict"));
    assert_eq!(state(server).as_deref(), Some("conflict"));
    assert_eq!(
        engine
            .xattr(own, "user.pdmount.conflict")
            .expect("conflict")
            .as_deref(),
        Some("1 create_create /docs/new.txt")
    );
    assert_eq!(
        engine
            .xattr(server, "user.pdmount.conflict")
            .expect("conflict"),
        Some(format!("1 create_create {copy}"))
    );
    assert!(
        engine
            .xattr(own, "user.pdmount.link_id")
            .expect("link")
            .is_some()
    );
    assert!(
        engine
            .xattr(own, "user.pdmount.rev")
            .expect("rev")
            .is_some()
    );
    let names = engine.xattr_names(own).expect("names");
    assert_eq!(
        names,
        [
            "user.pdmount.state",
            "user.pdmount.link_id",
            "user.pdmount.rev",
            "user.pdmount.conflict",
            "user.pdmount.access"
        ]
    );
    assert_eq!(
        engine.xattr_names(ROOT_INO).expect("root"),
        [
            "user.pdmount.state",
            "user.pdmount.link_id",
            "user.pdmount.access"
        ]
    );

    engine
        .conflict_resolve("1", Resolution::KeepBoth)
        .await
        .expect("dismiss");
    assert_eq!(state(own).as_deref(), Some("clean"));
    assert_eq!(
        engine.xattr(own, "user.pdmount.conflict").expect("none"),
        None
    );

    // A newer remote revision under dirty content, and a parked upload.
    engine
        .tree
        .write()
        .get_mut(server)
        .expect("node")
        .remote_newer = true;
    assert_eq!(state(server).as_deref(), Some("remote-newer"));
}

// ---- connectivity (§9.6, D041) ----

fn offline_rule() -> pdmount_backend::fake::FaultRule {
    pdmount_backend::fake::FaultRule {
        kind: pdmount_backend::fake::FaultKind::Offline,
        until_ms: None,
        client: None,
    }
}

/// A fake whose faults can be switched from the test, and a config that
/// probes every 50 ms.
async fn faulty(dir: &std::path::Path) -> (FakeBackend, Config) {
    let backend = seed(FakeBackend::open(&dir.join("fake")).expect("fake")).await;
    let mut config = config();
    config.sync.probe_interval = HumanDuration(Duration::from_millis(50));
    (backend, config)
}

#[tokio::test]
async fn offline_after_three_connection_failures_and_back_after_a_probe() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (backend, config) = faulty(dir.path()).await;
    let (engine, _inval) = open_with(&backend, dir.path(), config).await;
    engine.gate.set_interval(Duration::ZERO);
    let fake_dir = dir.path().join("fake");
    pdmount_backend::fake::add_fault(&fake_dir, offline_rule()).expect("fault");
    for _ in 0..2 {
        engine.backend.storage_info().await.unwrap_err();
    }
    assert_eq!(engine.health().state(), Connectivity::Online);
    engine.backend.storage_info().await.unwrap_err();
    assert_eq!(engine.health().state(), Connectivity::Offline);

    let task = Arc::clone(&engine);
    tokio::spawn(async move { task.run_health().await });
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        engine.health().state(),
        Connectivity::Offline,
        "a failed probe changes nothing"
    );
    pdmount_backend::fake::clear_faults(&fake_dir, None).expect("clear");
    eventually("the probe to find the server", || {
        engine.health().state() == Connectivity::Online
    })
    .await;
}

#[tokio::test]
async fn writes_are_accepted_and_uploaded_after_reconnect() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (backend, config) = faulty(dir.path()).await;
    let (engine, _inval) = open_with(&backend, dir.path(), config).await;
    engine.gate.set_interval(Duration::ZERO);
    engine.ensure_listed(ROOT_INO).await.expect("list");
    let fake_dir = dir.path().join("fake");
    pdmount_backend::fake::add_fault(&fake_dir, offline_rule()).expect("fault");
    engine.health().set(Connectivity::Offline);
    engine.start_background();

    let remote_changes = backend.changes().len();
    let ino = write_file(&engine, ROOT_INO, "made-offline.txt", b"written offline");
    let calls = engine.metrics().backend_calls.load(Ordering::Relaxed);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(engine.health().state(), Connectivity::Offline);
    assert_eq!(engine.queue_counts().pending, 1);
    assert!(engine.parked().is_empty());
    assert_eq!(
        backend.changes().len(),
        remote_changes,
        "nothing reaches the drive while offline"
    );
    assert!(
        engine.metrics().backend_calls.load(Ordering::Relaxed) - calls <= 6,
        "only probes ask the server while offline"
    );
    assert_eq!(read_file(&engine, ino).await, b"written offline");

    pdmount_backend::fake::clear_faults(&fake_dir, None).expect("clear");
    settle(&engine).await;
    assert_eq!(
        fake_tree(&backend).await["/made-offline.txt"].content,
        b"written offline"
    );
    assert_eq!(engine.health().state(), Connectivity::Online);
}

#[tokio::test]
async fn offline_uncached_reads_fail_within_offline_read() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (backend, config) = faulty(dir.path()).await;
    let (engine, _inval) = open_with(&backend, dir.path(), config).await;
    engine.ensure_listed(ROOT_INO).await.expect("list");
    let top = found(hot(|| engine.lookup(ROOT_INO, b"top.bin")).expect("lookup")).ino;
    pdmount_backend::fake::add_fault(&dir.path().join("fake"), offline_rule()).expect("fault");
    let (handle, _) = engine.open_file(top, READ).expect("open");
    let started = Instant::now();
    let error = engine.read(handle, 0, 10).await.expect_err("uncached");
    assert!(
        matches!(error, CoreError::Backend(BackendError::Offline(_))),
        "{error:?}"
    );
    assert!(
        started.elapsed() < engine.config().timeouts.offline_read.0,
        "the answer must not wait for a timeout"
    );
    // The listing of an unlisted directory fails the same way.
    let docs = found(hot(|| engine.lookup(ROOT_INO, b"docs")).expect("lookup")).ino;
    assert!(engine.ensure_listed(docs).await.is_err());
}

#[tokio::test]
async fn needs_login_short_circuits_network_calls() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (backend, config) = faulty(dir.path()).await;
    let (engine, _inval) = open_with(&backend, dir.path(), config).await;
    let fake_dir = dir.path().join("fake");
    pdmount_backend::fake::add_fault(
        &fake_dir,
        pdmount_backend::fake::FaultRule {
            kind: pdmount_backend::fake::FaultKind::Unauthorized,
            until_ms: None,
            client: None,
        },
    )
    .expect("fault");
    assert_eq!(
        engine.backend.storage_info().await.unwrap_err(),
        BackendError::Unauthorized
    );
    assert_eq!(engine.health().state(), Connectivity::NeedsLogin);

    // The server would answer now, but nothing is sent.
    pdmount_backend::fake::clear_faults(&fake_dir, None).expect("clear");
    assert_eq!(
        engine.backend.storage_info().await.unwrap_err(),
        BackendError::Unauthorized
    );
    assert_eq!(engine.health().state(), Connectivity::NeedsLogin);
    // A login ends it.
    engine.health().set(Connectivity::Online);
    engine.backend.storage_info().await.expect("online again");
}

/// A rejected session keeps the mount up (D042): local writes are accepted
/// and queued, and a login's kick ends the state once the server answers
/// again; then the queue drains.
#[tokio::test]
async fn needs_login_accepts_writes_and_a_kick_after_the_login_drains_them() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (backend, config) = faulty(dir.path()).await;
    let (engine, _inval) = open_with(&backend, dir.path(), config).await;
    engine.gate.set_interval(Duration::ZERO);
    engine.ensure_listed(ROOT_INO).await.expect("list");
    let fake_dir = dir.path().join("fake");
    pdmount_backend::fake::add_fault(
        &fake_dir,
        pdmount_backend::fake::FaultRule {
            kind: pdmount_backend::fake::FaultKind::Unauthorized,
            until_ms: None,
            client: None,
        },
    )
    .expect("fault");
    engine.health().set(Connectivity::NeedsLogin);
    start_journal(&engine);
    let task = Arc::clone(&engine);
    tokio::spawn(async move { task.run_health().await });

    write_file(&engine, ROOT_INO, "queued.txt", b"queued");
    assert_eq!(engine.queue_counts().pending, 1);
    assert_eq!(engine.queue_counts().parked, 0);

    // The new session is not accepted yet: the probe is rejected again.
    engine.health().kick();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(engine.health().state(), Connectivity::NeedsLogin);
    assert_eq!(engine.queue_counts().pending, 1);

    pdmount_backend::fake::clear_faults(&fake_dir, None).expect("clear");
    engine.health().kick();
    eventually("the kick to end needs_login", || {
        engine.health().state() == Connectivity::Online
    })
    .await;
    settle(&engine).await;
    assert!(fake_tree(&backend).await.contains_key("/queued.txt"));
}

#[tokio::test]
async fn journal_does_not_park_while_offline() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (backend, config) = faulty(dir.path()).await;
    let (engine, _inval) = open_with(&backend, dir.path(), config).await;
    engine.gate.set_interval(Duration::ZERO);
    engine.ensure_listed(ROOT_INO).await.expect("list");
    let fake_dir = dir.path().join("fake");
    pdmount_backend::fake::add_fault(&fake_dir, offline_rule()).expect("fault");
    // The journal alone: its own failures take the engine offline.
    start_journal(&engine);
    write_file(&engine, ROOT_INO, "queued.txt", b"queued");
    eventually("the journal's failures to take the engine offline", || {
        engine.health().state() == Connectivity::Offline
    })
    .await;
    assert_eq!(engine.queue_counts().parked, 0);
    assert_eq!(engine.queue_counts().pending, 1);

    pdmount_backend::fake::clear_faults(&fake_dir, None).expect("clear");
    let task = Arc::clone(&engine);
    tokio::spawn(async move { task.run_health().await });
    settle(&engine).await;
    assert!(fake_tree(&backend).await.contains_key("/queued.txt"));
}
