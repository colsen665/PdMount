use super::*;
use crate::access::RuleSource;
use crate::config::AccessMode;
use crate::fake_fs::{fake_tree, read_all};
use crate::metrics::hot;
use crate::units::HumanDuration;
use pdmount_backend::fake::{FakeBackend, FakeChange};
use pdmount_backend::{FileMeta, UploadSource};
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
    let backend = FakeBackend::new();
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
    Engine::open(
        Arc::new(backend.clone()),
        EngineOptions {
            state_db: dir.join("state.db"),
            staging_dir: dir.join("staging"),
            local_ignore: None,
            cache: CacheOptions {
                dir: dir.join("blocks"),
                master_key: [1; 32],
                ram_bytes: 1 << 20,
                disk_bytes: 1 << 20,
            },
            config,
        },
    )
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
    data
}

#[tokio::test]
async fn listing_lookup_and_read_through_the_cache() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let (engine, _inval) = open(&backend, dir.path()).await;
    assert_eq!(
        hot(|| engine.lookup(ROOT_INO, b"docs")).expect("lookup"),
        Lookup::NeedsListing
    );
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
    assert_eq!(
        hot(|| engine.read_local(handle, 0, 100)).expect("local"),
        None
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
        EngineOptions {
            state_db: dir.path().join("state.db"),
            staging_dir: dir.path().join("staging"),
            local_ignore: None,
            cache: CacheOptions {
                dir: dir.path().join("blocks"),
                master_key: [1; 32],
                ram_bytes: 1 << 20,
                disk_bytes: 1 << 20,
            },
            config: Config::default(),
        },
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
    engine.opendir(ROOT_INO).expect("opendir").expect("listed");
    tokio::time::sleep(Duration::from_millis(1200)).await;
    // Turns start at 0, 0.5 and 1.0 s; each prefetches one file with two
    // calls (block list, block). Unpaced, all 20 files would be done.
    let calls = engine.metrics().backend_calls.load(Ordering::Relaxed) - before;
    assert!((2..=6).contains(&calls), "{calls} backend calls in 1.2 s");
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
    for _ in 0..50 {
        if engine.metrics().prefetched_blocks.load(Ordering::Relaxed) > 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("no readahead happened");
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
        for _ in 0..100 {
            if engine.is_ignored(ROOT_INO, "x.log", false) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
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
    let deadline = Instant::now() + Duration::from_secs(20);
    while engine.events_refreshing() {
        assert!(Instant::now() < deadline, "the refresh did not finish");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn stored_cursor(engine: &Engine) -> Option<EventId> {
    engine.store.flush().await.expect("flush");
    crate::events::load_cursor(&engine.store).expect("cursor")
}

#[tokio::test]
async fn events_flow_while_a_refresh_lists_again() {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = seeded().await;
    let (engine, _inval, docs) = listed(&backend, dir.path()).await;
    let root = backend.root().await.expect("root").link_id;
    let docs_link = remote_child(&backend, &root, "docs").await.link_id;
    let old = stored_cursor(&engine).await;
    backend.set_latency(Duration::from_millis(300));
    backend.request_refresh();
    engine.sync_events().await.expect("events");
    let stored = stored_cursor(&engine).await;
    assert!(engine.events_refreshing(), "listings take a while");
    assert_eq!(stored, old, "no cursor is stored mid-refresh");
    backend
        .upload_new(
            &docs_link,
            "during.txt",
            UploadSource::Bytes(Bytes::from_static(b"d")),
            meta(),
        )
        .await
        .expect("upload");
    engine.sync_events().await.expect("events");
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
